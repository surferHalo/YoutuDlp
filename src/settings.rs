use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::storage;

pub const MAX_CONCURRENT_DOWNLOADS_LIMIT: u8 = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppSettings {
    pub download_root: PathBuf,
    pub max_concurrent_downloads: u8,
    pub open_browser_on_first_launch: bool,
    pub open_browser_on_later_launch: bool,
    pub incomplete_task_recovery: String,
    pub default_target_subdir: String,
    pub show_advanced_by_default: bool,
    pub default_download_mode: String,
    pub default_container: String,
    pub default_quality: String,
    pub default_subtitle_languages: Vec<String>,
    pub default_cookie_file_name: Option<String>,
    pub default_raw_args: Vec<String>,
    pub default_raw_format_code: Option<String>,
    pub tool_overrides: ToolOverrides,
    pub playlist_resolve_concurrency: u8,
    pub playlist_resolve_interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ToolOverrides {
    #[serde(rename = "ytDlpPath")]
    pub yt_dlp_path: Option<PathBuf>,
    pub ffmpeg_path: Option<PathBuf>,
    pub ffprobe_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            download_root: default_download_root(),
            max_concurrent_downloads: 3,
            open_browser_on_first_launch: true,
            open_browser_on_later_launch: true,
            incomplete_task_recovery: "manual".to_string(),
            default_target_subdir: String::new(),
            show_advanced_by_default: false,
            default_download_mode: "video".to_string(),
            default_container: "mp4".to_string(),
            default_quality: "1080p".to_string(),
            default_subtitle_languages: Vec::new(),
            default_cookie_file_name: None,
            default_raw_args: Vec::new(),
            default_raw_format_code: None,
            tool_overrides: ToolOverrides::default(),
            playlist_resolve_concurrency: 3,
            playlist_resolve_interval_ms: 300,
        }
    }
}

impl AppSettings {
    pub fn validate(&self) -> Result<()> {
        if self.download_root.as_os_str().is_empty() {
            bail!("download root cannot be empty");
        }

        if self.max_concurrent_downloads == 0 {
            bail!("max concurrent downloads must be at least 1");
        }

        if self.max_concurrent_downloads > MAX_CONCURRENT_DOWNLOADS_LIMIT {
            bail!(
                "max concurrent downloads cannot exceed {}",
                MAX_CONCURRENT_DOWNLOADS_LIMIT
            );
        }

        if self.default_download_mode.trim().is_empty() {
            bail!("default download mode cannot be empty");
        }

        if self.default_container.trim().is_empty() {
            bail!("default container cannot be empty");
        }

        if self.default_quality.trim().is_empty() {
            bail!("default quality cannot be empty");
        }

        if self
            .default_subtitle_languages
            .iter()
            .any(|value| value.trim().is_empty())
        {
            bail!("default subtitle languages cannot contain empty values");
        }

        if self
            .default_raw_args
            .iter()
            .any(|value| value.trim().is_empty())
        {
            bail!("default raw args cannot contain empty values");
        }

        if self.playlist_resolve_concurrency == 0 {
            bail!("playlist resolve concurrency must be at least 1");
        }
        if self.playlist_resolve_concurrency > 16 {
            bail!("playlist resolve concurrency cannot exceed 16");
        }

        Ok(())
    }
}

impl SettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load_or_create_default(&self) -> Result<(AppSettings, bool)> {
        if !self.path.exists() {
            let settings = AppSettings::default();
            settings.validate()?;
            self.save(&settings)?;
            return Ok((settings, true));
        }

        let raw = std::fs::read(&self.path)
            .with_context(|| format!("failed to read settings file at {}", self.path.display()))?;
        let settings = serde_json::from_slice::<AppSettings>(&raw)
            .with_context(|| format!("failed to parse settings file at {}", self.path.display()))?;

        settings.validate()?;

        Ok((settings, false))
    }

    pub fn save(&self, settings: &AppSettings) -> Result<()> {
        settings.validate()?;
        storage::write_json_atomic(&self.path, settings)
    }
}

fn default_download_root() -> PathBuf {
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("."))
}
