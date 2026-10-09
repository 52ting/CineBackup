//! 魔王拷贝（MowangCopy）—— 影视素材 / DCP 跨平台备份工具（Tauri 2 + Rust）
//!
//! 模块划分：
//! - `disks`    磁盘 / 卷枚举（自动拉取本机硬盘，含卷标、容量、只读标志）
//! - `fsinfo`   磁盘文件系统类型探测（macOS statfs / Windows GetVolumeInformation）
//! - `walk`     文件/目录判断 + 目录遍历（含 macOS 元数据过滤）
//! - `hash`     分块流式内容哈希（默认 SHA-256，可切 xxHash64，绝不整文件读入内存）
//! - `copy`     原生分块拷贝，支持断点续传 / Dry Run
//! - `scan`     冲突预扫描，产出拷贝计划
//! - `engine`   任务调度：串行串起 扫描 → 拷贝 → 校验，后台线程执行
//! - `verify`   拷贝完成后的全量哈希校验阶段（默认 SHA-256）
//! - `compare`  独立的对比校验（两边文件/文件夹是否一致，只读）
//! - `posix`    唯一的 Data Fork 读写入入口（拷贝与哈希共用，见模块注释里的三条铁律）
//! - `task`     JSON 任务保存 / 加载
//! - `commands` Tauri 命令入口

pub mod commands;
pub mod compare;
pub mod copy;
pub mod disks;
pub mod engine;
pub mod events;
pub mod fsinfo;
pub mod hash;
pub mod namecheck;
pub mod posix;
pub mod scan;
pub mod state;
pub mod task;
pub mod types;
pub mod unassigned;
pub mod util;
pub mod verify;
pub mod walk;

use tauri::Emitter;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state::AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::app_version,
            commands::probe_path,
            commands::fs_type,
            commands::free_space,
            commands::list_disks,
            commands::start_job,
            commands::reply_decision,
            commands::cancel_job,
            commands::is_busy,
            commands::skip_current_verify,
            commands::start_compare,
            commands::cancel_compare,
            commands::save_task_file,
            commands::load_task_file,
            commands::close_app,
        ])
        .build(tauri::generate_context!())
        .expect("魔王拷贝 启动失败")
        .run(|app_handle: &tauri::AppHandle<tauri::Wry>, event: tauri::RunEvent| {
            // 关闭确认：拦截主窗口关闭（点红叉 / Cmd+W / 关闭按钮），
            // 先 prevent_close 并把请求发给前端，由前端弹确认框；
            // 用户确认后前端调用 `close_app` 命令真正退出（app.exit 不经过本事件，不会循环）。
            // 注意：macOS 的 Cmd+Q（应用菜单退出）走 ExitRequested，不在本拦截范围内，直接退出。
            if let tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::CloseRequested { api, .. },
                ..
            } = event
            {
                if label == "main" {
                    api.prevent_close();
                    let _ = app_handle.emit("cb:close-requested", ());
                }
            }
        });
}
