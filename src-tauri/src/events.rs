//! 前后端事件契约（事件名 + 载荷结构 + 发送辅助）

use serde::Serialize;
use std::sync::mpsc::channel;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

use crate::state::{lock, AppState};
use crate::types::UserReply;
use crate::util::{human_bytes, now_iso8601};

// -------- 事件名（必须与前端 backend.js 的 EV 一致） --------
pub const EV_LOG: &str = "cb:log";
pub const EV_STATUS: &str = "cb:status";
pub const EV_SCAN: &str = "cb:scan";
pub const EV_PROGRESS: &str = "cb:progress";
pub const EV_PLAN: &str = "cb:plan";
pub const EV_CONFLICT: &str = "cb:conflict";
pub const EV_COPY_ERROR: &str = "cb:copy-error";
pub const EV_FILE_RESULT: &str = "cb:file-result";
pub const EV_JOB_END: &str = "cb:job-end";

// -------- 状态值 --------
pub const ST_IDLE: &str = "idle";
pub const ST_SCANNING: &str = "scanning";
pub const ST_COPYING: &str = "copying";
pub const ST_VERIFYING: &str = "verifying";
pub const ST_DONE: &str = "done";

// ---------------------------------------------------------------- 载荷

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEvent {
    /// "info" | "ok" | "warn" | "error"
    pub level: String,
    pub message: String,
    pub ts: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusEvent {
    pub status: String,
}

/// 预扫描进度（大目录扫描时持续推送，UI 不卡死）
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    /// "enum" 枚举文件 | "check" 冲突比对 | "hash" 哈希比对
    pub phase: String,
    pub files_seen: u64,
    pub dirs_seen: u64,
    pub checked: u64,
    pub total: u64,
    pub bytes_hashed: u64,
    pub current: String,
}

/// 单个源的传送进度。
///
/// 界面中间栏在任务运行时是「一行一个源」的传输列表，靠这个结构渲染每一行：
/// 源 → 目标、各自的进度条、当前文件、状态。
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SourceProgress {
    /// 该源在 `JobRequest.sources` 里的下标
    pub index: usize,
    /// 源路径（绝对路径，界面自行截断显示）
    pub path: String,
    /// 这个源本次需要写入的字节总量
    pub bytes_total: u64,
    pub bytes_done: u64,
    /// 归属该源的文件数
    pub files_total: u64,
    pub files_done: u64,
    /// "waiting" 排队中 | "active" 进行中 | "done" 已完成 | "failed" 有失败
    pub state: String,
    /// 该源当前正在处理的文件
    pub current_file: String,
}

/// 拷贝 / 校验进度（含速度与剩余时间）
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProgressReport {
    /// "copy" | "verify"
    pub phase: String,
    pub files_total: u64,
    pub files_done: u64,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub speed_bps: f64,
    pub eta_secs: f64,
    pub current_file: String,
    pub current_file_done: u64,
    pub current_file_total: u64,
    pub elapsed_secs: f64,
    /// 按源分组的进度。
    /// 拷贝阶段填充；校验阶段为空数组，界面此时沿用上一批数据只刷新阶段标签。
    pub sources: Vec<SourceProgress>,
}

/// 预扫描完成后的计划摘要
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlanSummary {
    pub copy: usize,
    pub resume: usize,
    pub skip: usize,
    pub overwrite: usize,
    pub conflict: usize,
    pub total_bytes: u64,
    pub filtered: usize,
}

/// 同名冲突询问
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictAsk {
    pub src: String,
    pub dst: String,
    pub src_size: u64,
    pub dst_size: u64,
    pub suggested: String,
    pub reason: String,
}

/// 文件操作失败询问
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyErrorAsk {
    pub src: String,
    pub dst: String,
    pub error: String,
    pub done: u64,
    pub total: u64,
}

// ---------------------------------------------------------------- 发送辅助

pub fn emit_log(app: &AppHandle, level: &str, message: impl Into<String>) {
    let e = LogEvent {
        level: level.into(),
        message: message.into(),
        ts: now_iso8601(),
    };
    let _ = app.emit(EV_LOG, e);
}

pub fn emit_status(app: &AppHandle, status: &str) {
    let _ = app.emit(EV_STATUS, StatusEvent { status: status.into() });
}

pub fn emit_scan(app: &AppHandle, p: &ScanProgress) {
    let _ = app.emit(EV_SCAN, p.clone());
}

pub fn emit_progress(app: &AppHandle, p: &ProgressReport) {
    let _ = app.emit(EV_PROGRESS, p.clone());
}

pub fn emit_plan(app: &AppHandle, p: &PlanSummary) {
    let _ = app.emit(EV_PLAN, p.clone());
}

pub fn emit_file_result(app: &AppHandle, r: &crate::types::FileResult) {
    let _ = app.emit(EV_FILE_RESULT, r.clone());
}

pub fn emit_job_end(app: &AppHandle, r: &crate::types::JobEnd) {
    let _ = app.emit(EV_JOB_END, r.clone());
}

/// 便捷日志：带字节数的信息
pub fn log_bytes(app: &AppHandle, prefix: &str, n: u64) {
    emit_log(app, "info", format!("{} {}", prefix, human_bytes(n)));
}

// ---------------------------------------------------------------- 模态框阻塞等待

/// 向前端弹出模态框并**阻塞等待**用户点击，直到：
/// - 用户点了某个按钮 → 返回对应 `UserReply`
/// - 用户点击「取消任务」 → 返回 `None`
///
/// 等待期间以 200ms 为粒度轮询取消标志，保证「取消」按钮在弹窗打开时依然有效。
pub fn ask_user<S: Serialize + Clone>(
    app: &AppHandle,
    st: &AppState,
    event: &str,
    payload: S,
) -> Option<UserReply> {
    let (tx, rx) = channel::<UserReply>();
    st.set_reply_sender(Some(tx));
    if let Err(e) = app.emit(event, payload) {
        st.set_reply_sender(None);
        emit_log(app, "error", format!("弹窗事件发送失败：{e}"));
        return None;
    }
    let mut out: Option<UserReply> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(r) => {
                out = Some(r);
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if st.cancelled() {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    st.set_reply_sender(None);
    out
}

/// 仅用于测试/防御：清空可能残留的回信通道
pub fn drop_pending_reply(st: &AppState) {
    *lock(&st.reply) = None;
}
