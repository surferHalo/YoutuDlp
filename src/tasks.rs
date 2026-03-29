use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::settings::AppSettings;
use crate::storage;

static TASK_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProgress {
    pub percent: f64,
    pub downloaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_sec: Option<u64>,
    pub eta_seconds: Option<u64>,
}

impl Default for TaskProgress {
    fn default() -> Self {
        Self {
            percent: 0.0,
            downloaded_bytes: None,
            total_bytes: None,
            speed_bytes_per_sec: None,
            eta_seconds: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: String,
    pub source_url: String,
    pub state: TaskState,
    pub extractor: Option<String>,
    pub video_id: Option<String>,
    pub title: String,
    pub uploader: Option<String>,
    pub duration_seconds: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub estimated_size_bytes: Option<u64>,
    pub target_subdir: String,
    pub output_dir_relative: String,
    pub download_mode: String,
    pub quality: String,
    pub container: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_error: Option<String>,
    pub output_files: Vec<String>,
    pub recent_logs: Vec<String>,
    pub progress: TaskProgress,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QueueState {
    pub task_order: Vec<String>,
    pub updated_at: String,
}

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
    pub resolved: ResolvedMedia,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskListResponse {
    pub tasks: Vec<TaskRecord>,
}

#[derive(Debug, Clone)]
pub struct TaskStore {
    tasks_dir: PathBuf,
    queue_path: PathBuf,
}

impl TaskRecord {
    pub fn new(request: &CreateTaskRequest) -> Self {
        let now = Utc::now().to_rfc3339();
        let output_dir_relative = request.target_subdir.clone();

        Self {
            id: new_task_id(),
            source_url: request.source_url.clone(),
            state: TaskState::Queued,
            extractor: request.resolved.extractor.clone(),
            video_id: request.resolved.video_id.clone(),
            title: request.resolved.title.clone(),
            uploader: request.resolved.uploader.clone(),
            duration_seconds: request.resolved.duration_seconds,
            thumbnail_url: request.resolved.thumbnail_url.clone(),
            estimated_size_bytes: request.resolved.estimated_size_bytes,
            target_subdir: request.target_subdir.clone(),
            output_dir_relative,
            download_mode: request.download_mode.clone(),
            quality: request.quality.clone(),
            container: request.container.clone(),
            created_at: now.clone(),
            updated_at: now,
            last_error: None,
            output_files: Vec::new(),
            recent_logs: Vec::new(),
            progress: TaskProgress::default(),
        }
    }

    pub fn update_timestamp(&mut self) {
        self.updated_at = Utc::now().to_rfc3339();
    }

    pub fn push_log(&mut self, line: String) {
        self.recent_logs.push(line);
        if self.recent_logs.len() > 80 {
            let drain_count = self.recent_logs.len() - 80;
            self.recent_logs.drain(0..drain_count);
        }
        self.update_timestamp();
    }

    pub fn reset_for_restart(&mut self) {
        self.state = TaskState::Queued;
        self.progress = TaskProgress::default();
        self.last_error = None;
        self.output_files.clear();
        self.recent_logs.clear();
        self.update_timestamp();
    }
}

impl QueueState {
    pub fn empty() -> Self {
        Self {
            task_order: Vec::new(),
            updated_at: Utc::now().to_rfc3339(),
        }
    }
}

impl TaskStore {
    pub fn new(tasks_dir: PathBuf, queue_path: PathBuf) -> Self {
        Self {
            tasks_dir,
            queue_path,
        }
    }

    pub fn load_or_create_queue(&self) -> Result<QueueState> {
        if !self.queue_path.exists() {
            let queue = QueueState::empty();
            self.save_queue(&queue)?;
            return Ok(queue);
        }

        let raw = fs::read(&self.queue_path)
            .with_context(|| format!("failed to read queue file {}", self.queue_path.display()))?;
        let queue = serde_json::from_slice::<QueueState>(&raw)
            .with_context(|| format!("failed to parse queue file {}", self.queue_path.display()))?;
        Ok(queue)
    }

    pub fn save_queue(&self, queue: &QueueState) -> Result<()> {
        storage::write_json_pretty_atomic(&self.queue_path, queue)
    }

    pub fn load_task(&self, task_id: &str) -> Result<TaskRecord> {
        let path = self.task_path(task_id);
        let raw = fs::read(&path)
            .with_context(|| format!("failed to read task file {}", path.display()))?;
        serde_json::from_slice::<TaskRecord>(&raw)
            .with_context(|| format!("failed to parse task file {}", path.display()))
    }

    pub fn save_task(&self, task: &TaskRecord) -> Result<()> {
        storage::write_json_pretty_atomic(&self.task_path(&task.id), task)
    }

    pub fn load_all_tasks(&self) -> Result<Vec<TaskRecord>> {
        let mut tasks = Vec::new();
        if !self.tasks_dir.exists() {
            return Ok(tasks);
        }

        for entry in fs::read_dir(&self.tasks_dir).with_context(|| {
            format!(
                "failed to read tasks directory {}",
                self.tasks_dir.display()
            )
        })? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }

            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }

            let raw = fs::read(entry.path())?;
            let task = serde_json::from_slice::<TaskRecord>(&raw)?;
            tasks.push(task);
        }

        Ok(tasks)
    }

    pub fn list_tasks_in_queue_order(&self) -> Result<Vec<TaskRecord>> {
        let queue = self.load_or_create_queue()?;
        let mut tasks = self.load_all_tasks()?;
        tasks.sort_by_key(|task| {
            queue
                .task_order
                .iter()
                .position(|task_id| task_id == &task.id)
                .unwrap_or(usize::MAX)
        });
        Ok(tasks)
    }

    pub fn append_to_queue(&self, task_id: &str) -> Result<QueueState> {
        let mut queue = self.load_or_create_queue()?;
        if !queue.task_order.iter().any(|item| item == task_id) {
            queue.task_order.push(task_id.to_string());
            queue.updated_at = Utc::now().to_rfc3339();
            self.save_queue(&queue)?;
        }
        Ok(queue)
    }

    pub fn find_first_queued_task_excluding(
        &self,
        excluded_ids: &[String],
    ) -> Result<Option<TaskRecord>> {
        let queue = self.load_or_create_queue()?;
        for task_id in queue.task_order {
            if excluded_ids.iter().any(|excluded| excluded == &task_id) {
                continue;
            }

            let task = match self.load_task(&task_id) {
                Ok(task) => task,
                Err(_) => continue,
            };

            if matches!(task.state, TaskState::Queued) {
                return Ok(Some(task));
            }
        }

        Ok(None)
    }

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

    pub fn find_duplicates(
        &self,
        extractor: Option<&str>,
        video_id: Option<&str>,
        download_root: &Path,
    ) -> Result<DuplicateCheck> {
        let Some(extractor) = extractor else {
            return Ok(no_duplicate());
        };
        let Some(video_id) = video_id else {
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

        scan_info_json_for_duplicates(download_root, extractor, video_id, &mut matches)?;

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

    pub fn create_task_from_request(&self, request: &CreateTaskRequest) -> Result<TaskRecord> {
        if request.resolved.is_playlist {
            bail!("playlist task creation is not available in M2 yet");
        }

        let task = TaskRecord::new(request);
        self.save_task(&task)?;
        self.append_to_queue(&task.id)?;
        Ok(task)
    }

    fn task_path(&self, task_id: &str) -> PathBuf {
        self.tasks_dir.join(format!("{task_id}.json"))
    }
}

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
        .map(|kind| kind == "playlist")
        .unwrap_or_else(|| value.get("entries").is_some());
    let playlist_count = value
        .get("playlist_count")
        .and_then(Value::as_u64)
        .or_else(|| {
            value
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| entries.len() as u64)
        });

    ResolvedMedia {
        source_url: url.to_string(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled")
            .to_string(),
        extractor: value
            .get("extractor")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        video_id: value
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        uploader: value
            .get("uploader")
            .or_else(|| value.get("channel"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        duration_seconds: value.get("duration").and_then(Value::as_u64),
        thumbnail_url: value
            .get("thumbnail")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
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
    }
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

fn scan_info_json_for_duplicates(
    root: &Path,
    extractor: &str,
    video_id: &str,
    matches: &mut Vec<DuplicateMatch>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(root)
        .with_context(|| format!("failed to scan download root {}", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            scan_info_json_for_duplicates(&path, extractor, video_id, matches)?;
            continue;
        }

        let is_info_json = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(".info.json"))
            .unwrap_or(false);
        if !is_info_json {
            continue;
        }

        let raw = fs::read(&path)?;
        let value = match serde_json::from_slice::<Value>(&raw) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let candidate_extractor = value.get("extractor").and_then(Value::as_str);
        let candidate_id = value.get("id").and_then(Value::as_str);
        if candidate_extractor == Some(extractor) && candidate_id == Some(video_id) {
            matches.push(DuplicateMatch {
                source: "library".to_string(),
                path: Some(path.to_string_lossy().to_string()),
                task_id: None,
                state: None,
            });
        }
    }

    Ok(())
}
