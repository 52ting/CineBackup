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

/// 取得应用版本号（编译期常量，与 Cargo.toml 一致，前端标题栏显示用）
#[tauri::command]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

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

/// 跳过「当前正在校验的文件」，继续校验下一个。
///
/// 与「取消任务」**不是一回事**：取消会中断整个任务，这个只放弃当前这一个文件
/// （可能是几百 GB 的网络盘文件，读一半就够了）。被跳过的文件在结果表里记
/// `status = skip`（「跳过」栏、⏭），**不计入校验失败**。
///
/// 返回 `false` 表示当前**不在校验阶段**（拷贝中 / 空闲），请求未被受理。
/// 命令层必须做这个判断：拷贝阶段点「跳过」没有意义，不该悄悄记下来
/// 等到校验时再误跳过某个文件。
#[tauri::command]
pub fn skip_current_verify(state: State<'_, AppState>) -> bool {
    state.inner().request_skip_current()
}

/// 跳过「当前文件所在目录及其子目录」里的全部剩余文件（校验阶段专用）。
///
/// 与 [`skip_current_verify`] 的区别：那是**一次性**跳过当前这一个文件，
/// 这个是**持续**跳过整个文件夹 —— 设置后该目录前缀下的所有未校验文件
/// （含子目录里的）都会在结果表里记 `status = skip` 并跳过，直到校验阶段结束。
///
/// 返回 `false` 表示当前不在校验阶段（拷贝中 / 空闲）或目录为空，请求未被受理。
#[tauri::command]
pub fn skip_folder_verify(folder: String, state: State<'_, AppState>) -> bool {
    state.inner().request_skip_folder(folder)
}

/// 启动**对比校验**（只读：同时看左右两边，回答是否一致）。
///
/// 立即返回，实际工作在后台线程；进度与结果通过
/// `cb:compare-progress` / `cb:compare-done` 事件推送。
/// 与备份任务**互相独立**（对比不抢备份的 busy 槽位），但同一时间只允许一个对比。
#[tauri::command]
pub fn start_compare(
    app: AppHandle,
    state: State<'_, AppState>,
    left: String,
    right: String,
    options: crate::compare::CompareOptions,
) -> Result<(), String> {
    if left.trim().is_empty() || right.trim().is_empty() {
        return Err("请先选择要对比的两边路径（文件或文件夹）。".into());
    }
    if state.is_comparing() {
        return Err("已有对比正在进行，请等待完成或先取消。".into());
    }
    state.clear_compare_cancel();
    state.try_acquire_compare();
    engine::spawn_compare(app, left, right, options);
    Ok(())
}

/// 取消正在进行的对比校验
#[tauri::command]
pub fn cancel_compare(state: State<'_, AppState>) -> Result<(), String> {
    state.request_compare_cancel();
    Ok(())
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

/// 前端确认退出后调用：立即退出进程。
///
/// ⚠️ 不要用 `window.close()` 或窗口 close 来退出——那会再次触发
/// `CloseRequested`（被 run() 里的拦截逻辑 prevent_close 拦下），造成死循环。
/// `AppHandle::exit(0)` 直接结束进程，不经过窗口关闭事件。
#[tauri::command]
pub fn close_app(app: AppHandle) {
    app.exit(0);
}
