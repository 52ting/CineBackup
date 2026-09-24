//! 拷贝完成后的全量哈希校验阶段
//!
//! - 逐文件计算「源文件」与「目标同名文件」的内容哈希并比对
//!   （算法由 `JobOptions.hash_algo` 决定，默认 **SHA-256**，64 位十六进制）
//! - 文件夹源：其下每个文件都已在 `Plan` 中被展开成独立条目，等价于递归遍历比对
//! - 读取失败（权限 / 盘掉线）→ 该文件标记为 error，不中断整体流程
//! - 单独统计校验速度与剩余时间（与拷贝阶段的速度互不干扰）
//!
//! ## 进度必须「边读边发」
//!
//! 一个文件要读**源 + 目标两遍**：2 GB 的素材在 64 MB/s 上就是 60 秒以上。
//! 如果只在「一个文件读完」之后才发一次进度，界面会长时间完全静止 ——
//! 用户看到的就是「校验时底下进度条数据都不会动」（0.4.4 及以前）。
//!
//! 所以这里对齐 `engine.rs` 拷贝阶段的做法：**在每个 4 MiB 读取块的回调里**上报
//! （150ms 节流），并在每个文件收尾时**强制补一条**。收尾那条不能用节流窗口判定：
//! 小文件几百毫秒能连着读完好几个，它们会整段被吞掉 —— 表现就是「结果表在涨、
//! 进度条不动」。两条约定各有回归测试，见 `tests/verify_progress.rs`。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tauri::AppHandle;

use crate::events::{self, ProgressReport};
use crate::hash::{self, HashAlgo};
use crate::types::{FileResult, PlanItem};
use crate::util::{human_bytes, path_to_string, RateMeter, Throttle};

/// 进度上报的最小间隔（毫秒）。与拷贝阶段保持一致。
pub const PROGRESS_MS: u64 = 150;

#[derive(Debug, Default, Clone, Copy)]
pub struct VerifyStats {
    pub pass: u64,
    pub failed: u64,
    pub errors: u64,
    pub bytes: u64,
}

/// 校验阶段的三个输出口（日志 / 结果表 / 进度）。
///
/// 抽成参数是为了让单测能接管它们：`tests/verify_progress.rs` 靠它数
/// 「到底上报了多少条」，而「按块上报」这条约定必须钉住 ——
/// 一旦被改回「循环末尾发一次」，那个真机 bug 就会原样复活。
pub struct VerifySinks<'a> {
    pub log: &'a mut dyn FnMut(&str, String),
    pub result: &'a mut dyn FnMut(&FileResult),
    pub progress: &'a mut dyn FnMut(&ProgressReport),
}

/// 执行校验（生产入口）：三个输出口都接到前端事件上。
///
/// - `items`：完整计划；`idx`：本次需要校验的条目下标
///   （= 所有被写入过的文件 + 快速扫描模式下未做过哈希比对而被跳过的文件）
/// - `algo`：内容哈希算法（同一次任务与预扫描、续传前缀保持一致）
/// - `total_bytes`：本轮校验将读取的总字节数（= Σ size × 2，源一遍、目标一遍），
///   用于换算剩余时间
pub fn run_verify(
    app: &AppHandle,
    items: &[PlanItem],
    idx: &[usize],
    algo: HashAlgo,
    cancel: &AtomicBool,
    total_bytes: u64,
) -> VerifyStats {
    let mut log = |level: &str, msg: String| events::emit_log(app, level, msg);
    let mut result = |r: &FileResult| events::emit_file_result(app, r);
    let mut progress = |p: &ProgressReport| events::emit_progress(app, p);
    run_verify_with(
        items,
        idx,
        algo,
        cancel,
        total_bytes,
        PROGRESS_MS,
        VerifySinks {
            log: &mut log,
            result: &mut result,
            progress: &mut progress,
        },
    )
}

/// 校验主体。`progress_ms` 是进度上报的最小间隔：生产传 [`PROGRESS_MS`]；
/// 单测传 0 表示「每个读取块都上报」，传一个极大值表示「只允许收尾强制上报」，
/// 两种极端各能测出一条约定。
pub fn run_verify_with(
    items: &[PlanItem],
    idx: &[usize],
    algo: HashAlgo,
    cancel: &AtomicBool,
    total_bytes: u64,
    progress_ms: u64,
    sinks: VerifySinks<'_>,
) -> VerifyStats {
    let VerifySinks {
        log,
        result: result_sink,
        progress: progress_sink,
    } = sinks;

    let mut stats = VerifyStats::default();
    let mut meter = RateMeter::new();
    let mut throttle = Throttle::new(progress_ms);
    let mut done_bytes: u64 = 0;
    let files_total = idx.len() as u64;
    let started = Instant::now();

    log(
        "info",
        format!(
            "校验阶段开始（{}）：共 {} 个文件，需读取 {}（源 + 目标各一遍）",
            algo.label(),
            files_total,
            human_bytes(total_bytes)
        ),
    );

    for (i, &pos) in idx.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            log("warn", "校验被取消。".to_string());
            break;
        }
        let it = &items[pos];
        let src = Path::new(&it.src);
        let dst = Path::new(&it.dst);
        // 一个文件读「源 + 目标」两遍，所以「当前文件」的总量是 size × 2
        let file_total = it.size.saturating_mul(2);
        let cur = path_to_string(src);

        let mut file_read: u64 = 0;
        // 边读边发。注意 files_done 传的是 i：正在读的这一个还没读完，
        // 报 i+1 会让人误以为它已经完成了。收尾时再强制补一条 i+1。
        let (h_src, h_dst) = {
            let mut on = |n: u64| {
                meter.add(n);
                done_bytes = done_bytes.saturating_add(n);
                file_read = file_read.saturating_add(n);
                if throttle.ready() {
                    progress_sink(&make_report(
                        &mut meter,
                        files_total,
                        i as u64,
                        total_bytes,
                        done_bytes,
                        &cur,
                        file_read,
                        file_total,
                        started,
                    ));
                }
            };
            let a = hash::hash_file(src, algo, cancel, &mut on);
            let b = hash::hash_file(dst, algo, cancel, &mut on);
            (a, b)
        };

        let result = match (h_src, h_dst) {
            (Ok((a, na)), Ok((b, nb))) => {
                let same = a == b && na == nb;
                if same {
                    stats.pass += 1;
                    FileResult {
                        path: path_to_string(src),
                        target: path_to_string(dst),
                        size: it.size,
                        status: "pass".into(),
                        src_hash: a.hex().to_string(),
                        dst_hash: b.hex().to_string(),
                        message: format!("{} 校验一致", algo.label()),
                    }
                } else {
                    stats.failed += 1;
                    FileResult {
                        path: path_to_string(src),
                        target: path_to_string(dst),
                        size: it.size,
                        status: "fail".into(),
                        src_hash: a.hex().to_string(),
                        dst_hash: b.hex().to_string(),
                        message: format!(
                            "{} 不一致（源 {} / 目标 {}，字节 {} vs {}）",
                            algo.label(),
                            a.short(8),
                            b.short(8),
                            na,
                            nb
                        ),
                    }
                }
            }
            (Err(e), _) => {
                stats.errors += 1;
                FileResult {
                    path: path_to_string(src),
                    target: path_to_string(dst),
                    size: it.size,
                    status: "error".into(),
                    src_hash: String::new(),
                    dst_hash: String::new(),
                    message: format!("源文件读取失败：{e}"),
                }
            }
            (_, Err(e)) => {
                stats.errors += 1;
                FileResult {
                    path: path_to_string(src),
                    target: path_to_string(dst),
                    size: it.size,
                    status: "error".into(),
                    src_hash: String::new(),
                    dst_hash: String::new(),
                    message: format!("目标文件读取失败：{e}"),
                }
            }
        };

        stats.bytes = stats.bytes.saturating_add(file_read);
        let level = match result.status.as_str() {
            "pass" => "ok",
            "fail" => "error",
            _ => "warn",
        };
        log(level, format!("{} {}", result.message, result.path));
        result_sink(&result);

        // 文件收尾：**强制**补一条（不走节流窗口），让「已完成 N / 共 M」立刻 +1。
        // 小文件连续读完时，中间那几次上报会被 150ms 窗口挡掉，
        // 只有这里补上，界面才不至于「结果表在涨、进度条不动」。
        throttle.force();
        progress_sink(&make_report(
            &mut meter,
            files_total,
            (i + 1) as u64,
            total_bytes,
            done_bytes,
            &cur,
            file_read,
            file_total,
            started,
        ));
    }

    log(
        if stats.failed == 0 && stats.errors == 0 {
            "ok"
        } else {
            "warn"
        },
        format!(
            "校验结束：通过 {}，失败 {}，读取错误 {}，共读取 {}，耗时 {:.1}s",
            stats.pass,
            stats.failed,
            stats.errors,
            human_bytes(stats.bytes),
            started.elapsed().as_secs_f64()
        ),
    );
    stats
}

#[allow(clippy::too_many_arguments)]
fn make_report(
    meter: &mut RateMeter,
    files_total: u64,
    files_done: u64,
    bytes_total: u64,
    bytes_done: u64,
    current_file: &str,
    file_done: u64,
    file_total: u64,
    started: Instant,
) -> ProgressReport {
    let speed = meter.speed();
    let remaining = bytes_total.saturating_sub(bytes_done);
    let eta = if speed > 1.0 {
        remaining as f64 / speed
    } else {
        0.0
    };
    ProgressReport {
        phase: "verify".into(),
        files_total,
        files_done,
        bytes_total,
        bytes_done,
        speed_bps: speed,
        eta_secs: eta,
        current_file: current_file.to_string(),
        current_file_done: file_done,
        current_file_total: file_total,
        elapsed_secs: started.elapsed().as_secs_f64(),
        // 校验阶段不做按源拆分：界面此时保留上一批数据，只把阶段标签换成「校验中」
        sources: Vec::new(),
    }
}
