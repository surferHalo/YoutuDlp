use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::storage;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCookiesRequest {
    pub file_name: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCookiesResponse {
    pub cookie_file_name: String,
}

pub fn import_cookies(runtime_dir: &Path, request: &ImportCookiesRequest) -> Result<ImportCookiesResponse> {
    let file_name = sanitize_cookie_file_name(&request.file_name)?;
    if request.content.trim().is_empty() {
        bail!("cookie file content cannot be empty");
    }

    let cookies_dir = runtime_dir.join("cookies");
    let path = cookies_dir.join(&file_name);
    storage::write_raw_atomic(&path, request.content.as_bytes())
        .with_context(|| format!("failed to store cookie file {}", path.display()))?;

    Ok(ImportCookiesResponse {
        cookie_file_name: file_name,
    })
}

pub fn cookie_file_path(runtime_dir: &Path, cookie_file_name: &str) -> PathBuf {
    runtime_dir.join("cookies").join(cookie_file_name)
}

fn sanitize_cookie_file_name(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("cookie file name cannot be empty");
    }

    let mut sanitized = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }

    if !sanitized.to_ascii_lowercase().ends_with(".txt") {
        sanitized.push_str(".txt");
    }

    if sanitized == ".txt" {
        bail!("cookie file name is invalid");
    }

    Ok(sanitized)
}