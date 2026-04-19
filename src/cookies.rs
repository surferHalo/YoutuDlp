use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

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

pub fn apply_cookie_arguments(
    command: &mut Command,
    runtime_dir: &Path,
    cookie_file_name: Option<&str>,
    cookie_browser: Option<&str>,
    cookie_browser_profile: Option<&str>,
) {
    if let Some(browser_spec) = browser_cookie_spec(cookie_browser, cookie_browser_profile) {
        command.arg("--cookies-from-browser").arg(browser_spec);
        return;
    }

    if let Some(cookie_file_name) = normalized_cookie_value(cookie_file_name) {
        command
            .arg("--cookies")
            .arg(cookie_file_path(runtime_dir, cookie_file_name));
    }
}

pub fn validate_cookie_preferences(
    cookie_file_name: Option<&str>,
    cookie_browser: Option<&str>,
    cookie_browser_profile: Option<&str>,
) -> Result<()> {
    if let Some(cookie_file_name) = normalized_cookie_value(cookie_file_name) {
        sanitize_cookie_file_name(cookie_file_name)?;
    }

    if normalized_cookie_value(cookie_browser).is_none()
        && normalized_cookie_value(cookie_browser_profile).is_some()
    {
        bail!("cookie browser profile requires a cookie browser");
    }

    Ok(())
}

pub fn explain_cookie_error(message: &str) -> Option<String> {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("could not copy chrome cookie database")
        || (normalized.contains("permission denied")
            && normalized.contains("network\\cookies"))
    {
        return Some(
            "Windows blocked access to the Chromium cookie database. Easiest fixes: fully close Chrome/Edge/Brave and retry; or switch cookie source to Firefox; or fall back to a manually exported cookies.txt file."
                .to_string(),
        );
    }

    None
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

fn browser_cookie_spec(cookie_browser: Option<&str>, cookie_browser_profile: Option<&str>) -> Option<String> {
    let browser = normalized_cookie_value(cookie_browser)?.to_ascii_lowercase();
    let profile = normalized_cookie_value(cookie_browser_profile);

    Some(match profile {
        Some(profile) => format!("{browser}:{profile}"),
        None => browser,
    })
}

fn normalized_cookie_value(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}