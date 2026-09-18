//! 文件 / 目录判断 + 目录遍历
//!
//! - 自动区分「单个文件」「文件夹」「不存在」
//! - 递归遍历目录，**跳过 macOS 系统元数据**（.DS_Store、._* 等）
//! - Windows 上不启用该过滤规则（按需求：Win 忽略这一类过滤）
//! - 遍历过程可被取消（检查 AtomicBool），不会卡住 UI

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use walkdir::WalkDir;

/// 路径类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    File,
    Dir,
    Missing,
    Other,
}

impl PathKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PathKind::File => "file",
            PathKind::Dir => "dir",
            PathKind::Missing => "missing",
            PathKind::Other => "other",
        }
    }
}

/// 判断一个路径是文件、文件夹还是不存在（不跟随符号链接）
pub fn classify(p: &Path) -> PathKind {
    match fs::symlink_metadata(p) {
        Ok(md) => {
            if md.is_dir() {
                PathKind::Dir
            } else if md.is_file() {
                PathKind::File
            } else {
                PathKind::Other
            }
        }
        Err(_) => PathKind::Missing,
    }
}

pub fn is_file(p: &Path) -> bool {
    classify(p) == PathKind::File
}

pub fn is_dir(p: &Path) -> bool {
    classify(p) == PathKind::Dir
}

/// 文件大小；失败返回 0
pub fn file_size(p: &Path) -> u64 {
    fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

// ---------------------------------------------------------------- macOS 元数据过滤

/// 目录级过滤（会连同子树一起跳过）
#[cfg(target_os = "macos")]
pub fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".Spotlight-V100"
            | ".Trashes"
            | ".fseventsd"
            | ".DocumentRevisions-V100"
            | ".TemporaryItems"
            | ".vol"
    ) || name.starts_with("._")
}

/// 文件级过滤
#[cfg(target_os = "macos")]
pub fn should_skip_file(name: &str) -> bool {
    name == ".DS_Store"
        || name == ".VolumeIcon.icns"
        || name == "Icon\r"
        || name.starts_with("._")
        || name == ".localized"
}

// Windows：按需求，不应用 macOS 的元数据过滤规则
#[cfg(not(target_os = "macos"))]
pub fn should_skip_dir(_name: &str) -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub fn should_skip_file(_name: &str) -> bool {
    false
}

// ---------------------------------------------------------------- 遍历

#[derive(Debug, Default, Clone)]
pub struct WalkOutcome {
    pub files: usize,
    pub dirs: usize,
    /// 被过滤掉的 macOS 元数据条目数
    pub filtered: usize,
    /// 读不到的条目（权限不足 / 盘掉线）描述
    pub errors: Vec<String>,
    pub cancelled: bool,
}

/// 递归遍历目录树。
///
/// `visit(path, is_dir)` 会对每个「目录」和「文件」各调用一次（根目录本身也会回调，方便建目录）。
/// 目录按名称排序，结果可复现；不跟随符号链接，避免死循环。
pub fn walk_tree(
    root: &Path,
    cancel: &AtomicBool,
    mut visit: impl FnMut(&Path, bool),
) -> WalkOutcome {
    let mut out = WalkOutcome::default();

    // 根目录本身
    if is_dir(root) {
        out.dirs += 1;
        visit(root, true);
    } else if is_file(root) {
        out.files += 1;
        visit(root, false);
        return out;
    } else {
        out.errors.push(format!("路径不可访问：{}", root.display()));
        return out;
    }

    let iter = WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            // 只对目录做「剪枝」判断
            !(e.file_type().is_dir() && should_skip_dir(&name))
        });

    for entry in iter {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            out.cancelled = true;
            return out;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                out.errors.push(format!("遍历失败：{err}"));
                continue;
            }
        };
        if entry.depth() == 0 {
            continue; // 根目录已处理
        }

        let path = entry.path();
        let ft = entry.file_type();
        if ft.is_dir() {
            let name = entry.file_name().to_string_lossy();
            if should_skip_dir(&name) {
                out.filtered += 1;
                continue;
            }
            out.dirs += 1;
            visit(path, true);
        } else if ft.is_file() {
            let name = entry.file_name().to_string_lossy();
            if should_skip_file(&name) {
                out.filtered += 1;
                continue;
            }
            out.files += 1;
            visit(path, false);
        } else {
            // 符号链接 / 设备文件 / fifo —— 备份工具一律忽略
            out.filtered += 1;
        }
    }
    out
}

/// 便利函数：只收集文件列表（递归）
pub fn collect_files(root: &Path, cancel: &AtomicBool) -> (Vec<PathBuf>, WalkOutcome) {
    let mut files = Vec::new();
    let out = walk_tree(root, cancel, |p, is_dir| {
        if !is_dir {
            files.push(p.to_path_buf());
        }
    });
    (files, out)
}

/// 计算 `path` 相对于 `root` 的相对路径（用于拼目标路径）
pub fn relative_to(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn classify_works() {
        assert_eq!(classify(Path::new(env!("CARGO_MANIFEST_DIR"))), PathKind::Dir);
        let f = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert_eq!(classify(&f), PathKind::File);
        assert_eq!(classify(Path::new("/no/such/path/xyz")), PathKind::Missing);
    }

    #[test]
    fn walk_finds_cargo_toml() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let cancel = AtomicBool::new(false);
        let (files, out) = collect_files(&root, &cancel);
        assert!(files.iter().any(|p| p.ends_with("Cargo.toml")));
        assert!(out.files > 0);
    }
}
