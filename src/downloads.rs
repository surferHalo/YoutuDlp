use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{error, warn};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

use crate::app::{ActiveDownload, AppState};
use crate::cookies::{apply_cookie_arguments, explain_cookie_error};
use crate::events::{self, ServerEvent};
use crate::tasks::{RuntimeState, TaskKind, TaskProgress, TaskRecord, TaskState, TaskView, output_directory};

pub async fn schedule_pending(state: Arc<AppState>) -> Result<()> {
    if state.scheduler_running.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    let result = async {
        loop {
            let max_concurrent = state.settings.read().await.max_concurrent_downloads as usize;
            let active_ids = current_active_ids(&state);
            if active_ids.len() >= max_concurrent {
                break;
            }

            let maybe_task = state
                .task_store
                .find_first_queued_task_excluding(&active_ids)?;

            let Some(task) = maybe_task else {
                break;
            };

            start_download(state.clone(), task).await?;
        }

        Ok(())
    }
    .await;

    state.scheduler_running.store(false, Ordering::SeqCst);
    result
}

pub async fn stop_task(state: Arc<AppState>, task_id: &str) -> Result<TaskRecord> {
    let task = state.task_store.load_task(task_id)?;
    if matches!(task.task_kind, TaskKind::PlaylistParent) {
        let mut affected = 0;
        for child_id in task.child_task_ids.clone() {
            let child = state.task_store.load_task(&child_id)?;
            match child.state {
                TaskState::Downloading => {
                    stop_single_task(state.clone(), &child_id).await?;
                    affected += 1;
                }
                TaskState::Queued => {
                    // Pull queued children out of the queue so the scheduler
                    // won't start them; mark them Paused to match stop semantics.
                    let mut c = child;
                    c.state = TaskState::Paused;
                    c.update_timestamp();
                    state.task_store.save_task(&c)?;
                    state.task_store.remove_from_queue(&[c.id.clone()])?;
                    let view = build_task_view(&state, &c).await;
                    events::publish(&state.event_tx, ServerEvent::task_updated(&view));
                    affected += 1;
                }
                _ => {}
            }
        }

        if affected == 0 {
            bail!("playlist has no running or queued child tasks to stop");
        }

        publish_refreshed_playlist_parents(&state)?;
        let refreshed = state.task_store.load_task(task_id)?;
        return Ok(refreshed);
    }

    stop_single_task(state, task_id).await
}

pub async fn resume_task(state: Arc<AppState>, task_id: &str) -> Result<TaskRecord> {
    let task = state.task_store.load_task(task_id)?;
    if matches!(task.task_kind, TaskKind::PlaylistParent) {
        let mut affected = 0;
        for child_id in task.child_task_ids.clone() {
            let child = state.task_store.load_task(&child_id)?;
            if matches!(child.state, TaskState::Paused | TaskState::Failed) {
                let _ = resume_single_task(state.clone(), &child_id).await?;
                affected += 1;
            }
        }

        if affected == 0 {
            bail!("playlist has no paused or failed child tasks to resume");
        }

        let refreshed = state.task_store.load_task(task_id)?;
        return Ok(refreshed);
    }

    resume_single_task(state, task_id).await
}

pub async fn restart_task(state: Arc<AppState>, task_id: &str) -> Result<TaskRecord> {
    let task = state.task_store.load_task(task_id)?;
    if matches!(task.task_kind, TaskKind::PlaylistParent) {
        let mut affected = 0;
        for child_id in task.child_task_ids.clone() {
            let child = state.task_store.load_task(&child_id)?;
            if !matches!(child.state, TaskState::Downloading | TaskState::Stopping) {
                let _ = restart_single_task(state.clone(), &child_id).await?;
                affected += 1;
            }
        }

        if affected == 0 {
            bail!("playlist has no child tasks eligible for restart");
        }

        let refreshed = state.task_store.load_task(task_id)?;
        return Ok(refreshed);
    }

    restart_single_task(state, task_id).await
}

async fn stop_single_task(state: Arc<AppState>, task_id: &str) -> Result<TaskRecord> {
    let mut task = state.task_store.load_task(task_id)?;
    if !matches!(task.state, TaskState::Downloading) {
        bail!("task is not currently downloading");
    }

    task.state = TaskState::Stopping;
    task.update_timestamp();
    state.task_store.save_task(&task)?;
    let view = build_task_view(&state, &task).await;
    events::publish(&state.event_tx, ServerEvent::task_updated(&view));
    publish_refreshed_playlist_parents(&state)?;

    let maybe_stop_tx = {
        let active = state
            .active_downloads
            .lock()
            .expect("active download lock poisoned");
        active.get(task_id).map(|download| download.stop_tx.clone())
    };

    if let Some(stop_tx) = maybe_stop_tx {
        stop_tx
            .send(())
            .map_err(|_| anyhow::anyhow!("failed to signal running download"))?;
    }

    Ok(task)
}

async fn resume_single_task(state: Arc<AppState>, task_id: &str) -> Result<TaskRecord> {
    let mut task = state.task_store.load_task(task_id)?;
    if !matches!(task.state, TaskState::Paused | TaskState::Failed) {
        bail!("task can only be resumed from paused or failed state");
    }

    task.state = TaskState::Queued;
    task.update_timestamp();
    state.task_store.save_task(&task)?;
    let view = build_task_view(&state, &task).await;
    events::publish(&state.event_tx, ServerEvent::task_updated(&view));
    publish_refreshed_playlist_parents(&state)?;
    state.task_store.append_to_queue(&task.id)?;
    schedule_pending(state).await?;

    Ok(task)
}

async fn restart_single_task(state: Arc<AppState>, task_id: &str) -> Result<TaskRecord> {
    let mut task = state.task_store.load_task(task_id)?;
    if matches!(task.state, TaskState::Downloading | TaskState::Stopping) {
        bail!("stop the task before restarting it");
    }

    let settings = state.settings.read().await.clone();
    delete_related_partial_files(&settings.download_root, &task).await;

    // Reset runtime state.
    state.runtime_states.write().await.remove(task_id);

    task.state = TaskState::Queued;
    task.last_error = None;
    task.output_files.clear();
    task.update_timestamp();
    state.task_store.save_task(&task)?;
    let view = build_task_view(&state, &task).await;
    events::publish(&state.event_tx, ServerEvent::task_updated(&view));
    publish_refreshed_playlist_parents(&state)?;
    state.task_store.append_to_queue(&task.id)?;
    schedule_pending(state).await?;

    Ok(task)
}

async fn start_download(state: Arc<AppState>, mut task: TaskRecord) -> Result<()> {
    let settings = state.settings.read().await.clone();
    let tools = state.tools.read().await.clone();
    let yt_dlp_path = tools
        .yt_dlp
        .path
        .clone()
        .ok_or_else(|| anyhow::anyhow!("yt-dlp is not configured"))?;

    let output_dir = output_directory(&settings, &task.target_subdir);
    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create output directory {}", output_dir.display()))?;

    task.state = TaskState::Downloading;
    task.last_error = None;
    task.update_timestamp();
    state.task_store.save_task(&task)?;
    // Initialise RuntimeState for the task.
    {
        let mut rt = state.runtime_states.write().await;
        rt.entry(task.id.clone()).or_insert_with(|| RuntimeState {
            progress: TaskProgress::default(),
            ..Default::default()
        });
    }
    let view = build_task_view(&state, &task).await;
    events::publish(&state.event_tx, ServerEvent::task_updated(&view));
    publish_refreshed_playlist_parents(&state)?;

    let mut command = build_command(
        &yt_dlp_path,
        &state.paths.runtime_dir,
        &task,
        &settings.download_root,
        &output_dir,
    );
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            task.state = TaskState::Failed;
            task.last_error = Some(error.to_string());
            task.update_timestamp();
            state.task_store.save_task(&task)?;
            let view = build_task_view(&state, &task).await;
            events::publish(&state.event_tx, ServerEvent::task_updated(&view));
            publish_refreshed_playlist_parents(&state)?;
            return Ok(());
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (stop_tx, stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

    {
        let mut active = state
            .active_downloads
            .lock()
            .expect("active download lock poisoned");
        active.insert(task.id.clone(), ActiveDownload { stop_tx, process_done: done_rx });
    }

    if let Some(stdout) = stdout {
        tokio::spawn(stream_output_lines(
            state.clone(),
            task.id.clone(),
            BufReader::new(stdout),
        ));
    }

    if let Some(stderr) = stderr {
        tokio::spawn(stream_output_lines(
            state.clone(),
            task.id.clone(),
            BufReader::new(stderr),
        ));
    }

    spawn_waiter(state, task.id.clone(), child, stop_rx, done_tx);

    Ok(())
}

fn spawn_waiter(
    state: Arc<AppState>,
    task_id: String,
    mut child: tokio::process::Child,
    mut stop_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    done_tx: tokio::sync::oneshot::Sender<()>,
) {
    let pid = child.id();
    tokio::spawn(async move {
        let wait_result = tokio::select! {
            result = child.wait() => result,
            _ = stop_rx.recv() => {
                kill_process_tree(pid).await;

                match tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await {
                    Ok(result) => result,
                    Err(_) => {
                        let _ = child.start_kill();
                        child.wait().await
                    }
                }
            }
        };

        // Process has fully exited — OS has released all file handles.
        // Signal the delete handler (if waiting) that it is safe to delete files.
        let _ = done_tx.send(());

        if let Err(error) = finalize_download(state.clone(), &task_id, wait_result).await {
            error!("failed to finalize task {}: {error}", task_id);
        }
    });
}

/// Kill the entire process tree rooted at `pid` using an async process.
/// On Windows uses `taskkill /F /T`; on other platforms SIGKILL via start_kill
/// is sufficient for the direct child, so this is a no-op.
pub async fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(windows)]
    {
        match tokio::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .kill_on_drop(true)
            .output()
            .await
        {
            Ok(output) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!("taskkill /F /T /PID {pid} failed: {stderr}");
                }
            }
            Err(e) => warn!("failed to run taskkill for PID {pid}: {e}"),
        }
    }
    #[cfg(unix)]
    {
        let pgid = pid as i32;
        let rc = unsafe { libc::kill(-pgid, libc::SIGKILL) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                warn!("failed to kill process group {pgid}: {error}");
            }
        }
    }
    #[cfg(not(any(windows, unix)))]
    let _ = pid;
}

#[cfg(windows)]
fn powershell_single_quote(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(windows)]
fn download_process_needles(download_root: &Path, task: &TaskRecord) -> Vec<String> {
    let mut needles = Vec::new();
    if let Some(video_id) = task.video_id.as_deref().filter(|value| !value.trim().is_empty()) {
        needles.push(video_id.to_string());
    }
    if !task.source_url.trim().is_empty() {
        needles.push(task.source_url.clone());
    }
    if !task.target_subdir.trim().is_empty() {
        needles.push(task.target_subdir.replace('/', "\\"));
    }
    for path in partial_file_candidates(download_root, task) {
        let full_path = path.to_string_lossy().replace('/', "\\");
        needles.push(full_path);
        if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
            needles.push(name.to_string());
        }
    }
    needles.sort();
    needles.dedup();
    needles
}

#[cfg(windows)]
async fn matching_download_helper_pids(download_root: &Path, task: &TaskRecord) -> Vec<u32> {
    let needles = download_process_needles(download_root, task);
    if needles.is_empty() {
        return Vec::new();
    }

    let contains_checks = needles
        .iter()
        .map(|needle| {
            format!(
                "$cmd.IndexOf('{}', [System.StringComparison]::OrdinalIgnoreCase) -ge 0",
                powershell_single_quote(needle)
            )
        })
        .collect::<Vec<_>>()
        .join(" -or ");

    let script = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         Get-CimInstance Win32_Process | Where-Object {{ \
         $cmd = $_.CommandLine; $name = $_.Name; \
         $cmd -and ($name -in @('yt-dlp.exe','yt-dlp','ffmpeg.exe','ffmpeg','python.exe','pythonw.exe')) -and ({contains_checks}) \
         }} | Select-Object -ExpandProperty ProcessId"
    );

    let output = match Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => {
            warn!("failed to query matching download helper processes: {e}");
            return Vec::new();
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            warn!("failed to query matching download helper processes: {stderr}");
        }
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

pub async fn ensure_download_processes_stopped(download_root: &Path, task: &TaskRecord) {
    #[cfg(windows)]
    {
        for _ in 0..20 {
            let pids = matching_download_helper_pids(download_root, task).await;
            if pids.is_empty() {
                return;
            }

            for pid in pids {
                kill_process_tree(Some(pid)).await;
            }

            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        let remaining = matching_download_helper_pids(download_root, task).await;
        if !remaining.is_empty() {
            warn!(
                "matching helper processes still alive for video_id={}: {:?}",
                task.video_id.as_deref().unwrap_or(""),
                remaining
            );
        }
    }

    #[cfg(not(windows))]
    let _ = (download_root, task);
}

fn partial_file_candidates(download_root: &Path, task: &TaskRecord) -> Vec<std::path::PathBuf> {
    let output_dir = if task.target_subdir.trim().is_empty() {
        download_root.to_path_buf()
    } else {
        download_root.join(&task.target_subdir)
    };
    if !output_dir.exists() {
        return Vec::new();
    }

    let video_id = task.video_id.clone().unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(&output_dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| {
            let Ok(ft) = entry.file_type() else { return None };
            if !ft.is_file() {
                return None;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let matches_task = !video_id.is_empty() && name.contains(video_id.as_str());
            let is_partial = name.ends_with(".part")
                || name.ends_with(".ytdl")
                || name.contains(".frag")
                || name.ends_with(".temp");
            if matches_task && is_partial {
                Some(path)
            } else {
                None
            }
        })
        .collect()
}

fn build_command(
    yt_dlp_path: &Path,
    runtime_dir: &Path,
    task: &TaskRecord,
    download_root: &Path,
    output_dir: &Path,
) -> Command {
    let mut command = Command::new(yt_dlp_path);

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }

    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    command
        .arg("--ignore-config")
        .arg("--newline")
        .arg("--progress")
        .arg("--paths")
        .arg(output_dir)
        .arg("-o")
        .arg("%(title)s [%(id)s].%(ext)s")
        .arg("--restrict-filenames")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(download_root);

    apply_cookie_arguments(
        &mut command,
        runtime_dir,
        task.cookie_file_name.as_deref(),
        task.cookie_browser.as_deref(),
        task.cookie_browser_profile.as_deref(),
    );

    if !task.subtitle_languages.is_empty() {
        command
            .arg("--write-subs")
            .arg("--write-auto-subs")
            .arg("--sub-langs")
            .arg(task.subtitle_languages.join(","))
            // Sleep 1 s between each subtitle track to avoid HTTP 429 rate-limits.
            .arg("--sleep-subtitles")
            .arg("1")
            // Retry subtitle (and fragment) requests up to 5 times on transient
            // errors such as HTTP 429 Too Many Requests.
            .arg("--retries")
            .arg("5")
            // If a subtitle still fails after all retries, keep going so the
            // video itself completes successfully.
            .arg("--ignore-errors");
    }

    if task.download_mode == "audio" {
        command.arg("-x").arg("--audio-format").arg(&task.container);
    } else {
        if let Some(raw_format_code) = task.raw_format_code.as_deref().filter(|value| !value.trim().is_empty()) {
            command.arg("-f").arg(raw_format_code);
        } else if let Some(height) = parse_quality_height(&task.quality) {
            command.arg("-f").arg(format!(
                "bv*[height<={height}]+ba/b[height<={height}]/best[height<={height}]/best"
            ));
        }

        if !task.container.trim().is_empty() {
            command.arg("--merge-output-format").arg(&task.container);
        }
    }

    for raw_arg in task.raw_args.iter().filter(|value| !value.trim().is_empty()) {
        command.arg(raw_arg);
    }

    command.arg(&task.source_url);

    command
}

async fn stream_output_lines<R>(state: Arc<AppState>, task_id: String, reader: BufReader<R>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Err(error) = handle_output_line(state.clone(), &task_id, &line).await {
            warn!("failed to handle yt-dlp output for {task_id}: {error}");
        }
    }
}

async fn handle_output_line(state: Arc<AppState>, task_id: &str, line: &str) -> Result<()> {
    // All hot-path updates go to in-memory RuntimeState only — no disk write.
    {
        let mut rt = state.runtime_states.write().await;
        let entry = rt.entry(task_id.to_string()).or_default();
        if let Some(progress) = parse_progress_line(line, &entry.progress) {
            entry.progress = progress;
        }
        entry.push_log(line.to_string());
    }

    let task = match state.task_store.load_task(task_id) {
        Ok(task) => task,
        Err(error) => {
            let is_active = state
                .active_downloads
                .lock()
                .expect("active download lock poisoned")
                .contains_key(task_id);
            if is_active {
                return Err(error);
            }
            return Ok(());
        }
    };
    let view = build_task_view(&state, &task).await;
    events::publish(&state.event_tx, ServerEvent::task_updated(&view));
    Ok(())
}

async fn finalize_download(
    state: Arc<AppState>,
    task_id: &str,
    wait_result: std::io::Result<std::process::ExitStatus>,
) -> Result<()> {
    {
        let mut active = state
            .active_downloads
            .lock()
            .expect("active download lock poisoned");
        active.remove(task_id);
    }

    let mut task = state.task_store.load_task(task_id)?;
    match wait_result {
        Ok(status) if status.success() => {
            task.state = TaskState::Completed;
            task.last_error = None;

            let settings = state.settings.read().await.clone();
            task.output_files = collect_output_files(&settings.download_root, &task)?;

            let missing_subtitles = missing_subtitle_languages(&task);

            // Mark 100 % and optionally push the subtitle warning.
            {
                let mut rt = state.runtime_states.write().await;
                let entry = rt.entry(task_id.to_string()).or_default();
                entry.progress.percent = 100.0;
                if !missing_subtitles.is_empty() {
                    let message = format!(
                        "Some requested subtitles were not downloaded: {}. \
                         This is usually caused by YouTube rate limiting (HTTP 429). \
                         Configure a cookies file to improve subtitle success rate.",
                        missing_subtitles.join(", ")
                    );
                    task.last_error = Some(message.clone());
                    entry.push_log(format!("[warning] {message}"));
                }
            }
        }
        Ok(_) if matches!(task.state, TaskState::Stopping) => {
            task.state = TaskState::Paused;
            task.last_error = None;
        }
        Ok(status) => {
            // Use the last error line collected during the run if available.
            let last_hint = {
                let rt = state.runtime_states.read().await;
                rt.get(task_id).and_then(|r| r.error_hints.last().cloned())
            };

            if should_retry_youtube_without_cookies(&task, last_hint.as_deref()) {
                {
                    let mut rt = state.runtime_states.write().await;
                    let mut entry = RuntimeState::default();
                    entry.push_log(
                        "[warning] Authenticated YouTube format selection failed; retrying once without cookies.".to_string(),
                    );
                    rt.insert(task_id.to_string(), entry);
                }

                task.cookie_file_name = None;
                task.cookie_browser = None;
                task.cookie_browser_profile = None;
                task.state = TaskState::Queued;
                task.last_error = None;
                task.update_timestamp();
                state.task_store.save_task(&task)?;

                let view = build_task_view(&state, &task).await;
                events::publish(&state.event_tx, ServerEvent::task_updated(&view));
                publish_refreshed_playlist_parents(&state)?;
                schedule_pending(state).await?;
                return Ok(());
            }

            task.state = TaskState::Failed;
            task.last_error = last_hint
                .as_deref()
                .and_then(explain_cookie_error)
                .or(last_hint)
                .or_else(|| Some(format!("yt-dlp exited with status {status}")));
        }
        Err(error) => {
            task.state = TaskState::Failed;
            task.last_error = Some(error.to_string());
        }
    }

    task.update_timestamp();
    state.task_store.save_task(&task)?;
    let view = build_task_view(&state, &task).await;
    events::publish(&state.event_tx, ServerEvent::task_updated(&view));
    publish_refreshed_playlist_parents(&state)?;

    if matches!(task.state, TaskState::Completed) {
        // Update the in-memory filename cache with the newly downloaded files.
        let mut cache = state.library_filenames.write().await;
        for file_path in &task.output_files {
            if let Some(name) = std::path::Path::new(file_path)
                .file_name()
                .and_then(|n| n.to_str())
            {
                cache.insert(name.to_string());
            }
        }
        drop(cache);
        events::publish(
            &state.event_tx,
            ServerEvent::library_changed("task_completed", &task.output_files),
        );
    }

    spawn_scheduler(state.clone());

    Ok(())
}

fn should_retry_youtube_without_cookies(task: &TaskRecord, last_hint: Option<&str>) -> bool {
    let has_cookies = task
        .cookie_file_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some()
        || task
            .cookie_browser
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some();

    if !has_cookies {
        return false;
    }

    let is_youtube = task
        .extractor
        .as_deref()
        .map(|value| value.to_ascii_lowercase().contains("youtube"))
        .unwrap_or(false)
        || task.source_url.contains("youtube.com")
        || task.source_url.contains("youtu.be");

    if !is_youtube {
        return false;
    }

    last_hint
        .map(|message| message.to_ascii_lowercase().contains("requested format is not available"))
        .unwrap_or(false)
}

fn spawn_scheduler(state: Arc<AppState>) {
    tokio::spawn(async move {
        if let Err(error) = schedule_pending(state).await {
            error!("failed to schedule next queued task: {error}");
        }
    });
}

fn publish_refreshed_playlist_parents(state: &Arc<AppState>) -> Result<()> {
    let parents = state.task_store.refresh_playlist_parents()?;
    for parent in &parents {
        // PlaylistParent tasks never have their own RuntimeState.
        let view = TaskView::from_record(parent, None);
        events::publish(&state.event_tx, ServerEvent::task_updated(&view));
    }
    Ok(())
}

/// Build a [`TaskView`] by merging the persisted record with its in-memory runtime state.
async fn build_task_view(state: &Arc<AppState>, task: &TaskRecord) -> TaskView {
    let rt = state.runtime_states.read().await;
    TaskView::from_record(task, rt.get(&task.id))
}

/// Delete any yt-dlp partial files in the task's output directory that belong
/// to this task (matched by video ID).  Retries up to `max_retries` times
/// with 500 ms gaps so callers don't need to worry about races with Windows
/// releasing file handles shortly after a process exits.
pub async fn delete_related_partial_files(download_root: &Path, task: &TaskRecord) {
    let output_dir = if task.target_subdir.trim().is_empty() {
        download_root.to_path_buf()
    } else {
        download_root.join(&task.target_subdir)
    };
    if !output_dir.exists() {
        return;
    }

    let video_id = task.video_id.clone().unwrap_or_default();
    for attempt in 0..10u32 {
        let Ok(entries) = std::fs::read_dir(&output_dir) else { return };
        let mut locked = false;
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_file() { continue; }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let matches_task = !video_id.is_empty() && name.contains(video_id.as_str());
            let is_partial = name.ends_with(".part")
                || name.ends_with(".ytdl")
                || name.contains(".frag")
                || name.ends_with(".temp");
            if matches_task && is_partial {
                let path = entry.path();
                if let Err(e) = std::fs::remove_file(&path) {
                    if e.raw_os_error() == Some(32) {
                        locked = true;
                        continue; // try remaining files, retry locked ones next attempt
                    }
                    warn!("failed to remove partial file {}: {e}", path.display());
                }
            }
        }
        if !locked {
            return;
        }
        let remaining = 9 - attempt;
        warn!("some partial files still locked, retrying in 500 ms ({remaining} retries left)");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    warn!("gave up deleting locked partial files for video_id={video_id}");
}

fn parse_quality_height(quality: &str) -> Option<u32> {
    let digits: String = quality.chars().filter(|ch| ch.is_ascii_digit()).collect();
    digits.parse::<u32>().ok()
}

fn parse_progress_line(line: &str, previous: &TaskProgress) -> Option<TaskProgress> {
    if !line.contains("[download]") || !line.contains('%') {
        return None;
    }

    let percent = line
        .split('%')
        .next()
        .and_then(|prefix| prefix.split_whitespace().last())
        .and_then(|value| value.parse::<f64>().ok())?;

    let speed_bytes_per_sec = line
        .split(" at ")
        .nth(1)
        .and_then(|segment| segment.split_whitespace().next())
        .and_then(parse_rate_to_bytes);

    let eta_seconds = line
        .split(" ETA ")
        .nth(1)
        .and_then(|segment| segment.split_whitespace().next())
        .and_then(parse_eta_seconds);

    Some(TaskProgress {
        percent,
        downloaded_bytes: previous.downloaded_bytes,
        total_bytes: previous.total_bytes,
        speed_bytes_per_sec,
        eta_seconds,
    })
}

fn parse_rate_to_bytes(rate: &str) -> Option<u64> {
    let normalized = rate.trim();
    let (value, multiplier) = if let Some(value) = normalized.strip_suffix("KiB/s") {
        (value, 1024.0)
    } else if let Some(value) = normalized.strip_suffix("MiB/s") {
        (value, 1024.0 * 1024.0)
    } else if let Some(value) = normalized.strip_suffix("GiB/s") {
        (value, 1024.0 * 1024.0 * 1024.0)
    } else if let Some(value) = normalized.strip_suffix("B/s") {
        (value, 1.0)
    } else {
        return None;
    };

    value
        .trim()
        .parse::<f64>()
        .ok()
        .map(|number| (number * multiplier) as u64)
}

fn parse_eta_seconds(value: &str) -> Option<u64> {
    let mut seconds = 0_u64;
    let parts: Vec<_> = value.split(':').collect();
    if parts.is_empty() {
        return None;
    }

    for part in parts {
        seconds = seconds.checked_mul(60)?;
        seconds = seconds.checked_add(part.parse::<u64>().ok()?)?;
    }

    Some(seconds)
}

fn collect_output_files(download_root: &Path, task: &TaskRecord) -> Result<Vec<String>> {
    let output_dir = if task.target_subdir.trim().is_empty() {
        download_root.to_path_buf()
    } else {
        download_root.join(&task.target_subdir)
    };
    if !output_dir.exists() {
        return Ok(Vec::new());
    }

    let video_id = task.video_id.as_deref().unwrap_or("");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }

        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !video_id.is_empty() && !file_name.contains(video_id) {
            continue;
        }
        if file_name.ends_with(".part") || file_name.ends_with(".ytdl") || file_name.ends_with(".info.json") {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(download_root)
            .ok()
            .and_then(|path| path.to_str())
            .map(|path| path.replace('\\', "/"))
            .unwrap_or_else(|| entry.path().to_string_lossy().to_string());
        files.push(relative);
    }

    Ok(files)
}

fn missing_subtitle_languages(task: &TaskRecord) -> Vec<String> {
    if task.subtitle_languages.is_empty() {
        return Vec::new();
    }

    let output_files_lower: Vec<String> = task
        .output_files
        .iter()
        .map(|file| file.to_ascii_lowercase())
        .collect();

    task.subtitle_languages
        .iter()
        .filter_map(|lang| {
            let normalized = lang.trim();
            if normalized.is_empty() {
                return None;
            }

            let token = format!(".{}.", normalized.to_ascii_lowercase());
            let found = output_files_lower.iter().any(|file| {
                file.contains(&token)
                    && (file.ends_with(".vtt")
                        || file.ends_with(".srt")
                        || file.ends_with(".ass")
                        || file.ends_with(".ssa")
                        || file.ends_with(".ttml")
                        || file.ends_with(".lrc")
                        || file.ends_with(".srv3")
                        || file.ends_with(".json3"))
            });

            if found {
                None
            } else {
                Some(normalized.to_string())
            }
        })
        .collect()
}


fn current_active_ids(state: &Arc<AppState>) -> Vec<String> {
    state
        .active_downloads
        .lock()
        .expect("active download lock poisoned")
        .keys()
        .cloned()
        .collect()
}
