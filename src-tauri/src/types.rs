//! 前后端共享的数据结构

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- 源 / 目标

/// 单个源条目（前端点「添加源」时由 `probe_path` 返回）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceEntry {
    pub path: String,
    /// "file" | "dir" | "missing"
    pub kind: String,
    /// 所在卷文件系统类型（APFS / NTFS / exFAT …）
    pub fs: String,
    pub exists: bool,
    pub size: u64,
}

// ---------------------------------------------------------------- 计划动作

/// 单个文件的目标动作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedAction {
    /// 目标不存在 → 完整拷贝
    Copy,
    /// 大小不一致 → 断点续传（从目标文件末尾继续写入）
    Resume,
    /// 大小一致且 xxHash64 相同 → 跳过
    Skip,
    /// 大小一致但 xxHash64 不同 → 覆盖
    Overwrite,
    /// 询问模式下的「待用户决定」
    Conflict,
}

impl PlannedAction {
    pub fn label(self) -> &'static str {
        match self {
            PlannedAction::Copy => "完整拷贝",
            PlannedAction::Resume => "断点续传",
            PlannedAction::Skip => "跳过（内容一致）",
            PlannedAction::Overwrite => "覆盖",
            PlannedAction::Conflict => "待询问",
        }
    }
    /// 是否需要写盘
    pub fn needs_write(self) -> bool {
        matches!(
            self,
            PlannedAction::Copy | PlannedAction::Resume | PlannedAction::Overwrite
        )
    }
}

// ---------------------------------------------------------------- 计划条目

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    pub src: String,
    pub dst: String,
    pub size: u64,
    /// 目标已存在文件的大小（不存在为 0）
    pub existing_size: u64,
    /// 最终执行的动作（询问模式下会在运行时被用户决定覆盖）
    pub action: PlannedAction,
    /// 预扫描给出的建议动作（询问模式下展示在弹窗里）
    pub suggested: PlannedAction,
    /// 人类可读的判定理由
    pub reason: String,
    /// 预扫描阶段是否已经用哈希确认过「内容一致」
    pub hash_checked: bool,
    /// 运行时的最终动作（拷贝阶段写入）
    #[serde(skip)]
    pub final_action: Option<PlannedAction>,
    /// 需要写入的字节数（续传为剩余字节）
    pub needed_bytes: u64,
    /// 这个文件归属第几个源（`JobRequest.sources` 的下标）。
    /// 界面按源分条显示传输进度时需要把文件归堆，所以扫描阶段就标好。
    #[serde(default)]
    pub src_idx: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStats {
    pub copy: usize,
    pub resume: usize,
    pub skip: usize,
    pub overwrite: usize,
    pub conflict: usize,
    pub total_bytes: u64,
    pub filtered: usize,
}

// ---------------------------------------------------------------- 任务请求

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobOptions {
    /// 冲突时弹窗询问；关闭则按断点续传规则自动判定
    pub ask_on_conflict: bool,
    /// 快速扫描：大小相同即视为已备份，预扫描阶段不做哈希比对
    pub quick_scan: bool,
    /// 续传前校验已写入部分（比对前缀 xxHash64）
    pub resume_prefix_check: bool,
    /// 拷贝完成后自动全量校验
    pub verify_after_copy: bool,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            ask_on_conflict: false,
            quick_scan: false,
            resume_prefix_check: true,
            verify_after_copy: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRequest {
    pub sources: Vec<String>,
    pub target: String,
    #[serde(default)]
    pub options: JobOptions,
    #[serde(default)]
    pub dry_run: bool,
}

// ---------------------------------------------------------------- 用户回复

/// 模态框回复：冲突决定 + 错误处理决定
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UserReply {
    /// ① 跳过此文件
    Skip,
    /// ② 覆盖此文件
    Overwrite,
    /// ③ 全部跳过
    SkipAll,
    /// ④ 全部覆盖
    OverwriteAll,
    /// 继续下一个文件（出错时）
    Continue,
    /// 终止全部任务（出错时）
    AbortAll,
}

// ---------------------------------------------------------------- 校验结果

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileResult {
    /// 源文件路径
    pub path: String,
    /// 目标文件路径
    pub target: String,
    pub size: u64,
    /// "pass" | "fail" | "error" | "skip"
    pub status: String,
    pub src_hash: String,
    pub dst_hash: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobEnd {
    pub ok: bool,
    pub aborted: bool,
    pub dry_run: bool,
    pub copied: u64,
    pub resumed: u64,
    pub overwritten: u64,
    pub skipped: u64,
    pub pass: u64,
    pub failed: u64,
    pub total_bytes: u64,
    pub elapsed_secs: f64,
    pub message: String,
}
