use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::settings::ToolOverrides;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRegistry {
    #[serde(rename = "ytDlp")]
    pub yt_dlp: ToolStatus,
    pub ffmpeg: ToolStatus,
    pub ffprobe: ToolStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    pub name: String,
    pub available: bool,
    pub source: String,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
}

impl ToolRegistry {
    pub fn discover(executable_dir: &Path, overrides: &ToolOverrides) -> Self {
        Self {
            yt_dlp: discover_tool("yt-dlp", &overrides.yt_dlp_path, executable_dir),
            ffmpeg: discover_tool("ffmpeg", &overrides.ffmpeg_path, executable_dir),
            ffprobe: discover_tool("ffprobe", &overrides.ffprobe_path, executable_dir),
        }
    }
}

fn discover_tool(name: &str, override_path: &Option<PathBuf>, executable_dir: &Path) -> ToolStatus {
    if let Some(path) = override_path.as_ref().filter(|path| path.exists()) {
        return ToolStatus {
            name: name.to_string(),
            available: true,
            source: "override".to_string(),
            path: Some(path.clone()),
            version: read_version(path),
        };
    }

    let sidecar_path = executable_dir.join(tool_binary_name(name));
    if sidecar_path.exists() {
        return ToolStatus {
            name: name.to_string(),
            available: true,
            source: "sidecar".to_string(),
            path: Some(sidecar_path.clone()),
            version: read_version(&sidecar_path),
        };
    }

    ToolStatus {
        name: name.to_string(),
        available: false,
        source: "missing".to_string(),
        path: None,
        version: None,
    }
}

fn tool_binary_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn read_version(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}
