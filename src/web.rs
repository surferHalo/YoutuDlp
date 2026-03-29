use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;

use crate::app::AppState;
use crate::downloads;
use crate::settings::AppSettings;
use crate::tasks::{
    CreateTaskRequest, ResolveRequest, ResolveResponse, TaskListResponse, resolve_from_json,
};
use crate::tools::ToolRegistry;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/app/status", get(app_status))
        .route("/api/settings", get(get_settings).put(update_settings))
        .route("/api/resolve", axum::routing::post(resolve_url))
        .route("/api/tasks", get(list_tasks).post(create_task))
        .route("/api/tasks/{task_id}/stop", axum::routing::post(stop_task))
        .route(
            "/api/tasks/{task_id}/resume",
            axum::routing::post(resume_task),
        )
        .route(
            "/api/tasks/{task_id}/restart",
            axum::routing::post(restart_task),
        )
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
        tools,
    })
}

async fn get_settings(State(state): State<Arc<AppState>>) -> Json<AppSettings> {
    Json(state.settings.read().await.clone())
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

    let output = Command::new(&yt_dlp_path)
        .arg("--dump-single-json")
        .arg("--skip-download")
        .arg("--no-warnings")
        .arg(&request.url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(ApiError::internal)?;

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ApiError::bad_request(if message.is_empty() {
            "yt-dlp resolve failed".to_string()
        } else {
            message
        }));
    }

    let value = serde_json::from_slice::<Value>(&output.stdout).map_err(ApiError::internal)?;
    let resolved = resolve_from_json(&request.url, &value);
    let settings = state.settings.read().await.clone();
    let duplicate_check = state
        .task_store
        .find_duplicates(
            resolved.extractor.as_deref(),
            resolved.video_id.as_deref(),
            &settings.download_root,
        )
        .map_err(ApiError::internal)?;

    Ok(Json(ResolveResponse {
        resolved,
        duplicate_check,
    }))
}

async fn list_tasks(
    State(state): State<Arc<AppState>>,
) -> Result<Json<TaskListResponse>, ApiError> {
    let tasks = state
        .task_store
        .list_tasks_in_queue_order()
        .map_err(ApiError::internal)?;
    Ok(Json(TaskListResponse { tasks }))
}

async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateTaskRequest>,
) -> Result<Json<crate::tasks::TaskRecord>, ApiError> {
    let task = state
        .task_store
        .create_task_from_request(&request)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;

    downloads::schedule_pending(state.clone())
        .await
        .map_err(ApiError::internal)?;

    Ok(Json(task))
}

async fn stop_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<Json<crate::tasks::TaskRecord>, ApiError> {
    let task = downloads::stop_task(state, &task_id)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(task))
}

async fn resume_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<Json<crate::tasks::TaskRecord>, ApiError> {
    let task = downloads::resume_task(state, &task_id)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(task))
}

async fn restart_task(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Result<Json<crate::tasks::TaskRecord>, ApiError> {
    let task = downloads::restart_task(state, &task_id)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(task))
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
