use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::sleep;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::app::AppState;
use crate::cookies::{ImportCookiesRequest, ImportCookiesResponse, import_cookies};
use crate::downloads;
use crate::events::{self, ServerEvent};
use crate::library;
use crate::library::{
    CreateFolderRequest, DeleteItemsRequest, LibraryMutationResponse, LibraryOpenResponse,
    LibraryTreeResponse, MoveItemsRequest, OpenPathRequest, PathChange, RenameItemRequest,
};
use crate::settings::AppSettings;
use crate::tasks::{
    BatchCreateTasksResponse, CreatePlaylistTasksRequest, CreateTaskRequest, ResolveRequest,
    ResolveResponse, ResolvedPlaylistEntry, TaskKind, TaskListResponse, TaskRecord, TaskState,
    TaskView, resolve_from_json, resolve_playlist_entry,
};
use crate::tools::ToolRegistry;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/app/status", get(app_status))
        .route("/api/settings", get(get_settings).put(update_settings))
        .route("/api/events", get(events_stream))
        .route("/api/cookies/import", post(import_cookie_file))
        .route("/api/resolve", axum::routing::post(resolve_url))
        .route("/api/tasks", get(list_tasks).post(create_task))
        .route("/api/tasks/batch", post(create_playlist_tasks))
        .route("/api/library/tree", get(get_library_tree))
        .route("/api/library/folders", post(create_library_folder))
        .route("/api/library/move", post(move_library_items))
        .route("/api/library/items", delete(delete_library_items))
        .route("/api/library/open-item", post(open_library_item))
        .route("/api/library/open-folder", post(open_library_folder))
        .route("/api/library/rename", post(rename_library_folder))
        .route("/api/tasks/{task_id}/stop", axum::routing::post(stop_task))
        .route(
            "/api/tasks/{task_id}/resume",
            axum::routing::post(resume_task),
        )
        .route(
            "/api/tasks/{task_id}/restart",
            axum::routing::post(restart_task),
        )
        .route("/api/tasks/{task_id}", delete(delete_task))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

async fn app_status(State(state): State<Arc<AppState>>) -> Json<AppStatusResponse> {
    let settings = state.settings.read().await.clone();
    let tools = state.tools.read().await.clone();

    Json(AppStatusResponse {
        app_name: "YoutuDlp Bridge",
        version: env!("CARGO_PKG_VERSION"),
        base_url: state.startup.base_url.clone(),
        started_at: state.startup.started_at.clone(),
        first_launch: state.startup.is_first_launch,
        executable_dir: state.startup.executable_dir.clone(),
        state_dir: state.paths.state_root.clone(),
        queue_path: state.paths.queue_path.clone(),
        download_root: settings.download_root.clone(),
        active_download_count: state
            .active_downloads
            .lock()
            .expect("active download lock poisoned")
            .len(),
        tools,
    })
}

async fn get_settings(State(state): State<Arc<AppState>>) -> Json<AppSettings> {
    Json(state.settings.read().await.clone())
}

async fn events_stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = BroadcastStream::new(state.event_tx.subscribe()).filter_map(|message| {
        let Ok(event) = message else {
            return None;
        };

        let data = serde_json::to_string(&event).ok()?;
        Some(Ok(Event::default().event(event.event).data(data)))
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}

async fn update_settings(
    State(state): State<Arc<AppState>>,
    Json(settings): Json<AppSettings>,
) -> Result<Json<AppSettings>, ApiError> {
    settings
        .validate()
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    std::fs::create_dir_all(&settings.download_root).map_err(|error| {
        ApiError::bad_request(format!(
            "failed to create or access download root {}: {error}",
            settings.download_root.display()
        ))
    })?;

    state
        .settings_store
        .save(&settings)
        .map_err(ApiError::internal)?;

    let tools = ToolRegistry::discover(&state.paths.executable_dir, &settings.tool_overrides);

    {
        let mut current_settings = state.settings.write().await;
        *current_settings = settings.clone();
    }

    {
        let mut current_tools = state.tools.write().await;
        *current_tools = tools;
    }

    events::publish(&state.event_tx, ServerEvent::settings_changed(&settings));
    events::publish(
        &state.event_tx,
        ServerEvent::library_changed("settings_changed", &Vec::<String>::new()),
    );

    Ok(Json(settings))
}

async fn resolve_url(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<ResolveResponse>, ApiError> {
    if request.url.trim().is_empty() {
        return Err(ApiError::bad_request("URL cannot be empty"));
    }

    let yt_dlp_path = state
        .tools
        .read()
        .await
        .yt_dlp
        .path
        .clone()
        .ok_or_else(|| ApiError::bad_request("yt-dlp is not configured"))?;

    // ── Step 1: flat-playlist scan (fast) ─────────────────────────────────────
    // Detects playlist vs single video without fetching per-entry metadata.
    let flat_output = Command::new(&yt_dlp_path)
        .arg("--flat-playlist")
        .arg("--dump-single-json")
        .arg("--skip-download")
        .arg("--no-warnings")
        .arg(&request.url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(ApiError::internal)?;

    if !flat_output.status.success() {
        let message = String::from_utf8_lossy(&flat_output.stderr).trim().to_string();
        return Err(ApiError::bad_request(if message.is_empty() {
            "yt-dlp resolve failed".to_string()
        } else {
            message
        }));
    }

    let flat_value =
        serde_json::from_slice::<Value>(&flat_output.stdout).map_err(ApiError::internal)?;

    let is_playlist = flat_value
        .get("_type")
        .and_then(Value::as_str)
        .map(|t| t == "playlist")
        .unwrap_or_else(|| flat_value.get("entries").is_some());

    if !is_playlist {
        // Single video: --flat-playlist produces identical output, use directly.
        let resolved = resolve_from_json(&request.url, &flat_value);
        let filenames = state.library_filenames.read().await;
        let duplicate_check = state
            .task_store
            .find_duplicates(
                resolved.extractor.as_deref(),
                resolved.video_id.as_deref(),
                &*filenames,
            )
            .map_err(ApiError::internal)?;
        return Ok(Json(ResolveResponse {
            resolved,
            duplicate_check,
        }));
    }

    // ── Step 2: concurrent entry resolution ───────────────────────────────────
    let flat_entries: Vec<Value> = flat_value
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let (concurrency, interval_ms) = {
        let s = state.settings.read().await;
        (s.playlist_resolve_concurrency as usize, s.playlist_resolve_interval_ms)
    };

    eprintln!(
        "[resolve] playlist: {} entries, concurrency={concurrency}, interval={interval_ms}ms",
        flat_entries.len()
    );

    let semaphore = Arc::new(Semaphore::new(concurrency));
    // Each slot holds either a spawned resolve task or a pre-built fallback entry.
    let mut handles: Vec<tokio::task::JoinHandle<ResolvedPlaylistEntry>> =
        Vec::with_capacity(flat_entries.len());

    for (i, flat_entry) in flat_entries.into_iter().enumerate() {
        if i > 0 && interval_ms > 0 {
            sleep(Duration::from_millis(interval_ms)).await;
        }

        // Derive the entry URL the same way resolve_playlist_entry does.
        let entry_url = entry_url_from_flat(&request.url, &flat_entry);

        let permit = Arc::clone(&semaphore)
            .acquire_owned()
            .await
            .expect("semaphore closed");

        let yt_dlp = yt_dlp_path.clone();
        let playlist_url = request.url.clone();
        let idx = i;

        let handle = tokio::spawn(async move {
            eprintln!("[resolve] entry {}: {entry_url}", idx + 1);
            let resolved = match run_entry_resolve(&yt_dlp, &entry_url).await {
                Ok(value) => resolve_playlist_entry(&playlist_url, &value)
                    .unwrap_or_else(|| flat_entry_fallback(&flat_entry, &entry_url)),
                Err(err) => {
                    eprintln!("[resolve] entry {} failed: {err}", idx + 1);
                    flat_entry_fallback(&flat_entry, &entry_url)
                }
            };
            drop(permit);
            resolved
        });
        handles.push(handle);
    }

    let mut entries: Vec<ResolvedPlaylistEntry> = Vec::with_capacity(handles.len());
    for handle in handles {
        let entry = handle
            .await
            .map_err(|e| ApiError::internal(anyhow::anyhow!("task join error: {e}")))?;
        entries.push(entry);
    }

    let mut resolved = resolve_from_json(&request.url, &flat_value);
    resolved.entries = entries;

    let filenames = state.library_filenames.read().await;
    let duplicate_check = state
        .task_store
        .find_duplicates(
            resolved.extractor.as_deref(),
            resolved.video_id.as_deref(),
            &*filenames,
        )
        .map_err(ApiError::internal)?;

    Ok(Json(ResolveResponse {
        resolved,
        duplicate_check,
    }))
}

/// Extracts the canonical URL for a flat-playlist entry.
fn entry_url_from_flat(playlist_url: &str, flat: &Value) -> String {
    let extractor = flat
        .get("ie_key")
        .or_else(|| flat.get("extractor"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let video_id = flat.get("id").and_then(Value::as_str);

    flat.get("webpage_url")
        .or_else(|| flat.get("url"))
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        .map(ToOwned::to_owned)
        .or_else(|| {
            if extractor.contains("youtube") {
                video_id.map(|id| format!("https://www.youtube.com/watch?v={id}"))
            } else {
                None
            }
        })
        .unwrap_or_else(|| playlist_url.to_string())
}

/// Runs yt-dlp `--dump-single-json` on a single entry URL and returns the parsed JSON.
async fn run_entry_resolve(
    yt_dlp_path: &std::path::Path,
    url: &str,
) -> Result<Value, anyhow::Error> {
    let output = Command::new(yt_dlp_path)
        .arg("--dump-single-json")
        .arg("--skip-download")
        .arg("--no-warnings")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!("yt-dlp failed: {msg}");
    }
    Ok(serde_json::from_slice::<Value>(&output.stdout)?)
}

/// Builds a `ResolvedPlaylistEntry` from flat-playlist data when individual
/// resolution fails or is unavailable.
fn flat_entry_fallback(flat: &Value, fallback_url: &str) -> ResolvedPlaylistEntry {
    ResolvedPlaylistEntry {
        source_url: flat
            .get("webpage_url")
            .or_else(|| flat.get("url"))
            .and_then(Value::as_str)
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| fallback_url.to_string()),
        title: flat
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled")
            .to_string(),
        extractor: flat
            .get("ie_key")
            .or_else(|| flat.get("extractor"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        video_id: flat.get("id").and_then(Value::as_str).map(ToOwned::to_owned),
        uploader: flat
            .get("uploader")
            .or_else(|| flat.get("channel"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        duration_seconds: flat.get("duration").and_then(Value::as_u64),
        thumbnail_url: flat
            .get("thumbnail")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        estimated_size_bytes: flat
            .get("filesize_approx")
            .or_else(|| flat.get("filesize"))
            .and_then(Value::as_u64),
        playlist_index: flat
            .get("playlist_index")
            .or_else(|| flat.get("playlist_autonumber"))
            .and_then(Value::as_u64),
    }
}

async fn import_cookie_file(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ImportCookiesRequest>,
) -> Result<Json<ImportCookiesResponse>, ApiError> {
    let response = import_cookies(&state.paths.runtime_dir, &request)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(response))
}

async fn list_tasks(
    State(state): State<Arc<AppState>>,
) -> Result<Json<TaskListResponse>, ApiError> {
    let tasks = state
        .task_store
        .list_tasks_in_queue_order()
        .map_err(ApiError::internal)?;
    let rt = state.runtime_states.read().await;
    let views = tasks.iter().map(|t| TaskView::from_record(t, rt.get(&t.id))).collect();
    Ok(Json(TaskListResponse { tasks: views }))
}

async fn get_library_tree(
    State(state): State<Arc<AppState>>,
) -> Result<Json<LibraryTreeResponse>, ApiError> {
    let download_root = state.settings.read().await.download_root.clone();
    let tree = library::build_tree(&download_root).map_err(ApiError::internal)?;
    Ok(Json(tree))
}

async fn create_library_folder(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateFolderRequest>,
) -> Result<Json<LibraryMutationResponse>, ApiError> {
    let download_root = state.settings.read().await.download_root.clone();
    let response = library::create_folder(&download_root, &request)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    *state.library_filenames.write().await = library::collect_filenames(&download_root);
    publish_library_changed(&state, &response.changed_paths);
    Ok(Json(response))
}

async fn move_library_items(
    State(state): State<Arc<AppState>>,
    Json(request): Json<MoveItemsRequest>,
) -> Result<Json<LibraryMutationResponse>, ApiError> {
    ensure_paths_not_in_active_tasks(&state, &request.source_paths)?;

    let download_root = state.settings.read().await.download_root.clone();
    let response = library::move_items(&download_root, &request)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    let changed_tasks = remap_task_paths(&state, &response.moved)?;
    let changed_parents = refresh_playlist_parents(&state)?;
    *state.library_filenames.write().await = library::collect_filenames(&download_root);
    publish_library_changed(&state, &response.changed_paths);
    publish_task_views(&state, &changed_tasks);
    publish_task_views(&state, &changed_parents);
    Ok(Json(response))
}

async fn delete_library_items(
    State(state): State<Arc<AppState>>,
    Json(request): Json<DeleteItemsRequest>,
) -> Result<Json<LibraryMutationResponse>, ApiError> {
    ensure_paths_not_in_active_tasks(&state, &request.target_paths)?;

    let download_root = state.settings.read().await.download_root.clone();
    let response = library::delete_items(&download_root, &request)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    let changed_tasks = prune_deleted_task_paths(&state, &response.deleted)?;
    let changed_parents = refresh_playlist_parents(&state)?;
    *state.library_filenames.write().await = library::collect_filenames(&download_root);
    publish_library_changed(&state, &response.changed_paths);
    publish_task_views(&state, &changed_tasks);
    publish_task_views(&state, &changed_parents);
    Ok(Json(response))
}

async fn open_library_item(
    State(state): State<Arc<AppState>>,
    Json(request): Json<OpenPathRequest>,
) -> Result<Json<LibraryOpenResponse>, ApiError> {
    let download_root = state.settings.read().await.download_root.clone();
    let response = library::open_item(&download_root, &request)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(response))
}

async fn open_library_folder(
    State(state): State<Arc<AppState>>,
    Json(request): Json<OpenPathRequest>,
) -> Result<Json<LibraryOpenResponse>, ApiError> {
    let download_root = state.settings.read().await.download_root.clone();
    let response = library::open_folder(&download_root, &request)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(response))
}

async fn rename_library_folder(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RenameItemRequest>,
) -> Result<Json<LibraryMutationResponse>, ApiError> {
    let download_root = state.settings.read().await.download_root.clone();
    let response = library::rename_item(&download_root, &request)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    let changed_tasks = remap_task_paths(&state, &response.moved)?;
    let changed_parents = refresh_playlist_parents(&state)?;
    *state.library_filenames.write().await = library::collect_filenames(&download_root);
    publish_library_changed(&state, &response.changed_paths);
    publish_task_views(&state, &changed_tasks);
    publish_task_views(&state, &changed_parents);
    Ok(Json(response))
}

async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(mut request): Json<CreateTaskRequest>,
) -> Result<Json<TaskView>, ApiError> {
    let settings = state.settings.read().await.clone();
    if request.cookie_file_name.is_none() {
        request.cookie_file_name = settings.default_cookie_file_name.clone();
    }
    let filenames = state.library_filenames.read().await;
    let task = state
        .task_store
        .create_task_from_request(&request, &*filenames)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    let view = TaskView::from_record(&task, None);
    events::publish(&state.event_tx, ServerEvent::task_updated(&view));

    downloads::schedule_pending(state.clone())
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(view))
}

async fn create_playlist_tasks(
    State(state): State<Arc<AppState>>,
    Json(mut request): Json<CreatePlaylistTasksRequest>,
) -> Result<Json<BatchCreateTasksResponse>, ApiError> {
    let settings = state.settings.read().await.clone();
    if request.cookie_file_name.is_none() {
        request.cookie_file_name = settings.default_cookie_file_name.clone();
    }
    let filenames = state.library_filenames.read().await;
    let response = state
        .task_store
        .create_playlist_tasks_from_request(&request, &*filenames)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    let changed_parents = refresh_playlist_parents(&state)?;
    publish_task_views(&state, &response.tasks);
    publish_task_views(&state, &changed_parents);

    downloads::schedule_pending(state.clone())
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(response))
}

async fn stop_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<Json<TaskView>, ApiError> {
    let task = downloads::stop_task(state.clone(), &task_id)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let rt = state.runtime_states.read().await;
    Ok(Json(TaskView::from_record(&task, rt.get(&task.id))))
}

async fn resume_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<Json<TaskView>, ApiError> {
    let task = downloads::resume_task(state.clone(), &task_id)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let rt = state.runtime_states.read().await;
    Ok(Json(TaskView::from_record(&task, rt.get(&task.id))))
}

async fn restart_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<Json<TaskView>, ApiError> {
    let task = downloads::restart_task(state.clone(), &task_id)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let rt = state.runtime_states.read().await;
    Ok(Json(TaskView::from_record(&task, rt.get(&task.id))))
}

async fn delete_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<StatusCode, ApiError> {
    let download_root = state.settings.read().await.download_root.clone();

    // Load task info *before* deletion so we have it for cleanup.
    let task = state
        .task_store
        .load_task(&task_id)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    // ---------- Kill all relevant yt-dlp processes ----------
    let child_ids: Vec<String> = if task.task_kind == TaskKind::PlaylistParent {
        task.child_task_ids.clone()
    } else {
        vec![task_id.clone()]
    };

    // Step 1: Remove ALL children from the download queue so the scheduler
    // cannot start any new downloads while we are cleaning up.
    let _ = state.task_store.remove_from_queue(&child_ids);

    // Step 2: Kill & wait — run TWO passes to catch any downloads the
    // scheduler may have started between us loading the task and draining
    // the queue (race window).
    for _pass in 0..2 {
        let done_rxs: Vec<_> = {
            let mut active = state.active_downloads.lock().expect("lock poisoned");
            child_ids
                .iter()
                .filter_map(|id| active.remove(id))
                .map(|dl| {
                    let _ = dl.stop_tx.send(());
                    dl.process_done
                })
                .collect()
        };
        if done_rxs.is_empty() {
            break;
        }
        for rx in done_rxs {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                rx,
            )
            .await;
        }
        // Brief yield so any just-started waiter tasks can register.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Collect non-completed tasks that need partial file cleanup.
    // Completed tasks have real files the user wants to keep.
    let tasks_to_clean: Vec<TaskRecord> = if task.task_kind == TaskKind::PlaylistParent {
        task.child_task_ids
            .iter()
            .filter_map(|id| state.task_store.load_task(id).ok())
            .filter(|c| !matches!(c.state, TaskState::Completed))
            .collect()
    } else if matches!(task.state, TaskState::Completed) {
        vec![]
    } else {
        vec![task]
    };

    let removed_ids = state
        .task_store
        .delete_task(&task_id)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    // Remove in-memory runtime states for deleted tasks.
    {
        let mut rt = state.runtime_states.write().await;
        for id in &removed_ids {
            rt.remove(id);
        }
    }

    // Clean up any partial/temp files left in the download directory.
    for t in &tasks_to_clean {
        downloads::ensure_download_processes_stopped(&download_root, t).await;
        downloads::delete_related_partial_files(&download_root, t).await;
    }

    let changed_parents = refresh_playlist_parents(&state)?;
    publish_task_views(&state, &changed_parents);
    for removed_id in removed_ids {
        events::publish(&state.event_tx, ServerEvent::task_removed(&removed_id));
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppStatusResponse {
    app_name: &'static str,
    version: &'static str,
    base_url: String,
    started_at: String,
    first_launch: bool,
    executable_dir: PathBuf,
    state_dir: PathBuf,
    queue_path: PathBuf,
    download_root: PathBuf,
    active_download_count: usize,
    tools: ToolRegistry,
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "BAD_REQUEST",
            message: message.into(),
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "INTERNAL_ERROR",
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}

fn publish_library_changed(state: &Arc<AppState>, changed_paths: &[String]) {
    events::publish(
        &state.event_tx,
        ServerEvent::library_changed("manual_operation", changed_paths),
    );
}

fn publish_task_views(state: &Arc<AppState>, views: &[TaskView]) {
    for view in views {
        events::publish(&state.event_tx, ServerEvent::task_updated(view));
    }
}

fn refresh_playlist_parents(state: &Arc<AppState>) -> Result<Vec<TaskView>, ApiError> {
    let parents = state
        .task_store
        .refresh_playlist_parents()
        .map_err(ApiError::internal)?;
    Ok(parents.iter().map(|p| TaskView::from_record(p, None)).collect())
}

fn ensure_paths_not_in_active_tasks(
    state: &Arc<AppState>,
    target_paths: &[String],
) -> Result<(), ApiError> {
    let tasks = state.task_store.load_all_tasks().map_err(ApiError::internal)?;
    let blocked_by = tasks
        .iter()
        .filter(|task| is_protected_task_state(&task.state))
        .find(|task| target_paths.iter().any(|path| path_intersects_task(path, task)));

    if let Some(task) = blocked_by {
        return Err(ApiError::bad_request(format!(
            "所选条目仍被任务占用: {} ({})",
            task.title, task.id
        )));
    }

    Ok(())
}

fn remap_task_paths(state: &Arc<AppState>, moved: &[PathChange]) -> Result<Vec<TaskView>, ApiError> {
    let changed = state.task_store.remap_paths(moved).map_err(ApiError::internal)?;
    Ok(changed.iter().map(|t| TaskView::from_record(t, None)).collect())
}

fn prune_deleted_task_paths(
    state: &Arc<AppState>,
    deleted: &[String],
) -> Result<Vec<TaskView>, ApiError> {
    let changed = state.task_store.prune_deleted_paths(deleted).map_err(ApiError::internal)?;
    Ok(changed.iter().map(|t| TaskView::from_record(t, None)).collect())
}

fn path_intersects_task(path: &str, task: &TaskRecord) -> bool {
    if is_same_or_descendant(&task.target_subdir, path)
        || is_same_or_descendant(&task.output_dir_relative, path)
    {
        return true;
    }

    task.output_files
        .iter()
        .any(|output_file| is_same_or_descendant(output_file, path))
}

fn is_same_or_descendant(path: &str, ancestor: &str) -> bool {
    if path.is_empty() || ancestor.is_empty() {
        return false;
    }

    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn is_protected_task_state(state: &TaskState) -> bool {
    !matches!(state, TaskState::Completed | TaskState::Failed | TaskState::Canceled)
}
