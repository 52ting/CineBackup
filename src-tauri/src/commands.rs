//! Tauri 命令入口 —— 前端 `invoke()` 的唯一落点

use std::path::PathBuf;

use tauri::{AppHandle, State};

use crate::disks::{self, DiskInfo};
use crate::engine;
use crate::events;
use crate::fsinfo;
use crate::state::AppState;
use crate::task::{self, TaskFile};
use crate::types::{JobRequest, SourceEntry, UserReply};
use crate::util::path_to_string;
use crate::walk::{self, PathKind};

/// 探测一个路径：类型（文件 / 文件夹 / 缺失）+ 所在磁盘文件系统 + 大小
#[tauri::command]
pub fn probe_path(path: String) -> Result<SourceEntry, String> {
    let p = PathBuf::from(&path);
    let kind = walk::classify(&p);
    Ok(SourceEntry {
        path: path_to_string(&p),
        kind: kind.as_str().to_string(),
        fs: fsinfo::fs_type_of(&p),
        exists: matches!(kind, PathKind::File | PathKind::Dir),
        size: walk::file_size(&p),
    })
}

/// 取得某路径所在卷的文件系统类型（APFS / NTFS / exFAT / FAT32 …）
#[tauri::command]
pub fn fs_type(path: String) -> String {
    fsinfo::fs_type_of(&PathBuf::from(&path))
}

/// 目标盘剩余可用空间（字节）；失败返回 0
#[tauri::command]
pub fn free_space(path: String) -> u64 {
    engine::query_free_space(&path)
}

/// 自动拉取本机所有磁盘 / 卷。
/// Windows 列出全部盘符，macOS 列出根卷 + `/Volumes/*`；
/// 含卷标、文件系统、总容量、剩余空间、介质类型与是否可写。
#[tauri::command]
pub fn list_disks() -> Vec<DiskInfo> {
    disks::list_disks()
}

/// 启动备份 / 试运行。
/// 立即返回，实际工作在后台线程；同一时间只允许一个任务。
#[tauri::command]
pub fn start_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: JobRequest,
) -> Result<(), String> {
    if req.sources.is_empty() {
        return Err("请先添加至少一个源（文件或文件夹）".into());
    }
    if req.target.trim().is_empty() {
        return Err("请先选择目标文件夹".into());
    }
    let target = PathBuf::from(req.target.trim());
    if !req.dry_run {
        engine::ensure_target_writable(&target)?;
    } else if !target.exists() {
        return Err(format!("目标文件夹不存在：{}", path_to_string(&target)));
    }

    if !state.try_acquire() {
        return Err("已有任务正在运行，请等待完成或先取消。".into());
    }
    engine::spawn_job(app, req);
    Ok(())
}

/// 模态框回复（冲突：跳过 / 覆盖 / 全部跳过 / 全部覆盖；错误：继续 / 终止全部）
#[tauri::command]
pub fn reply_decision(state: State<'_, AppState>, reply: UserReply) -> Result<(), String> {
    // 没有挂起弹窗时（例如用户重复点击）幂等忽略，不报错
    let _ = state.send_reply(reply);
    Ok(())
}

/// 取消当前任务（拷贝在秒级内安全停止，已完成部分可被下次续传）
#[tauri::command]
pub fn cancel_job(state: State<'_, AppState>) -> Result<(), String> {
    state.request_cancel();
    Ok(())
}

/// 是否已有任务在跑
#[tauri::command]
pub fn is_busy(state: State<'_, AppState>) -> bool {
    engine::busy(state.inner())
}

/// 保存任务为 JSON（只存源路径数组 + 目标路径 + 选项）
#[tauri::command]
pub fn save_task_file(path: String, task: TaskFile) -> Result<(), String> {
    let p = PathBuf::from(&path);
    task::save_task(&p, task)?;
    Ok(())
}

/// 加载任务 JSON；跨平台或磁盘缺失的路径通过日志提示，不直接失败
#[tauri::command]
pub fn load_task_file(app: AppHandle, path: String) -> Result<TaskFile, String> {
    let p = PathBuf::from(&path);
    let loaded = task::load_task(&p)?;
    for w in &loaded.warnings {
        events::emit_log(&app, "warn", w.clone());
    }
    if loaded.cross_platform {
        events::emit_log(
            &app,
            "warn",
            "该任务由另一操作系统创建，请确认磁盘已按相同方式挂载。",
        );
    }
    events::emit_log(
        &app,
        "ok",
        format!(
            "任务已加载：{} 个源 → {}",
            loaded.task.sources.len(),
            loaded.task.target_dir
        ),
    );
    Ok(loaded.task)
}
