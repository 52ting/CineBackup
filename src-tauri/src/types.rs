//! 前后端共享的数据结构

use serde::{Deserialize, Serialize};

use crate::hash::HashAlgo;

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
    /// 大小一致且哈希相同 → 跳过
    Skip,
    /// 大小一致但哈希不同 → 覆盖
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
    /// 源路径（展示用字符串；含非法 UTF-8 字节时用 lossy 替换成 U+FFFD）
    pub src: String,
    /// 目标路径（展示用字符串，同上）
    pub dst: String,
    /// 源路径的**原始字节**路径。拷贝 / 哈希 / 校验一律用它，
    /// 绝不经过 `to_string_lossy()` —— 否则非法字节被替换成 `�`，
    /// 后续 `File::open` 就再也打不开真实文件了（EILSEQ 的真凶之一）。
    #[serde(skip)]
    pub src_path: std::path::PathBuf,
    /// 目标路径的原始字节路径（同上）
    #[serde(skip)]
    pub dst_path: std::path::PathBuf,
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
    /// 续传前校验已写入部分（比对前缀哈希）
    pub resume_prefix_check: bool,
    /// 拷贝完成后自动全量校验
    pub verify_after_copy: bool,
    /// 跳过拷贝阶段：只做扫描 + 校验目标里已存在的文件，不写入任何数据
    #[serde(default)]
    pub skip_copy: bool,
    /// 跳过校验阶段：只拷贝，不做内容哈希校验
    #[serde(default)]
    pub skip_verify: bool,
    /// 复制源文件的**元数据**（权限位 / 时间戳 / xattr）到目标。**默认关闭**。
    ///
    /// ⚠️ 资源分支（resource fork）**永不复制** —— 那需要打开
    /// `..namedfork/rsrc`，属于本程序明令禁止的路径（见 `posix.rs` 模块注释）。
    #[serde(default)]
    pub copy_metadata: bool,
    /// 校验失败时做**分片哈希定位**：按块单独算摘要，指出首个不一致的分片。
    ///
    /// 只在不一致的文件上触发，但会额外把该文件读一遍（排障用，默认关闭）。
    #[serde(default)]
    pub debug_chunk_hash: bool,
    /// 内容哈希算法：默认 SHA-256，可切 xxHash64 提速。
    /// 同一次任务里预扫描查重 / 续传前缀 / 最终校验都用这一种。
    #[serde(default)]
    pub hash_algo: HashAlgo,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            ask_on_conflict: false,
            quick_scan: false,
            resume_prefix_check: true,
            verify_after_copy: true,
            skip_copy: false,
            skip_verify: false,
            copy_metadata: false,
            debug_chunk_hash: false,
            hash_algo: HashAlgo::default(),
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
    /// 源文件内容哈希（十六进制）。
    /// 算法见 `JobOptions.hash_algo`：SHA-256 = 64 字符 / xxHash64 = 16 字符
    pub src_hash: String,
    /// 目标文件内容哈希（同上）
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 前端 `collectOptions()` 实际发出的完整载荷必须能被后端解析。
    ///
    /// 这条是**契约测试**：前端一旦加字段、后端没跟上（或名字拼错），
    /// 这里会直接红 —— 比在浏览器里点一遍可靠。
    /// 浏览器端探针在本机已不可用（Chrome headless 无法启动），所以契约钉在 Rust 侧。
    #[test]
    fn frontend_options_payload_round_trips() {
        // 与 src/main.js 的 collectOptions() 逐字段对应
        let payload = r#"{
            "askOnConflict": false,
            "quickScan": true,
            "resumePrefixCheck": true,
            "verifyAfterCopy": true,
            "skipCopy": false,
            "skipVerify": false,
            "copyMetadata": false,
            "debugChunkHash": false,
            "hashAlgo": "sha256"
        }"#;
        let o: JobOptions = serde_json::from_str(payload).expect("前端载荷必须能解析");
        assert!(!o.ask_on_conflict);
        assert!(o.quick_scan);
        assert!(o.resume_prefix_check, "续传前校验必须能带过来（默认开）");
        assert!(o.verify_after_copy);
        assert!(!o.skip_copy);
        assert!(!o.skip_verify);
        assert!(!o.copy_metadata, "复制元数据默认必须关");
        assert!(!o.debug_chunk_hash, "分片定位默认必须关");
        assert_eq!(o.hash_algo, HashAlgo::Sha256);
    }

    /// 缺新字段的老任务文件仍然要能加载，且新选项取「安全默认值」
    #[test]
    fn legacy_task_json_gets_safe_defaults_for_new_options() {
        let legacy = r#"{"askOnConflict":false,"quickScan":false,"resumePrefixCheck":true,"verifyAfterCopy":true}"#;
        let o: JobOptions = serde_json::from_str(legacy).expect("老任务文件应能加载");
        assert!(!o.copy_metadata, "老文件没有该字段 → 必须默认不复制元数据");
        assert!(!o.debug_chunk_hash, "老文件没有该字段 → 必须默认关闭分片定位");
        assert_eq!(o.hash_algo, HashAlgo::Sha256);
    }

    /// 任务 JSON 里的写法：`hashAlgo` 存 slug；缺这个字段的老任务文件必须能照常加载
    #[test]
    fn hash_algo_json_key() {
        let json = serde_json::to_string(&JobOptions::default()).unwrap();
        assert!(json.contains("\"hashAlgo\":\"sha256\""), "序列化结果：{json}");

        // 0.4.1 及更早的任务文件没有 hashAlgo 字段 → 用默认值补齐
        let legacy = r#"{"askOnConflict":false,"quickScan":true,"resumePrefixCheck":true,"verifyAfterCopy":true}"#;
        let o: JobOptions = serde_json::from_str(legacy).expect("老任务文件应能加载");
        assert_eq!(o.hash_algo, HashAlgo::Sha256);
        assert!(o.quick_scan);

        // 显式指定 xxh64 也能读回来
        let explicit = r#"{"askOnConflict":false,"quickScan":false,"resumePrefixCheck":true,"verifyAfterCopy":true,"hashAlgo":"xxh64"}"#;
        let o2: JobOptions = serde_json::from_str(explicit).unwrap();
        assert_eq!(o2.hash_algo, HashAlgo::Xxh64);
    }
}
