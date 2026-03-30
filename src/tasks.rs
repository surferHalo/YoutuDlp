use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::settings::AppSettings;
use crate::storage;

static TASK_COUNTER: AtomicU64 = AtomicU64::new(1);

// ── State machine ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Submitted,
    Resolving,
    Queued,
    Downloading,
    Stopping,
    Paused,
    Completed,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    #[default]
    Single,
    PlaylistParent,
    PlaylistItem,
}

// ── Progress (runtime-only, never persisted) ──────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaskProgress {
    pub percent: f64,
    pub downloaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_sec: Option<u64>,
    pub eta_seconds: Option<u64>,
}

// ── Runtime state (lives in AppState::runtime_states, never on disk) ──────────

#[derive(Debug, Default)]
pub struct RuntimeState {
    pub progress: TaskProgress,
    /// Rolling window of the last 200 yt-dlp output lines.
    pub recent_logs: VecDeque<String>,
    /// Error lines collected during the run; used by finalize to pick last_error.
    pub error_hints: Vec<String>,
}

impl RuntimeState {
    pub fn push_log(&mut self, line: String) {
        if line.to_ascii_lowercase().contains("error") {
            self.error_hints.push(line.clone());
        }
        self.recent_logs.push_back(line);
        if self.recent_logs.len() > 200 {
            self.recent_logs.pop_front();
        }
    }
}

// ── Persistent task record (written to disk only on state transitions) ─────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    // Identity & config (written once at creation, never changed)
    pub id: String,
    pub task_kind: TaskKind,
    pub source_url: String,
    #[serde(default)]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub child_task_ids: Vec<String>,
    pub extractor: Option<String>,
    pub video_id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub playlist_title: Option<String>,
    #[serde(default)]
    pub playlist_index: Option<u64>,
    pub uploader: Option<String>,
    pub duration_seconds: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub estimated_size_bytes: Option<u64>,
    pub target_subdir: String,
    pub output_dir_relative: String,
    pub download_mode: String,
    pub quality: String,
    pub container: String,
    #[serde(default)]
    pub subtitle_languages: Vec<String>,
    #[serde(default)]
    pub cookie_file_name: Option<String>,
    #[serde(default)]
    pub raw_args: Vec<String>,
    #[serde(default)]
    pub raw_format_code: Option<String>,
    pub created_at: String,

    // Mutable fields (written on state transitions only — a handful of times per task)
    pub state: TaskState,
    pub updated_at: String,
    pub output_files: Vec<String>,
    pub last_error: Option<String>,
}

// ── View model sent to the frontend ───────────────────────────────────────────
//
// `TaskView` merges the persisted `TaskRecord` with the in-memory
// `RuntimeState`.  It is constructed on the fly and never stored anywhere.

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub id: String,
    pub task_kind: TaskKind,
    pub source_url: String,
    pub parent_task_id: Option<String>,
    pub child_task_ids: Vec<String>,
    pub extractor: Option<String>,
    pub video_id: Option<String>,
    pub title: String,
    pub playlist_title: Option<String>,
    pub playlist_index: Option<u64>,
    pub uploader: Option<String>,
    pub duration_seconds: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub estimated_size_bytes: Option<u64>,
    pub target_subdir: String,
    pub output_dir_relative: String,
    pub download_mode: String,
    pub quality: String,
    pub container: String,
    pub subtitle_languages: Vec<String>,
    pub cookie_file_name: Option<String>,
    pub raw_args: Vec<String>,
    pub raw_format_code: Option<String>,
    pub created_at: String,
    pub state: TaskState,
    pub updated_at: String,
    pub output_files: Vec<String>,
    pub last_error: Option<String>,
    // Runtime-only fields (zero / empty when no active runtime state)
    pub progress: TaskProgress,
    pub recent_logs: Vec<String>,
}

impl TaskView {
    pub fn from_record(record: &TaskRecord, rt: Option<&RuntimeState>) -> Self {
        let (progress, recent_logs) = match rt {
            Some(rt) => (rt.progress.clone(), rt.recent_logs.iter().cloned().collect()),
            None => (TaskProgress::default(), Vec::new()),
        };
        Self {
            id: record.id.clone(),
            task_kind: record.task_kind.clone(),
            source_url: record.source_url.clone(),
            parent_task_id: record.parent_task_id.clone(),
            child_task_ids: record.child_task_ids.clone(),
            extractor: record.extractor.clone(),
            video_id: record.video_id.clone(),
            title: record.title.clone(),
            playlist_title: record.playlist_title.clone(),
            playlist_index: record.playlist_index,
            uploader: record.uploader.clone(),
            duration_seconds: record.duration_seconds,
            thumbnail_url: record.thumbnail_url.clone(),
            estimated_size_bytes: record.estimated_size_bytes,
            target_subdir: record.target_subdir.clone(),
            output_dir_relative: record.output_dir_relative.clone(),
            download_mode: record.download_mode.clone(),
            quality: record.quality.clone(),
            container: record.container.clone(),
            subtitle_languages: record.subtitle_languages.clone(),
            cookie_file_name: record.cookie_file_name.clone(),
            raw_args: record.raw_args.clone(),
            raw_format_code: record.raw_format_code.clone(),
            created_at: record.created_at.clone(),
            state: record.state.clone(),
            updated_at: record.updated_at.clone(),
            output_files: record.output_files.clone(),
            last_error: record.last_error.clone(),
            progress,
            recent_logs,
        }
    }
}

// ── Queue ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QueueState {
    pub task_order: Vec<String>,
    pub updated_at: String,
}

// ── Request / response DTOs ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveRequest {
    pub url: String,
    pub download_mode: String,
    pub quality: String,
    pub container: String,
    pub target_subdir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedPlaylistEntry {
    pub source_url: String,
    pub title: String,
    pub extractor: Option<String>,
    pub video_id: Option<String>,
    pub uploader: Option<String>,
    pub duration_seconds: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub estimated_size_bytes: Option<u64>,
    pub playlist_index: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleOption {
    pub language_code: String,
    pub is_auto: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedMedia {
    pub source_url: String,
    pub title: String,
    pub extractor: Option<String>,
    pub video_id: Option<String>,
    pub uploader: Option<String>,
    pub duration_seconds: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub estimated_size_bytes: Option<u64>,
    pub is_playlist: bool,
    pub playlist_title: Option<String>,
    pub playlist_count: Option<u64>,
    #[serde(default)]
    pub entries: Vec<ResolvedPlaylistEntry>,
    #[serde(default)]
    pub available_qualities: Vec<String>,
    #[serde(default)]
    pub available_subtitles: Vec<SubtitleOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateCheck {
    pub status: String,
    pub matches: Vec<DuplicateMatch>,
    pub default_action: String,
    pub allowed_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateMatch {
    pub source: String,
    pub path: Option<String>,
    pub task_id: Option<String>,
    pub state: Option<TaskState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveResponse {
    pub resolved: ResolvedMedia,
    pub duplicate_check: DuplicateCheck,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTaskRequest {
    pub source_url: String,
    pub target_subdir: String,
    pub download_mode: String,
    pub quality: String,
    pub container: String,
    #[serde(default)]
    pub subtitle_languages: Vec<String>,
    #[serde(default)]
    pub cookie_file_name: Option<String>,
    #[serde(default)]
    pub raw_args: Vec<String>,
    #[serde(default)]
    pub raw_format_code: Option<String>,
    pub resolved: ResolvedMedia,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePlaylistTasksRequest {
    pub source_url: String,
    pub target_subdir: String,
    pub download_mode: String,
    pub quality: String,
    pub container: String,
    #[serde(default)]
    pub subtitle_languages: Vec<String>,
    #[serde(default)]
    pub cookie_file_name: Option<String>,
    #[serde(default)]
    pub raw_args: Vec<String>,
    #[serde(default)]
    pub raw_format_code: Option<String>,
    pub resolved: ResolvedMedia,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchCreateTasksResponse {
    pub tasks: Vec<TaskView>,
    pub parent_task_id: Option<String>,
    pub created_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskListResponse {
    pub tasks: Vec<TaskView>,
}

// ── TaskRecord helpers ─────────────────────────────────────────────────────────

impl TaskRecord {
    pub fn update_timestamp(&mut self) {
        self.updated_at = Utc::now().to_rfc3339();
    }
}

// ── QueueState helpers ─────────────────────────────────────────────────────────

impl QueueState {
    pub fn empty() -> Self {
        Self {
            task_order: Vec::new(),
            updated_at: Utc::now().to_rfc3339(),
        }
    }
}

// ── TaskStore ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct TaskStore {
    tasks_dir: PathBuf,
    queue_path: PathBuf,
    /// Serialises disk writes so the remove+rename pair is never interleaved.
    write_lock: Mutex<()>,
}

impl TaskStore {
    pub fn new(tasks_dir: PathBuf, queue_path: PathBuf) -> Self {
        Self {
            tasks_dir,
            queue_path,
            write_lock: Mutex::new(()),
        }
    }

    // ── Queue ──────────────────────────────────────────────────────────────────

    pub fn load_or_create_queue(&self) -> Result<QueueState> {
        if !self.queue_path.exists() {
            let queue = QueueState::empty();
            self.save_queue_inner(&queue)?;
            return Ok(queue);
        }
        let raw = fs::read(&self.queue_path)
            .with_context(|| format!("failed to read queue file {}", self.queue_path.display()))?;
        match serde_json::from_slice::<QueueState>(&raw) {
            Ok(queue) => Ok(queue),
            Err(err) => {
                warn!(
                    "queue file {} is corrupted ({}), recreating",
                    self.queue_path.display(),
                    err
                );
                let queue = QueueState::empty();
                self.save_queue_inner(&queue)?;
                Ok(queue)
            }
        }
    }

    fn save_queue_inner(&self, queue: &QueueState) -> Result<()> {
        let _g = self.write_lock.lock().expect("write lock poisoned");
        storage::write_json_atomic(&self.queue_path, queue)
    }

    pub fn append_to_queue(&self, task_id: &str) -> Result<()> {
        let mut queue = self.load_or_create_queue()?;
        if !queue.task_order.iter().any(|id| id == task_id) {
            queue.task_order.push(task_id.to_string());
            queue.updated_at = Utc::now().to_rfc3339();
            self.save_queue_inner(&queue)?;
        }
        Ok(())
    }

    pub fn remove_from_queue(&self, task_ids: &[String]) -> Result<()> {
        let mut queue = self.load_or_create_queue()?;
        queue.task_order.retain(|id| !task_ids.contains(id));
        queue.updated_at = Utc::now().to_rfc3339();
        self.save_queue_inner(&queue)?;
        Ok(())
    }

    // ── Tasks ──────────────────────────────────────────────────────────────────

    pub fn load_task(&self, task_id: &str) -> Result<TaskRecord> {
        let path = self.task_path(task_id);
        let raw = fs::read(&path)
            .with_context(|| format!("failed to read task file {}", path.display()))?;
        serde_json::from_slice::<TaskRecord>(&raw)
            .with_context(|| format!("failed to parse task file {}", path.display()))
    }

    pub fn save_task(&self, task: &TaskRecord) -> Result<()> {
        let _g = self.write_lock.lock().expect("write lock poisoned");
        storage::write_json_atomic(&self.task_path(&task.id), task)
    }

    pub fn load_all_tasks(&self) -> Result<Vec<TaskRecord>> {
        let mut tasks = Vec::new();
        if !self.tasks_dir.exists() {
            return Ok(tasks);
        }
        for entry in fs::read_dir(&self.tasks_dir).with_context(|| {
            format!("failed to read tasks directory {}", self.tasks_dir.display())
        })? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = match fs::read(entry.path()) {
                Ok(b) => b,
                Err(err) => {
                    warn!("failed to read task file {}: {}", entry.path().display(), err);
                    continue;
                }
            };
            match serde_json::from_slice::<TaskRecord>(&raw) {
                Ok(task) => tasks.push(task),
                Err(err) => {
                    let path = entry.path();
                    if fs::remove_file(&path).is_ok() {
                        warn!("removed corrupted task file {} ({})", path.display(), err);
                    } else {
                        warn!("corrupted task file {} ({}) — cannot remove", path.display(), err);
                    }
                }
            }
        }
        Ok(tasks)
    }

    /// Returns tasks in queue order (parent before its children), with
    /// tasks not in the queue appended sorted by creation time.
    pub fn list_tasks_in_queue_order(&self) -> Result<Vec<TaskRecord>> {
        let queue = self.load_or_create_queue()?;
        let by_id: HashMap<String, TaskRecord> = self
            .load_all_tasks()?
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();
        let mut ordered = Vec::new();
        let mut emitted = HashSet::new();

        for task_id in &queue.task_order {
            let Some(task) = by_id.get(task_id).cloned() else { continue };

            // If this is a child, emit parent first.
            if let Some(parent_id) = &task.parent_task_id {
                if emitted.insert(parent_id.clone()) {
                    if let Some(parent) = by_id.get(parent_id).cloned() {
                        ordered.push(parent);
                    }
                }
            }
            if emitted.insert(task.id.clone()) {
                ordered.push(task);
            }
        }

        let mut remaining: Vec<TaskRecord> = by_id
            .into_values()
            .filter(|t| !emitted.contains(&t.id))
            .collect();
        remaining.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        ordered.extend(remaining);
        Ok(ordered)
    }

    pub fn find_first_queued_task_excluding(
        &self,
        excluded_ids: &[String],
    ) -> Result<Option<TaskRecord>> {
        let queue = self.load_or_create_queue()?;
        for task_id in queue.task_order {
            if excluded_ids.contains(&task_id) {
                continue;
            }
            let task = match self.load_task(&task_id) {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Never dispatch a PlaylistParent directly — it's just a display node.
            if matches!(task.task_kind, TaskKind::PlaylistParent) {
                continue;
            }
            if matches!(task.state, TaskState::Queued) {
                return Ok(Some(task));
            }
        }
        Ok(None)
    }

    // ── Startup recovery ───────────────────────────────────────────────────────

    pub fn recover_on_startup(&self) -> Result<()> {
        let tasks = self.load_all_tasks()?;
        for mut task in tasks {
            let updated = match task.state {
                TaskState::Downloading | TaskState::Stopping => {
                    task.state = TaskState::Paused;
                    task.last_error =
                        Some("应用在下载过程中退出，任务已恢复为可继续状态".to_string());
                    true
                }
                TaskState::Submitted | TaskState::Resolving => {
                    task.state = TaskState::Failed;
                    task.last_error = Some("应用在解析阶段退出，任务已标记为失败".to_string());
                    true
                }
                _ => false,
            };
            if updated {
                task.update_timestamp();
                self.save_task(&task)?;
            }
        }
        Ok(())
    }

    // ── Duplicate detection ────────────────────────────────────────────────────

    pub fn find_duplicates(
        &self,
        extractor: Option<&str>,
        video_id: Option<&str>,
        library_filenames: &HashSet<String>,
    ) -> Result<DuplicateCheck> {
        let (Some(extractor), Some(video_id)) = (extractor, video_id) else {
            return Ok(no_duplicate());
        };
        let mut matches = Vec::new();
        for task in self.load_all_tasks()? {
            if task.extractor.as_deref() == Some(extractor)
                && task.video_id.as_deref() == Some(video_id)
            {
                matches.push(DuplicateMatch {
                    source: "task".to_string(),
                    path: task.output_files.first().cloned(),
                    task_id: Some(task.id),
                    state: Some(task.state),
                });
            }
        }
        let needle = format!("[{video_id}]");
        for name in library_filenames {
            if name.contains(&needle) {
                matches.push(DuplicateMatch {
                    source: "library".to_string(),
                    path: Some(name.clone()),
                    task_id: None,
                    state: None,
                });
            }
        }
        if matches.is_empty() {
            Ok(no_duplicate())
        } else {
            Ok(DuplicateCheck {
                status: "duplicate-found".to_string(),
                matches,
                default_action: "skip".to_string(),
                allowed_actions: vec![
                    "skip".to_string(),
                    "add_anyway".to_string(),
                    "open_existing".to_string(),
                ],
            })
        }
    }

    // ── Task creation ──────────────────────────────────────────────────────────

    pub fn create_task_from_request(
        &self,
        request: &CreateTaskRequest,
        library_filenames: &HashSet<String>,
    ) -> Result<TaskRecord> {
        if request.resolved.is_playlist {
            bail!("use the batch endpoint to enqueue a playlist");
        }
        self.ensure_not_duplicate(
            request.resolved.extractor.as_deref(),
            request.resolved.video_id.as_deref(),
            library_filenames,
        )?;
        let task = make_single_task(request);
        self.save_task(&task)?;
        self.append_to_queue(&task.id)?;
        Ok(task)
    }

    pub fn create_playlist_tasks_from_request(
        &self,
        request: &CreatePlaylistTasksRequest,
        library_filenames: &HashSet<String>,
    ) -> Result<BatchCreateTasksResponse> {
        if !request.resolved.is_playlist {
            bail!("playlist batch creation requires a playlist resolve result");
        }
        if request.resolved.entries.is_empty() {
            bail!("playlist has no resolved entries to enqueue");
        }

        let parent_id = new_task_id();
        let now = Utc::now().to_rfc3339();
        let mut child_tasks: Vec<TaskRecord> = Vec::new();
        let mut child_ids: Vec<String> = Vec::new();
        let mut seen_keys: HashSet<String> = HashSet::new();

        for entry in &request.resolved.entries {
            let dup_key = entry
                .extractor
                .as_deref()
                .zip(entry.video_id.as_deref())
                .map(|(e, v)| format!("{e}:{v}"));
            if let Some(ref key) = dup_key {
                if !seen_keys.insert(key.clone()) {
                    continue;
                }
            }
            if self
                .find_duplicates(
                    entry.extractor.as_deref(),
                    entry.video_id.as_deref(),
                    library_filenames,
                )?
                .status
                == "duplicate-found"
            {
                continue;
            }

            let task = TaskRecord {
                id: new_task_id(),
                task_kind: TaskKind::PlaylistItem,
                source_url: entry.source_url.clone(),
                parent_task_id: Some(parent_id.clone()),
                child_task_ids: Vec::new(),
                extractor: entry.extractor.clone(),
                video_id: entry.video_id.clone(),
                title: entry.title.clone(),
                playlist_title: request.resolved.playlist_title.clone(),
                playlist_index: entry.playlist_index,
                uploader: entry.uploader.clone(),
                duration_seconds: entry.duration_seconds,
                thumbnail_url: entry.thumbnail_url.clone(),
                estimated_size_bytes: entry.estimated_size_bytes,
                target_subdir: request.target_subdir.clone(),
                output_dir_relative: request.target_subdir.clone(),
                download_mode: request.download_mode.clone(),
                quality: request.quality.clone(),
                container: request.container.clone(),
                subtitle_languages: request.subtitle_languages.clone(),
                cookie_file_name: request.cookie_file_name.clone(),
                raw_args: request.raw_args.clone(),
                raw_format_code: request.raw_format_code.clone(),
                created_at: now.clone(),
                state: TaskState::Queued,
                updated_at: now.clone(),
                output_files: Vec::new(),
                last_error: None,
            };
            child_ids.push(task.id.clone());
            child_tasks.push(task);
        }

        if child_tasks.is_empty() {
            bail!("all playlist entries already exist in the queue or download library");
        }

        let estimated_size_bytes = child_tasks
            .iter()
            .try_fold(0_u64, |sum, t| t.estimated_size_bytes.map(|s| sum + s));

        let parent_task = TaskRecord {
            id: parent_id.clone(),
            task_kind: TaskKind::PlaylistParent,
            source_url: request.source_url.clone(),
            parent_task_id: None,
            child_task_ids: child_ids.clone(),
            extractor: request.resolved.extractor.clone(),
            video_id: request.resolved.video_id.clone(),
            title: request
                .resolved
                .playlist_title
                .clone()
                .unwrap_or_else(|| request.resolved.title.clone()),
            playlist_title: request.resolved.playlist_title.clone(),
            playlist_index: None,
            uploader: request.resolved.uploader.clone(),
            duration_seconds: None,
            thumbnail_url: request.resolved.thumbnail_url.clone(),
            estimated_size_bytes,
            target_subdir: request.target_subdir.clone(),
            output_dir_relative: request.target_subdir.clone(),
            download_mode: request.download_mode.clone(),
            quality: request.quality.clone(),
            container: request.container.clone(),
            subtitle_languages: request.subtitle_languages.clone(),
            cookie_file_name: request.cookie_file_name.clone(),
            raw_args: request.raw_args.clone(),
            raw_format_code: request.raw_format_code.clone(),
            created_at: now.clone(),
            state: TaskState::Queued,
            updated_at: now,
            output_files: Vec::new(),
            last_error: None,
        };

        self.save_task(&parent_task)?;
        for task in &child_tasks {
            self.save_task(task)?;
            self.append_to_queue(&task.id)?;
        }

        let created_count = child_tasks.len() + 1;
        let mut all_views: Vec<TaskView> = Vec::with_capacity(created_count);
        all_views.push(TaskView::from_record(&parent_task, None));
        for t in &child_tasks {
            all_views.push(TaskView::from_record(t, None));
        }

        Ok(BatchCreateTasksResponse {
            tasks: all_views,
            parent_task_id: Some(parent_id),
            created_count,
        })
    }

    // ── Task deletion ──────────────────────────────────────────────────────────

    /// Delete a task (and its children if PlaylistParent).  Returns the list of
    /// removed task IDs so callers can publish task.removed events.
    pub fn delete_task(&self, task_id: &str) -> Result<Vec<String>> {
        let task = self.load_task(task_id)?;
        let mut removed = Vec::new();

        match task.task_kind {
            TaskKind::PlaylistParent => {
                // Active downloads are stopped by the caller before delete.
                // Any in-flight finalize_download will fail to load the task
                // (file already removed) and bail gracefully.
                let mut all_ids = task.child_task_ids.clone();
                all_ids.push(task.id.clone());
                self.remove_from_queue(&all_ids)?;
                for id in &all_ids {
                    let path = self.task_path(id);
                    if path.exists() {
                        fs::remove_file(&path).with_context(|| {
                            format!("failed to remove task file {}", path.display())
                        })?;
                    }
                    removed.push(id.clone());
                }
            }
            TaskKind::PlaylistItem => {
                if matches!(task.state, TaskState::Downloading | TaskState::Stopping) {
                    bail!("stop the task before deleting it");
                }
                self.remove_from_queue(std::slice::from_ref(&task.id))?;
                let path = self.task_path(&task.id);
                if path.exists() {
                    fs::remove_file(&path).with_context(|| {
                        format!("failed to remove task file {}", path.display())
                    })?;
                }
                removed.push(task.id.clone());

                if let Some(parent_id) = &task.parent_task_id {
                    if let Ok(mut parent) = self.load_task(parent_id) {
                        parent.child_task_ids.retain(|id| id != &task.id);
                        if parent.child_task_ids.is_empty() {
                            self.remove_from_queue(std::slice::from_ref(parent_id))?;
                            let pp = self.task_path(parent_id);
                            if pp.exists() {
                                let _ = fs::remove_file(&pp);
                            }
                            removed.push(parent_id.clone());
                        } else {
                            parent.update_timestamp();
                            self.save_task(&parent)?;
                        }
                    }
                }
            }
            TaskKind::Single => {
                if matches!(task.state, TaskState::Downloading | TaskState::Stopping) {
                    bail!("stop the task before deleting it");
                }
                self.remove_from_queue(std::slice::from_ref(&task.id))?;
                let path = self.task_path(&task.id);
                if path.exists() {
                    fs::remove_file(&path).with_context(|| {
                        format!("failed to remove task file {}", path.display())
                    })?;
                }
                removed.push(task.id.clone());
            }
        }

        Ok(removed)
    }

    // ── Playlist parent state sync ─────────────────────────────────────────────

    /// Recompute each PlaylistParent's state and output_files from its children.
    /// Only saves + returns records that actually changed.
    pub fn refresh_playlist_parents(&self) -> Result<Vec<TaskRecord>> {
        let tasks = self.load_all_tasks()?;
        let by_id: HashMap<String, TaskRecord> =
            tasks.iter().cloned().map(|t| (t.id.clone(), t)).collect();
        let mut changed = Vec::new();

        for task in &tasks {
            if task.task_kind != TaskKind::PlaylistParent {
                continue;
            }
            let children: Vec<&TaskRecord> = task
                .child_task_ids
                .iter()
                .filter_map(|id| by_id.get(id))
                .collect();
            let mut updated = task.clone();
            sync_parent_from_children(&mut updated, &children);
            if updated != *task {
                self.save_task(&updated)?;
                changed.push(updated);
            }
        }
        Ok(changed)
    }

    // ── Library path remapping ─────────────────────────────────────────────────

    pub fn remap_paths(&self, moved: &[crate::library::PathChange]) -> Result<Vec<TaskRecord>> {
        if moved.is_empty() {
            return Ok(Vec::new());
        }
        let mut changed = Vec::new();
        for mut task in self.load_all_tasks()? {
            let mut dirty = false;
            if let Some(new_subdir) = remap_relative_path(&task.target_subdir, moved) {
                if new_subdir != task.target_subdir {
                    task.target_subdir = new_subdir;
                    dirty = true;
                }
            }
            if let Some(new_dir) = remap_relative_path(&task.output_dir_relative, moved) {
                if new_dir != task.output_dir_relative {
                    task.output_dir_relative = new_dir;
                    dirty = true;
                }
            }
            let mut new_files = Vec::with_capacity(task.output_files.len());
            for f in &task.output_files {
                let updated = remap_relative_path(f, moved).unwrap_or_else(|| f.clone());
                if updated != *f { dirty = true; }
                new_files.push(updated);
            }
            if dirty {
                task.output_files = new_files;
                task.update_timestamp();
                self.save_task(&task)?;
                changed.push(task);
            }
        }
        Ok(changed)
    }

    pub fn prune_deleted_paths(&self, deleted: &[String]) -> Result<Vec<TaskRecord>> {
        if deleted.is_empty() {
            return Ok(Vec::new());
        }
        let mut changed = Vec::new();
        for mut task in self.load_all_tasks()? {
            let before = task.output_files.len();
            task.output_files.retain(|f| {
                !deleted.iter().any(|d| is_same_or_descendant(f, d))
            });
            if task.output_files.len() != before {
                task.update_timestamp();
                self.save_task(&task)?;
                changed.push(task);
            }
        }
        Ok(changed)
    }

    // ── Internals ──────────────────────────────────────────────────────────────

    fn ensure_not_duplicate(
        &self,
        extractor: Option<&str>,
        video_id: Option<&str>,
        library_filenames: &HashSet<String>,
    ) -> Result<()> {
        let check = self.find_duplicates(extractor, video_id, library_filenames)?;
        if check.status == "duplicate-found" {
            let hint = check
                .matches
                .first()
                .and_then(|m| m.task_id.clone().or(m.path.clone()))
                .unwrap_or_else(|| "existing entry".to_string());
            bail!("this video already exists: {hint}");
        }
        Ok(())
    }

    fn task_path(&self, task_id: &str) -> PathBuf {
        self.tasks_dir.join(format!("{task_id}.json"))
    }
}

// ── Public helpers ─────────────────────────────────────────────────────────────

pub fn output_directory(settings: &AppSettings, target_subdir: &str) -> PathBuf {
    if target_subdir.trim().is_empty() {
        settings.download_root.clone()
    } else {
        settings.download_root.join(target_subdir)
    }
}

pub fn resolve_from_json(url: &str, value: &Value) -> ResolvedMedia {
    let is_playlist = value
        .get("_type")
        .and_then(Value::as_str)
        .map(|k| k == "playlist")
        .unwrap_or_else(|| value.get("entries").is_some());
    let playlist_count = value
        .get("playlist_count")
        .and_then(Value::as_u64)
        .or_else(|| {
            value
                .get("entries")
                .and_then(Value::as_array)
                .map(|e| e.iter().filter(|v| v.is_object()).count() as u64)
        });
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .map(|e| {
            e.iter()
                .filter_map(|v| resolve_playlist_entry(url, v))
                .collect()
        })
        .unwrap_or_default();

    ResolvedMedia {
        source_url: url.to_string(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled")
            .to_string(),
        extractor: value.get("extractor").and_then(Value::as_str).map(ToOwned::to_owned),
        video_id: value.get("id").and_then(Value::as_str).map(ToOwned::to_owned),
        uploader: value
            .get("uploader")
            .or_else(|| value.get("channel"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        duration_seconds: value.get("duration").and_then(Value::as_u64),
        thumbnail_url: value.get("thumbnail").and_then(Value::as_str).map(ToOwned::to_owned),
        estimated_size_bytes: value
            .get("filesize_approx")
            .or_else(|| value.get("filesize"))
            .and_then(Value::as_u64),
        is_playlist,
        playlist_title: value
            .get("playlist_title")
            .or_else(|| value.get("title"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        playlist_count,
        entries,
        available_qualities: extract_available_qualities(value),
        available_subtitles: extract_available_subtitles(value),
    }
}

// ── Private helpers ────────────────────────────────────────────────────────────

fn make_single_task(request: &CreateTaskRequest) -> TaskRecord {
    let now = Utc::now().to_rfc3339();
    TaskRecord {
        id: new_task_id(),
        task_kind: TaskKind::Single,
        source_url: request.source_url.clone(),
        parent_task_id: None,
        child_task_ids: Vec::new(),
        extractor: request.resolved.extractor.clone(),
        video_id: request.resolved.video_id.clone(),
        title: request.resolved.title.clone(),
        playlist_title: request.resolved.playlist_title.clone(),
        playlist_index: None,
        uploader: request.resolved.uploader.clone(),
        duration_seconds: request.resolved.duration_seconds,
        thumbnail_url: request.resolved.thumbnail_url.clone(),
        estimated_size_bytes: request.resolved.estimated_size_bytes,
        target_subdir: request.target_subdir.clone(),
        output_dir_relative: request.target_subdir.clone(),
        download_mode: request.download_mode.clone(),
        quality: request.quality.clone(),
        container: request.container.clone(),
        subtitle_languages: request.subtitle_languages.clone(),
        cookie_file_name: request.cookie_file_name.clone(),
        raw_args: request.raw_args.clone(),
        raw_format_code: request.raw_format_code.clone(),
        created_at: now.clone(),
        state: TaskState::Queued,
        updated_at: now,
        output_files: Vec::new(),
        last_error: None,
    }
}

fn sync_parent_from_children(parent: &mut TaskRecord, children: &[&TaskRecord]) {
    let total = children.len();
    let completed = children.iter().filter(|c| c.state == TaskState::Completed).count();
    let failed = children.iter().filter(|c| matches!(c.state, TaskState::Failed | TaskState::Canceled)).count();
    let active = children.iter().filter(|c| matches!(c.state, TaskState::Downloading | TaskState::Stopping)).count();
    let queued = children.iter().filter(|c| matches!(c.state, TaskState::Submitted | TaskState::Resolving | TaskState::Queued)).count();
    let paused = children.iter().filter(|c| c.state == TaskState::Paused).count();

    parent.state = if total > 0 && completed == total {
        TaskState::Completed
    } else if active > 0 {
        TaskState::Downloading
    } else if queued > 0 {
        TaskState::Queued
    } else if paused > 0 {
        TaskState::Paused
    } else if failed > 0 {
        TaskState::Failed
    } else {
        TaskState::Submitted
    };

    parent.estimated_size_bytes = children
        .iter()
        .try_fold(0_u64, |sum, c| c.estimated_size_bytes.map(|s| sum + s));
    parent.output_files = children
        .iter()
        .flat_map(|c| c.output_files.iter().cloned())
        .collect();
    parent.last_error = if failed > 0 {
        Some(format!("{failed}/{total} 个子任务失败"))
    } else if completed > 0 && completed < total {
        Some(format!("已完成 {completed}/{total} 个子任务"))
    } else {
        None
    };
    parent.update_timestamp();
}

fn new_task_id() -> String {
    let count = TASK_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("task_{}_{}", Utc::now().timestamp_millis(), count)
}

fn no_duplicate() -> DuplicateCheck {
    DuplicateCheck {
        status: "not_found".to_string(),
        matches: Vec::new(),
        default_action: "skip".to_string(),
        allowed_actions: vec![
            "skip".to_string(),
            "add_anyway".to_string(),
            "open_existing".to_string(),
        ],
    }
}

pub fn resolve_playlist_entry(playlist_url: &str, value: &Value) -> Option<ResolvedPlaylistEntry> {
    let obj = value.as_object()?;
    let extractor = obj.get("extractor").and_then(Value::as_str).map(ToOwned::to_owned);
    let video_id = obj.get("id").and_then(Value::as_str).map(ToOwned::to_owned);
    let source_url = obj
        .get("webpage_url")
        .or_else(|| obj.get("original_url"))
        .or_else(|| obj.get("url"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .or_else(|| {
            if extractor.as_deref().map(|e| e.to_ascii_lowercase().contains("youtube")).unwrap_or(false) {
                video_id.as_ref().map(|id| format!("https://www.youtube.com/watch?v={id}"))
            } else {
                None
            }
        })
        .unwrap_or_else(|| playlist_url.to_string());

    Some(ResolvedPlaylistEntry {
        source_url,
        title: obj.get("title").and_then(Value::as_str).unwrap_or("Untitled").to_string(),
        extractor,
        video_id,
        uploader: obj
            .get("uploader")
            .or_else(|| obj.get("channel"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        duration_seconds: obj.get("duration").and_then(Value::as_u64),
        thumbnail_url: obj.get("thumbnail").and_then(Value::as_str).map(ToOwned::to_owned),
        estimated_size_bytes: obj
            .get("filesize_approx")
            .or_else(|| obj.get("filesize"))
            .and_then(Value::as_u64),
        playlist_index: obj
            .get("playlist_index")
            .or_else(|| obj.get("playlist_autonumber"))
            .and_then(Value::as_u64),
    })
}

fn extract_available_qualities(value: &Value) -> Vec<String> {
    let Some(formats) = value.get("formats").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut heights: Vec<u32> = formats
        .iter()
        .filter_map(|fmt| {
            if fmt.get("vcodec").and_then(Value::as_str).unwrap_or("") == "none" {
                return None;
            }
            fmt.get("height").and_then(Value::as_u64).map(|h| h as u32)
        })
        .filter(|&h| h > 0)
        .collect();
    heights.sort_unstable();
    heights.dedup();
    heights.reverse();
    heights.iter().map(|h| format!("{h}p")).collect()
}

fn extract_available_subtitles(value: &Value) -> Vec<SubtitleOption> {
    let mut options = Vec::new();
    let mut seen = HashSet::new();
    if let Some(obj) = value.get("subtitles").and_then(Value::as_object) {
        for code in obj.keys() {
            if seen.insert(code.clone()) {
                options.push(SubtitleOption { language_code: code.clone(), is_auto: false });
            }
        }
    }
    if let Some(obj) = value.get("automatic_captions").and_then(Value::as_object) {
        for code in obj.keys() {
            if seen.insert(code.clone()) {
                options.push(SubtitleOption { language_code: code.clone(), is_auto: true });
            }
        }
    }
    options.sort_by(|a, b| a.is_auto.cmp(&b.is_auto).then_with(|| a.language_code.cmp(&b.language_code)));
    options
}

fn remap_relative_path(path: &str, moved: &[crate::library::PathChange]) -> Option<String> {
    for change in moved {
        if path == change.from {
            return Some(change.to.clone());
        }
        let prefix = format!("{}/", change.from);
        if let Some(rest) = path.strip_prefix(&prefix) {
            return Some(format!("{}/{rest}", change.to));
        }
    }
    None
}

fn is_same_or_descendant(path: &str, ancestor: &str) -> bool {
    path == ancestor || path.starts_with(&format!("{ancestor}/"))
}


