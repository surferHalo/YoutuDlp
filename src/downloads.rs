use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{error, warn};

use crate::app::{ActiveDownload, AppState};
use crate::tasks::{TaskProgress, TaskRecord, TaskState, output_directory};

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
    let mut task = state.task_store.load_task(task_id)?;
    if !matches!(task.state, TaskState::Downloading) {
        bail!("task is not currently downloading");
    }

    task.state = TaskState::Stopping;
    task.update_timestamp();
    state.task_store.save_task(&task)?;

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

pub async fn resume_task(state: Arc<AppState>, task_id: &str) -> Result<TaskRecord> {
    let mut task = state.task_store.load_task(task_id)?;
    if !matches!(task.state, TaskState::Paused | TaskState::Failed) {
        bail!("task can only be resumed from paused or failed state");
    }

    task.state = TaskState::Queued;
    task.update_timestamp();
    state.task_store.save_task(&task)?;
    state.task_store.append_to_queue(&task.id)?;
    schedule_pending(state).await?;

    Ok(task)
}

pub async fn restart_task(state: Arc<AppState>, task_id: &str) -> Result<TaskRecord> {
    let mut task = state.task_store.load_task(task_id)?;
    if matches!(task.state, TaskState::Downloading | TaskState::Stopping) {
        bail!("stop the task before restarting it");
    }

    let settings = state.settings.read().await.clone();
    delete_related_partial_files(&settings.download_root, &task)?;

    task.reset_for_restart();
    state.task_store.save_task(&task)?;
    state.task_store.append_to_queue(&task.id)?;
    schedule_pending(state).await?;

    Ok(task)
}

async fn start_download(state: Arc<AppState>, mut task: TaskRecord) -> Result<()> {
    let settings = state.settings.read().await.clone();
    let yt_dlp_path = state
        .tools
        .read()
        .await
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

    let mut command = build_command(&yt_dlp_path, &task, &settings.download_root, &output_dir);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            task.state = TaskState::Failed;
            task.last_error = Some(error.to_string());
            task.update_timestamp();
            state.task_store.save_task(&task)?;
            return Ok(());
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (stop_tx, stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    {
        let mut active = state
            .active_downloads
            .lock()
            .expect("active download lock poisoned");
        active.insert(task.id.clone(), ActiveDownload { stop_tx });
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

    spawn_waiter(state, task.id.clone(), child, stop_rx);

    Ok(())
}

fn spawn_waiter(
    state: Arc<AppState>,
    task_id: String,
    mut child: tokio::process::Child,
    mut stop_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    tokio::spawn(async move {
        let wait_result = tokio::select! {
            result = child.wait() => result,
            _ = stop_rx.recv() => {
                let _ = child.start_kill();
                child.wait().await
            }
        };

        if let Err(error) = finalize_download(state.clone(), &task_id, wait_result).await {
            error!("failed to finalize task {}: {error}", task_id);
        }
    });
}

fn build_command(
    yt_dlp_path: &Path,
    task: &TaskRecord,
    download_root: &Path,
    output_dir: &Path,
) -> Command {
    let mut command = Command::new(yt_dlp_path);
    command
        .arg("--newline")
        .arg("--progress")
        .arg("--no-warnings")
        .arg("--paths")
        .arg(output_dir)
        .arg("-o")
        .arg("%(title)s [%(id)s].%(ext)s")
        .arg("--write-info-json")
        .arg("--restrict-filenames")
        .arg(&task.source_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(download_root);

    if task.download_mode == "audio" {
        command.arg("-x").arg("--audio-format").arg(&task.container);
    } else {
        if let Some(height) = parse_quality_height(&task.quality) {
            command.arg("-f").arg(format!(
                "bv*[height<={height}]+ba/b[height<={height}]/best[height<={height}]/best"
            ));
        }

        if !task.container.trim().is_empty() {
            command.arg("--merge-output-format").arg(&task.container);
        }
    }

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
    let mut task = state.task_store.load_task(task_id)?;
    task.push_log(line.to_string());

    if let Some(progress) = parse_progress_line(line, &task.progress) {
        task.progress = progress;
    }

    if line.to_ascii_lowercase().contains("error") {
        task.last_error = Some(line.to_string());
    }

    state.task_store.save_task(&task)?;
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
            task.progress.percent = 100.0;
            task.last_error = None;

            let settings = state.settings.read().await.clone();
            task.output_files = collect_output_files(&settings.download_root, &task)?;
        }
        Ok(_) if matches!(task.state, TaskState::Stopping) => {
            task.state = TaskState::Paused;
            task.last_error = None;
        }
        Ok(status) => {
            task.state = TaskState::Failed;
            task.last_error = Some(format!("yt-dlp exited with status {status}"));
        }
        Err(error) => {
            task.state = TaskState::Failed;
            task.last_error = Some(error.to_string());
        }
    }

    task.update_timestamp();
    state.task_store.save_task(&task)?;

    spawn_scheduler(state.clone());

    Ok(())
}

fn spawn_scheduler(state: Arc<AppState>) {
    tokio::spawn(async move {
        if let Err(error) = schedule_pending(state).await {
            error!("failed to schedule next queued task: {error}");
        }
    });
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
        if file_name.ends_with(".part") || file_name.ends_with(".ytdl") {
            continue;
        }

        files.push(entry.path().to_string_lossy().to_string());
    }

    Ok(files)
}

fn delete_related_partial_files(download_root: &Path, task: &TaskRecord) -> Result<()> {
    let output_dir = if task.target_subdir.trim().is_empty() {
        download_root.to_path_buf()
    } else {
        download_root.join(&task.target_subdir)
    };
    if !output_dir.exists() {
        return Ok(());
    }

    let video_id = task.video_id.clone().unwrap_or_default();
    for entry in std::fs::read_dir(output_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        let matches_task = !video_id.is_empty() && name.contains(&video_id);
        let is_partial = name.ends_with(".part")
            || name.ends_with(".ytdl")
            || name.contains(".frag")
            || name.ends_with(".temp");

        if matches_task || is_partial {
            let path = entry.path();
            if let Err(error) = std::fs::remove_file(&path) {
                warn!("failed to remove partial file {}: {error}", path.display());
            }
        }
    }

    Ok(())
}

fn current_active_ids(state: &Arc<AppState>) -> Vec<String> {
    let active: HashMap<String, ActiveDownload> = state
        .active_downloads
        .lock()
        .expect("active download lock poisoned")
        .clone();
    active.into_keys().collect()
}
