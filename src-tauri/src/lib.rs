//! CineBackup —— 影视素材 / DCP 跨平台备份工具（Tauri 2 + Rust）
//!
//! 模块划分：
//! - `disks`    磁盘 / 卷枚举（自动拉取本机硬盘，含卷标、容量、只读标志）
//! - `fsinfo`   磁盘文件系统类型探测（macOS statfs / Windows GetVolumeInformation）
//! - `walk`     文件/目录判断 + 目录遍历（含 macOS 元数据过滤）
//! - `hash`     分块流式 xxHash64 计算（绝不整文件读入内存）
//! - `copy`     原生分块拷贝，支持断点续传 / Dry Run
//! - `scan`     冲突预扫描，产出拷贝计划
//! - `engine`   任务调度：串行串起 扫描 → 拷贝 → 校验，后台线程执行
//! - `verify`   拷贝完成后的 xxHash64 校验阶段
//! - `task`     JSON 任务保存 / 加载
//! - `commands` Tauri 命令入口

pub mod commands;
pub mod copy;
pub mod disks;
pub mod engine;
pub mod events;
pub mod fsinfo;
pub mod hash;
pub mod scan;
pub mod state;
pub mod task;
pub mod types;
pub mod util;
pub mod verify;
pub mod walk;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state::AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::probe_path,
            commands::fs_type,
            commands::free_space,
            commands::list_disks,
            commands::start_job,
            commands::reply_decision,
            commands::cancel_job,
            commands::is_busy,
            commands::save_task_file,
            commands::load_task_file,
        ])
        .run(tauri::generate_context!())
        .expect("CineBackup 启动失败");
}
