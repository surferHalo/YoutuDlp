use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryTreeResponse {
    pub root_path: String,
    pub nodes: Vec<LibraryNode>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryNode {
    pub name: String,
    pub relative_path: String,
    pub item_type: String,
    pub size_bytes: Option<u64>,
    pub children: Vec<LibraryNode>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFolderRequest {
    pub parent_path: String,
    pub folder_name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveItemsRequest {
    pub source_paths: Vec<String>,
    pub destination_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteItemsRequest {
    pub target_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameItemRequest {
    pub relative_path: String,
    pub new_name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenPathRequest {
    pub relative_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathChange {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryMutationResponse {
    pub changed_paths: Vec<String>,
    pub moved: Vec<PathChange>,
    pub deleted: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryOpenResponse {
    pub opened_path: String,
}

pub fn collect_filenames(root: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    collect_filenames_into(root, &mut set);
    set
}

fn collect_filenames_into(dir: &Path, set: &mut HashSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_filenames_into(&path, set);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            set.insert(name.to_string());
        }
    }
}

pub fn build_tree(download_root: &Path) -> Result<LibraryTreeResponse> {
    ensure_download_root(download_root)?;

    let nodes = scan_dir(download_root, download_root)?;
    Ok(LibraryTreeResponse {
        root_path: download_root.to_string_lossy().to_string(),
        nodes,
    })
}

pub fn create_folder(
    download_root: &Path,
    request: &CreateFolderRequest,
) -> Result<LibraryMutationResponse> {
    ensure_download_root(download_root)?;

    let parent_path = resolve_relative_path(download_root, &request.parent_path)?;
    if !parent_path.is_dir() {
        bail!("目标父目录不存在或不是目录");
    }

    validate_folder_name(&request.folder_name)?;
    let folder_path = parent_path.join(request.folder_name.trim());
    if folder_path.exists() {
        bail!("同名目录已存在");
    }

    fs::create_dir(&folder_path)
        .with_context(|| format!("failed to create folder {}", folder_path.display()))?;

    let relative = relative_path(download_root, &folder_path);
    Ok(LibraryMutationResponse {
        changed_paths: vec![relative],
        moved: Vec::new(),
        deleted: Vec::new(),
    })
}

pub fn move_items(download_root: &Path, request: &MoveItemsRequest) -> Result<LibraryMutationResponse> {
    ensure_download_root(download_root)?;

    if request.source_paths.is_empty() {
        bail!("至少选择一个要移动的条目");
    }

    let destination_dir = resolve_relative_path(download_root, &request.destination_dir)?;
    if !destination_dir.is_dir() {
        bail!("目标目录不存在或不是目录");
    }

    let normalized_sources = dedupe_paths(&request.source_paths)?;
    let mut moved = Vec::new();
    let mut changed_paths = Vec::new();

    for source in normalized_sources {
        let source_path = resolve_relative_path(download_root, &source)?;
        if !source_path.exists() {
            bail!("条目不存在: {source}");
        }

        let file_name = source_path
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("无法识别条目名称: {source}"))?;
        let target_path = destination_dir.join(file_name);

        if source_path == target_path {
            bail!("源路径与目标路径相同: {source}");
        }

        if target_path.exists() {
            bail!("目标位置已存在同名条目: {}", target_path.display());
        }

        if source_path.is_dir() && target_path.starts_with(&source_path) {
            bail!("不能把目录移动到自己的子目录中: {source}");
        }

        fs::rename(&source_path, &target_path).with_context(|| {
            format!(
                "failed to move {} -> {}",
                source_path.display(),
                target_path.display()
            )
        })?;

        let target_relative = relative_path(download_root, &target_path);
        changed_paths.push(source.clone());
        changed_paths.push(target_relative.clone());
        moved.push(PathChange {
            from: source,
            to: target_relative,
        });
    }

    Ok(LibraryMutationResponse {
        changed_paths,
        moved,
        deleted: Vec::new(),
    })
}

pub fn delete_items(download_root: &Path, request: &DeleteItemsRequest) -> Result<LibraryMutationResponse> {
    ensure_download_root(download_root)?;

    if request.target_paths.is_empty() {
        bail!("至少选择一个要删除的条目");
    }

    let targets = dedupe_paths(&request.target_paths)?;
    let mut deleted = Vec::new();

    for target in targets {
        let target_path = resolve_relative_path(download_root, &target)?;
        if !target_path.exists() {
            continue;
        }

        if target_path.is_dir() {
            fs::remove_dir_all(&target_path).with_context(|| {
                format!("failed to remove directory {}", target_path.display())
            })?;
        } else {
            fs::remove_file(&target_path)
                .with_context(|| format!("failed to remove file {}", target_path.display()))?;
        }

        deleted.push(target);
    }

    Ok(LibraryMutationResponse {
        changed_paths: deleted.clone(),
        moved: Vec::new(),
        deleted,
    })
}

pub fn rename_item(download_root: &Path, request: &RenameItemRequest) -> Result<LibraryMutationResponse> {
    ensure_download_root(download_root)?;

    let source_path = resolve_relative_path(download_root, &request.relative_path)?;
    if !source_path.exists() {
        bail!("条目不存在");
    }

    if !source_path.is_dir() {
        bail!("只能重命名文件夹");
    }

    validate_folder_name(&request.new_name)?;

    let parent = source_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法获取父目录"))?;
    let target_path = parent.join(request.new_name.trim());

    if target_path.exists() {
        bail!("同名条目已存在");
    }

    let from = relative_path(download_root, &source_path);
    fs::rename(&source_path, &target_path).with_context(|| {
        format!(
            "failed to rename {} -> {}",
            source_path.display(),
            target_path.display()
        )
    })?;

    let to = relative_path(download_root, &target_path);
    Ok(LibraryMutationResponse {
        changed_paths: vec![from.clone(), to.clone()],
        moved: vec![PathChange { from, to }],
        deleted: Vec::new(),
    })
}

pub fn open_item(download_root: &Path, request: &OpenPathRequest) -> Result<LibraryOpenResponse> {
    ensure_download_root(download_root)?;
    let target_path = resolve_relative_path(download_root, &request.relative_path)?;
    if !target_path.exists() {
        bail!("条目不存在");
    }

    open_with_system(&target_path, false)?;
    Ok(LibraryOpenResponse {
        opened_path: relative_path(download_root, &target_path),
    })
}

pub fn open_folder(download_root: &Path, request: &OpenPathRequest) -> Result<LibraryOpenResponse> {
    ensure_download_root(download_root)?;
    let target_path = resolve_relative_path(download_root, &request.relative_path)?;
    if !target_path.exists() {
        bail!("条目不存在");
    }

    let folder_path = if target_path.is_dir() {
        target_path
    } else {
        target_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("无法定位父目录"))?
    };

    open_with_system(&folder_path, true)?;
    Ok(LibraryOpenResponse {
        opened_path: relative_path(download_root, &folder_path),
    })
}

fn scan_dir(root: &Path, current_dir: &Path) -> Result<Vec<LibraryNode>> {
    let mut nodes = Vec::new();

    for entry in fs::read_dir(current_dir)
        .with_context(|| format!("failed to scan library directory {}", current_dir.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if should_hide(&name, file_type.is_dir()) {
            continue;
        }

        let relative_path = relative_path(root, &path);
        if file_type.is_dir() {
            nodes.push(LibraryNode {
                name,
                relative_path,
                item_type: "directory".to_string(),
                size_bytes: None,
                children: scan_dir(root, &path)?,
            });
        } else {
            let size_bytes = entry.metadata().ok().map(|metadata| metadata.len());
            nodes.push(LibraryNode {
                name,
                relative_path,
                item_type: "file".to_string(),
                size_bytes,
                children: Vec::new(),
            });
        }
    }

    nodes.sort_by(|left, right| match (left.item_type.as_str(), right.item_type.as_str()) {
        ("directory", "file") => std::cmp::Ordering::Less,
        ("file", "directory") => std::cmp::Ordering::Greater,
        _ => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
    });

    Ok(nodes)
}

fn should_hide(name: &str, is_dir: bool) -> bool {
    if is_dir {
        return name.starts_with('.');
    }

    name.ends_with(".part")
        || name.ends_with(".ytdl")
        || name.ends_with(".temp")
        || name.contains(".frag")
        || name.ends_with(".info.json")
}

fn relative_path(root: &Path, path: &PathBuf) -> String {
    path.strip_prefix(root)
        .ok()
        .and_then(|relative| relative.to_str())
        .map(|relative| relative.replace('\\', "/"))
        .unwrap_or_default()
}

fn ensure_download_root(download_root: &Path) -> Result<()> {
    if !download_root.exists() {
        fs::create_dir_all(download_root).with_context(|| {
            format!("failed to create download root {}", download_root.display())
        })?;
    }

    Ok(())
}

fn resolve_relative_path(download_root: &Path, relative_path_input: &str) -> Result<PathBuf> {
    let relative_path = normalize_relative_path(relative_path_input)?;
    let path = if relative_path.is_empty() {
        download_root.to_path_buf()
    } else {
        download_root.join(&relative_path)
    };

    Ok(path)
}

fn normalize_relative_path(value: &str) -> Result<String> {
    let trimmed = value.trim().replace('\\', "/");
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let candidate = Path::new(&trimmed);
    let mut cleaned = PathBuf::new();

    for component in candidate.components() {
        match component {
            Component::Normal(part) => cleaned.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                bail!("路径必须限制在下载根目录内")
            }
        }
    }

    Ok(cleaned.to_string_lossy().replace('\\', "/"))
}

fn validate_folder_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("目录名不能为空");
    }

    if trimmed == "." || trimmed == ".." {
        bail!("目录名无效");
    }

    if trimmed.contains(['/', '\\']) {
        bail!("目录名不能包含路径分隔符");
    }

    if cfg!(target_os = "windows") && trimmed.contains(['<', '>', ':', '"', '|', '?', '*']) {
        bail!("目录名包含 Windows 不支持的字符");
    }

    Ok(())
}

fn dedupe_paths(values: &[String]) -> Result<Vec<String>> {
    let mut normalized = values
        .iter()
        .map(|value| normalize_relative_path(value))
        .collect::<Result<Vec<_>>>()?;
    normalized.sort();
    normalized.dedup();
    normalized.retain(|path| !path.is_empty());

    let mut filtered = Vec::new();
    for path in normalized {
        if filtered.iter().any(|existing: &String| is_same_or_descendant(&path, existing)) {
            continue;
        }
        filtered.push(path);
    }

    Ok(filtered)
}

fn is_same_or_descendant(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn open_with_system(path: &Path, directory_hint: bool) -> Result<()> {
    let mut command = if cfg!(target_os = "windows") {
        if directory_hint {
            let mut command = Command::new("explorer");
            command.arg(path);
            command
        } else {
            let mut command = Command::new("cmd");
            command.args(["/C", "start", "", &path.to_string_lossy()]);
            command
        }
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(path);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    };

    command
        .spawn()
        .with_context(|| format!("failed to open {}", path.display()))?;

    Ok(())
}