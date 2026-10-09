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

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
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
    /// 用户手动「跳过此文件」的次数。**不计入 failed / errors** ——
    /// 跳过是用户主动选择，不算校验失败。
    pub skipped: u64,
    pub bytes: u64,
}

/// 构造一条「用户手动跳过校验」的结果行。
///
/// 状态用 `skip`（前端映射到「跳过」栏与 ⏭ 图标），说明里写明是**人为**跳过、
/// 以及跳过的位置，便于事后区分「没校验」和「校验通过」。
fn skip_result(src: &std::path::Path, dst: &std::path::Path, size: u64, read: u64) -> FileResult {
    FileResult {
        path: path_to_string(src),
        target: path_to_string(dst),
        size,
        status: "skip".into(),
        src_hash: String::new(),
        dst_hash: String::new(),
        message: if read == 0 {
            "用户手动跳过校验（未读取就跳过）".to_string()
        } else {
            format!("用户手动跳过校验（已读 {} 后跳过）", human_bytes(read))
        },
    }
}

/// 分片调试时用的分片大小。
///
/// 64 MiB 是个折中：太小 → 一个 400 GB 的文件会有几千片，扫一遍很久；
/// 太大 → 「第 137 片不同」换算出的偏移不够精确，指不回「上次中断在哪」。
const DEBUG_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

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
    skip: &AtomicBool,
    skip_folder: &Mutex<Option<PathBuf>>,
    total_bytes: u64,
    debug_chunk: bool,
) -> VerifyStats {
    let mut log = |level: &str, msg: String| events::emit_log(app, level, msg);
    let mut result = |r: &FileResult| events::emit_file_result(app, r);
    let mut progress = |p: &ProgressReport| events::emit_progress(app, p);
    run_verify_with_opts(
        items,
        idx,
        algo,
        cancel,
        skip,
        skip_folder,
        total_bytes,
        PROGRESS_MS,
        debug_chunk,
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
    // 保持旧签名不变（`tests/verify_progress.rs` 依赖它）：分片调试默认关，
    // 并给一个永远不会被置位的「跳过」标志（这些测试不涉及手动跳过）。
    let never = AtomicBool::new(false);
    let never_folder = Mutex::new(None);
    run_verify_with_opts(
        items, idx, algo, cancel, &never, &never_folder, total_bytes, progress_ms, false, sinks,
    )
}

/// 校验主体（带分片调试 + 单文件跳过 + 整个文件夹跳过）。
///
/// - `debug_chunk = true`：对不一致的文件额外按 [`DEBUG_CHUNK_SIZE`] 分片重算两边摘要，
///   指出**首个不一致的分片与偏移**。
/// - `skip`：用户点「跳过此文件」时被置位。**文件开头与每个读取块**都会检查它，
///   命中即放弃该文件（记 `status = "skip"`，**不计入失败**）并继续下一个。
/// - `skip_folder`：用户选「跳过整个文件夹」时写入目标目录。**每个文件开头**都会检查：
///   源路径属于该目录（含子目录）的，一律直接跳过（记 `status = "skip"`），
///   直到校验阶段结束。持续生效，不需要像 `skip` 那样取走消费。
pub fn run_verify_with_opts(
    items: &[PlanItem],
    idx: &[usize],
    algo: HashAlgo,
    cancel: &AtomicBool,
    skip: &AtomicBool,
    skip_folder: &Mutex<Option<PathBuf>>,
    total_bytes: u64,
    progress_ms: u64,
    debug_chunk: bool,
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
        // 用原始字节路径做哈希（哈希只校验文件内容，不校验文件名）；
        // `it.src`/`it.dst` 是 lossy 后的字符串，非法字节会被替换成 `�`，
        // 拿它去 open 会打不开真实文件。
        let src = &it.src_path;
        let dst = &it.dst_path;
        // 一个文件读「源 + 目标」两遍，所以「当前文件」的总量是 size × 2
        let file_total = it.size.saturating_mul(2);
        let cur = path_to_string(src);

        // ── 用户在**这个文件开始读之前**就点了「跳过此文件」──
        // 直接记一条跳过就走，连 open 都不做（可能是几百 GB，能省则省）。
        if skip.swap(false, Ordering::SeqCst) {
            stats.skipped += 1;
            let r = skip_result(src, dst, it.size, 0);
            log("warn", format!("{} {}", r.message, r.path));
            result_sink(&r);
            throttle.force();
            progress_sink(&make_report(
                &mut meter,
                files_total,
                (i + 1) as u64,
                total_bytes,
                done_bytes,
                &cur,
                0,          // 跳过的文件没读，file_done 记 0
                file_total, // 但「本文件总量」照报，界面不会显示成 0/0
                started,
            ));
            continue;
        }

        // ── 用户选了「跳过整个文件夹」── 当前文件属于该目录（含子目录）→ 直接跳过。
        //    与上面的单文件跳过互不冲突：文件夹跳过是**持续**生效的（不取走消费），
        //    直到校验阶段结束，期间该前缀下的所有剩余文件都会命中这里。
        let skip_folder = skip_folder.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(folder) = &skip_folder {
            if src.starts_with(folder) {
                stats.skipped += 1;
                let r = skip_result(src, dst, it.size, 0);
                log(
                    "warn",
                    format!("{}（整个文件夹 {}）{}", r.message, folder.display(), r.path),
                );
                result_sink(&r);
                throttle.force();
                progress_sink(&make_report(
                    &mut meter,
                    files_total,
                    (i + 1) as u64,
                    total_bytes,
                    done_bytes,
                    &cur,
                    0,
                    file_total,
                    started,
                ));
                continue;
            }
        }

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
            let a = hash::hash_file_skip(src, algo, cancel, skip, &mut on);
            // 源这一步已经失败（取消 / 跳过 / 真 IO 错）→ 不必再读目标，
            // 否则白读一整份文件。占位错误不会被展示：下面 match 的
            // 第一个分支 (Err(e), _) 会优先命中 `a` 的真实错误。
            let b = if a.is_err() {
                Err(io::Error::new(io::ErrorKind::Other, "src-not-read"))
            } else {
                hash::hash_file_skip(dst, algo, cancel, skip, &mut on)
            };
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
                    let mut msg = format!(
                        "{} 不一致（源 {} / 目标 {}，字节 {} vs {}）",
                        algo.label(),
                        a.short(8),
                        b.short(8),
                        na,
                        nb
                    );
                    // ---- 分片调试定位（可选）----
                    // 只看「整体不一致」没法判成因：是整份文件都不对（没真拷）？
                    // 还是前面对、从某个偏移开始错（续传拼接 / 中途写坏）？
                    // 分片摘要能直接给出「第几片开始不同」，乘上片大小就是首个坏字节偏移。
                    if debug_chunk {
                        match crate::posix::diff_chunks(
                            src,
                            dst,
                            DEBUG_CHUNK_SIZE,
                            algo,
                            cancel,
                            8,
                        ) {
                            Ok(d) => match d.first_bad {
                                Some(bi) => {
                                    let off = bi * d.chunk_size;
                                    let pct = if it.size > 0 {
                                        off as f64 / it.size as f64 * 100.0
                                    } else {
                                        0.0
                                    };
                                    msg.push_str(&format!(
                                        "；首个不一致分片 #{}（偏移 {}，占全文 {:.2}%，片大小 {}）",
                                        bi,
                                        human_bytes(off),
                                        pct,
                                        human_bytes(d.chunk_size)
                                    ));
                                    if d.bad.len() > 1 {
                                        msg.push_str(&format!("，共扫描到 {} 片不一致", d.bad.len()));
                                    }
                                    log(
                                        "error",
                                        format!(
                                            "分片定位：{} 第 {} 片起不同（源码 {} / 目标码 {}）",
                                            path_to_string(src),
                                            bi,
                                            d.bad.first().map(|c| c.hex.as_str()).unwrap_or("-"),
                                            d.bad_dst.first().map(|c| c.hex.as_str()).unwrap_or("-")
                                        ),
                                    );
                                    if bi == 0 {
                                        log(
                                            "warn",
                                            "首个分片就不同 → 目标那份很可能不是本次拷贝的产物（旧的同名文件 / 未真正写入）".to_string(),
                                        );
                                    } else {
                                        log(
                                            "warn",
                                            format!(
                                                "前 {} 片一致、从第 {} 片起不同 → 典型的「追加式损坏」或写入中途失效；偏移 {} 值得与上次中断时的文件长度对照",
                                                bi,
                                                bi,
                                                human_bytes(off)
                                            ),
                                        );
                                    }
                                }
                                None => msg.push_str("；分片复扫未发现差异（可能读取不稳定，建议重复读两遍比对）"),
                            },
                            Err(e) => msg.push_str(&format!("；分片调试失败：{e}")),
                        }
                    }
                    FileResult {
                        path: path_to_string(src),
                        target: path_to_string(dst),
                        size: it.size,
                        status: "fail".into(),
                        src_hash: a.hex().to_string(),
                        dst_hash: b.hex().to_string(),
                        message: msg,
                    }
                }
            }
            (Err(e), _) => {
                // ⚠️ 必须先于「读取失败」判定：跳过哨兵与取消的 ErrorKind 都是 Interrupted，
                // 判反了用户点「跳过」会看到「源文件读取失败」。
                if crate::posix::is_skip_err(&e) {
                    skip.store(false, Ordering::SeqCst); // 消费掉，别影响下一个文件
                    stats.skipped += 1;
                    skip_result(src, dst, it.size, file_read)
                } else {
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
            }
            (_, Err(e)) => {
                if crate::posix::is_skip_err(&e) {
                    skip.store(false, Ordering::SeqCst);
                    stats.skipped += 1;
                    skip_result(src, dst, it.size, file_read)
                } else {
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
            "校验结束：通过 {}，失败 {}，读取错误 {}{}，共读取 {}，耗时 {:.1}s",
            stats.pass,
            stats.failed,
            stats.errors,
            // 有手动跳过时必须报出来：否则「通过 + 失败 + 错误」凑不齐文件总数，
            // 用户会以为漏了文件（其实是被自己跳过的那几个）。
            if stats.skipped > 0 {
                format!("，手动跳过 {}", stats.skipped)
            } else {
                String::new()
            },
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
