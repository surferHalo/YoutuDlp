use std::fs::{self, File};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::Serialize;

static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub executable_dir: PathBuf,
    pub data_root: PathBuf,
    pub state_root: PathBuf,
    pub settings_path: PathBuf,
    pub queue_path: PathBuf,
    pub tasks_dir: PathBuf,
    pub playlists_dir: PathBuf,
    pub attempts_dir: PathBuf,
    pub library_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let current_executable =
            std::env::current_exe().context("failed to locate bridge executable")?;
        let executable_dir = current_executable
            .parent()
            .context("bridge executable has no parent directory")?
            .to_path_buf();
        let project_dirs = ProjectDirs::from("", "", "YoutuDlpBridge")
            .context("failed to resolve application data directory")?;
        let data_root = project_dirs.config_dir().to_path_buf();
        let state_root = data_root.join("state");

        Ok(Self {
            executable_dir,
            data_root,
            settings_path: state_root.join("settings.json"),
            queue_path: state_root.join("queue.json"),
            tasks_dir: state_root.join("tasks"),
            playlists_dir: state_root.join("playlists"),
            attempts_dir: state_root.join("attempts"),
            library_dir: state_root.join("library"),
            runtime_dir: state_root.join("runtime"),
            logs_dir: state_root.join("logs"),
            state_root,
        })
    }

    pub fn ensure_state_dirs(&self) -> Result<()> {
        for directory in [
            &self.data_root,
            &self.state_root,
            &self.tasks_dir,
            &self.playlists_dir,
            &self.attempts_dir,
            &self.library_dir,
            &self.runtime_dir,
            &self.logs_dir,
        ] {
            fs::create_dir_all(directory).with_context(|| {
                format!("failed to create state directory {}", directory.display())
            })?;
        }

        Ok(())
    }
}

/// Atomically write raw bytes to `path` (same remove+rename strategy as
/// [`write_json_atomic`]).
pub fn write_raw_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("path {} has no parent directory", path.display()))?;

    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp_path = parent.join(format!("{stem}.{seq}.tmp"));

    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create parent directory {}", parent.display()))?;

    let mut file = File::create(&tmp_path)
        .with_context(|| format!("failed to create temp file {}", tmp_path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write temp file {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync temp file {}", tmp_path.display()))?;
    drop(file);

    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("failed to replace file {}", path.display()))?;
    }
    fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to finalise write to {}", path.display()))?;

    Ok(())
}

/// Serialize `value` to pretty JSON and atomically write it to `path`.
///
/// Uses a unique per-call temp filename derived from a global sequence counter
/// so that concurrent writes to *different* final paths never share the same
/// `.tmp` file.  Writes to the *same* final path must be serialised by the
/// caller (e.g. via `TaskStore::write_lock`) so the remove + rename pair
/// cannot be interleaved.
pub fn write_json_atomic<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;

    let parent = path
        .parent()
        .with_context(|| format!("path {} has no parent directory", path.display()))?;

    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp_path = parent.join(format!("{stem}.{seq}.tmp"));

    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create parent directory {}", parent.display()))?;

    let mut file = File::create(&tmp_path)
        .with_context(|| format!("failed to create temp file {}", tmp_path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("failed to write temp file {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync temp file {}", tmp_path.display()))?;
    drop(file);

    // Windows does not support rename-over-existing; remove first then rename.
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("failed to replace file {}", path.display()))?;
    }
    fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to finalise write to {}", path.display()))?;

    Ok(())
}
