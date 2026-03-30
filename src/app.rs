use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::downloads;
use crate::events::{self, ServerEvent};
use crate::library;
use crate::settings::{AppSettings, SettingsStore};
use crate::storage::AppPaths;
use crate::tasks::{RuntimeState, TaskStore};
use crate::tools::ToolRegistry;
use crate::web;

const DEFAULT_BIND_ADDRESS: &str = "127.0.0.1:39035";

pub struct AppState {
    pub paths: AppPaths,
    pub settings_store: SettingsStore,
    pub task_store: TaskStore,
    pub settings: RwLock<AppSettings>,
    pub tools: RwLock<ToolRegistry>,
    pub active_downloads: Mutex<HashMap<String, ActiveDownload>>,
    pub runtime_states: RwLock<HashMap<String, RuntimeState>>,
    pub library_filenames: RwLock<HashSet<String>>,
    pub scheduler_running: AtomicBool,
    pub event_tx: tokio::sync::broadcast::Sender<ServerEvent>,
    pub startup: StartupInfo,
}

#[derive(Debug)]
pub struct ActiveDownload {
    pub stop_tx: tokio::sync::mpsc::UnboundedSender<()>,
    /// OS process ID of the yt-dlp child process.
    pub pid: Option<u32>,
    /// Fires exactly once after `child.wait()` completes, guaranteeing the OS
    /// has released all file handles held by the process.
    pub process_done: tokio::sync::oneshot::Receiver<()>,
}

#[derive(Debug, Clone)]
pub struct StartupInfo {
    pub started_at: String,
    pub base_url: String,
    pub is_first_launch: bool,
    pub executable_dir: PathBuf,
}

pub async fn run() -> Result<()> {
    init_tracing();

    let paths = AppPaths::discover()?;
    paths.ensure_state_dirs()?;

    let settings_store = SettingsStore::new(paths.settings_path.clone());
    let (settings, is_first_launch) = settings_store.load_or_create_default()?;
    let task_store = TaskStore::new(paths.tasks_dir.clone(), paths.queue_path.clone());
    task_store.load_or_create_queue()?;
    task_store.recover_on_startup()?;
    let tools = ToolRegistry::discover(&paths.executable_dir, &settings.tool_overrides);

    if !tools.yt_dlp.available {
        warn!("yt-dlp was not found beside the bridge executable or in configured overrides");
    }

    let listener = TcpListener::bind(DEFAULT_BIND_ADDRESS)
        .await
        .with_context(|| format!("failed to bind local HTTP server at {DEFAULT_BIND_ADDRESS}"))?;
    let base_url = format!("http://{}", listener.local_addr()?);
    let (event_tx, _) = tokio::sync::broadcast::channel(256);

    let state = Arc::new(AppState {
        paths: paths.clone(),
        settings_store,
        task_store,
        settings: RwLock::new(settings.clone()),
        tools: RwLock::new(tools),
        active_downloads: Mutex::new(HashMap::new()),
        runtime_states: RwLock::new(HashMap::new()),
        library_filenames: RwLock::new(library::collect_filenames(&settings.download_root)),
        scheduler_running: AtomicBool::new(false),
        event_tx,
        startup: StartupInfo {
            started_at: Utc::now().to_rfc3339(),
            base_url: base_url.clone(),
            is_first_launch,
            executable_dir: paths.executable_dir.clone(),
        },
    });

    events::publish(&state.event_tx, ServerEvent::app_ready(&base_url));

    if should_open_browser(&settings, is_first_launch) {
        if let Err(error) = webbrowser::open(&base_url) {
            warn!("failed to open browser automatically: {error}");
        }
    }

    info!("Bridge listening on {base_url}");
    info!("State directory: {}", paths.state_root.display());

    if let Err(error) = downloads::schedule_pending(state.clone()).await {
        warn!("failed to schedule recovered tasks on startup: {error}");
    }

    let router = web::router(state);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("bridge server stopped unexpectedly")?;

    Ok(())
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "youtudlp_bridge=info,axum=info".into()),
        )
        .with_target(false)
        .compact()
        .init();
}

fn should_open_browser(settings: &AppSettings, is_first_launch: bool) -> bool {
    if is_first_launch {
        settings.open_browser_on_first_launch
    } else {
        settings.open_browser_on_later_launch
    }
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        warn!("failed to listen for ctrl-c shutdown signal: {error}");
        return;
    }

    info!("Shutdown signal received, stopping bridge");
}
