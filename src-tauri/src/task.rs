//! 任务保存 / 加载（JSON，macOS ↔ Windows 可互相加载）
//!
//! ## 设计
//! - JSON **只存**：多个源路径、目标文件夹路径、任务选项。
//! - **不存**磁盘文件系统信息 —— 每次加载任务时重新调用 `fsinfo::fs_type_of` 读取，
//!   因为同一个路径在不同机器上挂载的可能是不同格式的盘。
//! - 加载时按当前平台规范化路径分隔符（`\` ↔ `/`），并逐个探测存在性，
//!   把「盘未挂载 / 路径来自另一平台」的条目作为警告返回给 UI，而不是直接丢弃。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::JobOptions;
use crate::util::{
    looks_like_macos_path, looks_like_windows_path, normalize_separators, now_iso8601,
};

pub const TASK_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskFile {
    pub version: u32,
    pub app: String,
    pub created_at: String,
    /// 目标文件夹路径
    pub target_dir: String,
    /// 多个源路径（文件 / 文件夹混排）
    pub sources: Vec<String>,
    #[serde(default)]
    pub options: JobOptions,
}

/// 加载结果：任务本体 + 需要提示给用户的警告
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadedTask {
    pub task: TaskFile,
    /// 例如「源路径不存在，可能磁盘未挂载」
    pub warnings: Vec<String>,
    /// 源路径来自另一平台时给出提示
    pub cross_platform: bool,
}

/// 保存任务到指定 JSON 文件（UTF-8、带缩进，方便人工查看/版本管理）
pub fn save_task(path: &Path, mut task: TaskFile) -> Result<(), String> {
    task.version = TASK_FORMAT_VERSION;
    task.app = "CineBackup".into();
    if task.created_at.is_empty() {
        task.created_at = now_iso8601();
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
        }
    }
    let json = serde_json::to_string_pretty(&task).map_err(|e| format!("序列化失败：{e}"))?;
    fs::write(path, json).map_err(|e| format!("写入任务文件失败：{e}"))
}

/// 从 JSON 文件加载任务
pub fn load_task(path: &Path) -> Result<LoadedTask, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("读取任务文件失败：{e}"))?;
    let mut task: TaskFile =
        serde_json::from_str(&text).map_err(|e| format!("任务文件格式不正确：{e}"))?;

    if task.version > TASK_FORMAT_VERSION {
        return Err(format!(
            "任务文件版本 {} 高于当前程序支持的 {}，请升级 CineBackup。",
            task.version, TASK_FORMAT_VERSION
        ));
    }

    let mut warnings = Vec::new();
    let mut cross_platform = false;

    // 规范化目标路径
    task.target_dir = normalize_separators(task.target_dir.trim());
    if !task.target_dir.is_empty() {
        let t = PathBuf::from(&task.target_dir);
        if !t.exists() {
            warnings.push(format!("目标文件夹不存在（磁盘可能未挂载）：{}", task.target_dir));
        }
        if cross_platform_reference(&task.target_dir) {
            cross_platform = true;
        }
    } else {
        warnings.push("任务文件中没有记录目标文件夹。".into());
    }

    // 规范化每个源路径
    let mut normalized = Vec::with_capacity(task.sources.len());
    for raw in std::mem::take(&mut task.sources) {
        let p = normalize_separators(raw.trim());
        if p.is_empty() {
            continue;
        }
        if cross_platform_reference(&p) {
            cross_platform = true;
        }
        if !PathBuf::from(&p).exists() {
            warnings.push(format!("源路径不存在（磁盘可能未挂载，或路径来自另一平台）：{p}"));
        }
        normalized.push(p);
    }
    task.sources = normalized;

    if cross_platform {
        warnings.push(
            "该任务由另一操作系统创建，路径可能无法直接使用；请确认磁盘已按相同方式挂载后重新选择。"
                .into(),
        );
    }

    Ok(LoadedTask {
        task,
        warnings,
        cross_platform,
    })
}

/// 判断路径是否是「另一个平台」的写法
fn cross_platform_reference(p: &str) -> bool {
    if cfg!(windows) {
        // Windows 上出现 /Volumes/... /Users/... 这种纯 Unix 绝对路径
        looks_like_macos_path(p) && !looks_like_windows_path(p)
    } else {
        // macOS 上出现 C:\... 或 UNC 路径
        looks_like_windows_path(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join("cinebackup_task_test");
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("t.json");

        let task = TaskFile {
            version: 0,
            app: String::new(),
            created_at: String::new(),
            target_dir: dir.to_string_lossy().into_owned(),
            sources: vec![dir.to_string_lossy().into_owned()],
            options: JobOptions::default(),
        };
        save_task(&f, task).unwrap();
        let loaded = load_task(&f).unwrap();
        assert_eq!(loaded.task.version, TASK_FORMAT_VERSION);
        assert_eq!(loaded.task.app, "CineBackup");
        assert_eq!(loaded.task.sources.len(), 1);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let _ = fs::remove_dir_all(&dir);
    }
}
