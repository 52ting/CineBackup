//! 拷贝完成后的 xxHash64 校验阶段
//!
//! - 逐文件计算「源文件」与「目标同名文件」的 xxHash64 并比对
//! - 文件夹源：其下每个文件都已在 `Plan` 中被展开成独立条目，等价于递归遍历比对
//! - 读取失败（权限 / 盘掉线）→ 该文件标记为 error，不中断整体流程
//! - 单独统计校验速度与剩余时间（与拷贝阶段的速度互不干扰）

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tauri::AppHandle;

use crate::events::{self, ProgressReport};
use crate::hash::{self, hash_hex};
use crate::types::{FileResult, PlanItem};
use crate::util::{human_bytes, path_to_string, RateMeter, Throttle};

#[derive(Debug, Default, Clone, Copy)]
pub struct VerifyStats {
    pub pass: u64,
    pub failed: u64,
    pub errors: u64,
    pub bytes: u64,
}

/// 执行校验。
///
/// - `items`：完整计划；`idx`：本次需要校验的条目下标
///   （= 所有被写入过的文件 + 快速扫描模式下未做过哈希比对而被跳过的文件）
/// - `total_bytes`：本轮校验将读取的总字节数（= Σ size × 2，源一遍、目标一遍），
///   用于换算剩余时间
pub fn run_verify(
    app: &AppHandle,
    items: &[PlanItem],
    idx: &[usize],
    cancel: &AtomicBool,
    total_bytes: u64,
) -> VerifyStats {
    let mut stats = VerifyStats::default();
    let mut meter = RateMeter::new();
    let mut throttle = Throttle::new(150);
    let mut done_bytes: u64 = 0;
    let files_total = idx.len() as u64;
    let started = Instant::now();

    events::emit_log(
        app,
        "info",
        format!(
            "校验阶段开始：共 {} 个文件，需读取 {}（源 + 目标各一遍）",
            files_total,
            human_bytes(total_bytes)
        ),
    );

    for (i, &pos) in idx.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            events::emit_log(app, "warn", "校验被取消。");
            break;
        }
        let it = &items[pos];
        let src = Path::new(&it.src);
        let dst = Path::new(&it.dst);

        let mut file_read: u64 = 0;
        let mut on = |n: u64| {
            meter.add(n);
            done_bytes = done_bytes.saturating_add(n);
            file_read = file_read.saturating_add(n);
        };

        let h_src = hash::hash_file(src, cancel, &mut on);
        let h_dst = hash::hash_file(dst, cancel, &mut on);

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
                        src_hash: hash_hex(a),
                        dst_hash: hash_hex(b),
                        message: "xxHash64 一致".into(),
                    }
                } else {
                    stats.failed += 1;
                    FileResult {
                        path: path_to_string(src),
                        target: path_to_string(dst),
                        size: it.size,
                        status: "fail".into(),
                        src_hash: hash_hex(a),
                        dst_hash: hash_hex(b),
                        message: format!(
                            "哈希不一致（源 {} / 目标 {}，字节 {} vs {}）",
                            &hash_hex(a)[..8],
                            &hash_hex(b)[..8],
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
        events::emit_log(app, level, format!("{} {}", result.message, result.path));
        events::emit_file_result(app, &result);

        // 进度（含校验速度与剩余时间）
        if throttle.ready() || i + 1 == idx.len() {
            let speed = meter.speed();
            let remaining = total_bytes.saturating_sub(done_bytes);
            let eta = if speed > 1.0 { remaining as f64 / speed } else { 0.0 };
            let report = ProgressReport {
                phase: "verify".into(),
                files_total,
                files_done: (i + 1) as u64,
                bytes_total: total_bytes,
                bytes_done: done_bytes,
                speed_bps: speed,
                eta_secs: eta,
                current_file: result.path.clone(),
                current_file_done: file_read,
                current_file_total: it.size.saturating_mul(2),
                elapsed_secs: started.elapsed().as_secs_f64(),
                // 校验阶段不做按源拆分：界面此时保留上一批数据，只把阶段标签换成「校验中」
                sources: Vec::new(),
            };
            events::emit_progress(app, &report);
        }
    }

    events::emit_log(
        app,
        if stats.failed == 0 && stats.errors == 0 { "ok" } else { "warn" },
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
