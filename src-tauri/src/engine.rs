//! 任务调度引擎
//!
//! 整个流程都在**后台线程**里跑，UI 只通过事件接收进度：
//! ```text
//! 预扫描（建计划，可取消，持续上报进度）
//!    ↓  （Dry Run 到此结束）
//! 创建目标目录树
//!    ↓
//! 串行拷贝每个源（文件 / 文件夹），逐个文件按规则决定 拷贝 / 续传 / 跳过 / 覆盖
//!    ↓  （遇到同名冲突 → 阻塞等待弹窗回复）
//! 全量哈希校验（默认 SHA-256，见 `hash.rs`）
//!    ↓
//! 汇总 job-end
//! ```

use std::path::Path;
use std::time::Instant;

use tauri::{AppHandle, Manager};

use crate::copy::{self, CopyMode};
use crate::events::{
    self, ConflictAsk, CopyErrorAsk, PlanSummary, ProgressReport, ScanProgress, SourceProgress,
};
use crate::fsinfo;
use crate::scan;
use crate::state::AppState;
use crate::types::{
    FileResult, JobEnd, JobOptions, JobRequest, PlanItem, PlannedAction, UserReply,
};
use crate::util::{escape_path_bytes, human_bytes, path_to_string, sanitize_path_for_filesystem, RateMeter, Throttle};
use crate::verify;
use crate::walk;

/// 冲突的「粘性」决定：选了「全部跳过 / 全部覆盖」后，后续冲突不再弹窗
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sticky {
    SkipAll,
    OverwriteAll,
}

/// 启动任务（命令层调用；本函数立即返回，实际工作在后台线程）
pub fn spawn_job(app: AppHandle, req: JobRequest) {
    std::thread::Builder::new()
        .name("cinebackup-worker".into())
        .spawn(move || {
            let st = app.state::<AppState>();
            let result = run_job(&app, st.inner(), req);
            // 释放任务槽位 + 清掉可能的残留弹窗通道
            events::drop_pending_reply(st.inner());
            st.release();
            match result {
                Ok(end) => events::emit_job_end(&app, &end),
                Err(msg) => {
                    events::emit_log(&app, "error", format!("任务失败：{msg}"));
                    events::emit_status(&app, events::ST_IDLE);
                    events::emit_job_end(
                        &app,
                        &JobEnd {
                            ok: false,
                            message: msg,
                            ..Default::default()
                        },
                    );
                }
            }
        })
        .expect("无法创建后台工作线程");
}

// ================================================================ 对比校验（独立于备份任务）

/// 启动对比校验（后台线程）。
///
/// 与 `spawn_job` 同构，但走**独立**的状态槽位（`comparing` / `compare_cancel`）
/// 与独立事件，互不干扰 —— 对比是只读的，允许与备份任务同时跑。
pub fn spawn_compare(
    app: AppHandle,
    left: String,
    right: String,
    opts: crate::compare::CompareOptions,
) {
    std::thread::Builder::new()
        .name("cinebackup-compare".into())
        .spawn(move || {
            let st = app.state::<AppState>();
            let mut throttle = Throttle::new(150);
            let app_prog = app.clone();
            let res = crate::compare::run_compare(
                Path::new(&left),
                Path::new(&right),
                &opts,
                &st.compare_cancel,
                |p| {
                    if throttle.ready() {
                        events::emit_compare_progress(&app_prog, p);
                    }
                },
            );
            // 收尾强制补一次（小目录跑太快时，前面几次可能全被节流窗口吞掉）
            let total = res.items.len() as u64;
            events::emit_compare_progress(
                &app,
                &crate::compare::CompareProgress {
                    phase: "done".into(),
                    files_total: total,
                    files_done: total,
                    current: String::new(),
                    bytes_hashed: res.hashed_bytes,
                },
            );
            events::emit_log(
                &app,
                if res.ok { "ok" } else { "warn" },
                format!("对比校验：{}", res.message),
            );
            events::emit_compare_done(&app, &res);
            st.release_compare();
        })
        .expect("无法创建对比线程");
}

// ================================================================ 主流程

fn run_job(app: &AppHandle, st: &AppState, req: JobRequest) -> Result<JobEnd, String> {
    st.clear_cancel();
    let started = Instant::now();
    let target = Path::new(&req.target);

    events::emit_log(app, "info", "========================================");
    events::emit_log(
        app,
        "info",
        format!(
            "任务开始：{} 个源 → {}",
            req.sources.len(),
            path_to_string(target)
        ),
    );
    events::emit_log(
        app,
        "info",
        format!(
            "目标盘文件系统：{}（剩余空间 {}）",
            fsinfo::fs_type_of(target),
            human_bytes(free_space(target))
        ),
    );
    if fsinfo::is_readonly_on_macos(&fsinfo::fs_type_of(target)) {
        events::emit_log(
            app,
            "warn",
            "目标盘是 NTFS：Windows 下可正常写入；macOS 原生驱动只能只读，需第三方 NTFS 驱动。",
        );
    }

    // ---------------- 1. 预扫描 ----------------
    events::emit_status(app, events::ST_SCANNING);
    let last_scan = std::rc::Rc::new(std::cell::RefCell::new(ScanProgress {
        phase: "enum".into(),
        ..Default::default()
    }));
    let mut plan = {
        let app2 = app.clone();
        let last2 = last_scan.clone();
        let mut throttle = Throttle::new(120);
        let mut cb = move |sp: &ScanProgress| {
            *last2.borrow_mut() = sp.clone();
            if throttle.ready() {
                events::emit_scan(&app2, sp);
            }
        };
        scan::build_plan(&req.sources, target, &req.options, &st.cancel, &mut cb)?
    };
    // 最后一次扫描进度（强制推送，避免结束在 99%）
    events::emit_scan(app, &last_scan.borrow());

    if plan.cancelled {
        events::emit_log(app, "warn", "预扫描被取消。");
        events::emit_status(app, events::ST_IDLE);
        return Ok(JobEnd {
            ok: false,
            aborted: true,
            message: "预扫描被取消".into(),
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..Default::default()
        });
    }

    for w in &plan.warnings {
        events::emit_log(app, "warn", w.clone());
    }

    // 文件名预检：目标文件系统大概率存不下的名字，开拷前一次性报出来，
    // 免得拷到一半才因为 EILSEQ 失败（见 namecheck.rs 模块注释）。
    if !plan.name_issues.is_empty() {
        events::emit_log(
            app,
            "warn",
            format!(
                "文件名预检：发现 {} 个名字目标盘可能存不下（含冒号 :、非 UTF-8、控制字符、尾随空格/点、超长或 Windows 保留名）。",
                plan.name_issues.len()
            ),
        );
        for issue in &plan.name_issues {
            let why = issue
                .reasons
                .iter()
                .map(|r| crate::namecheck::reason_label(r))
                .collect::<Vec<_>>()
                .join("、");
            events::emit_log(
                app,
                "warn",
                format!(
                    "  · {}：名字「{}」{}，建议改为「{}」",
                    issue.path, issue.name_display, why, issue.suggestion
                ),
            );
        }
        events::emit_log(
            app,
            "warn",
            "  这些文件仍会按原样尝试拷贝；若失败，请按建议改名后重跑（半截文件会自动续传，不会重复拷）。",
        );
    }
    if plan.items.is_empty() {
        events::emit_log(app, "error", "没有找到任何可备份的文件。");
        events::emit_status(app, events::ST_IDLE);
        return Ok(JobEnd {
            ok: false,
            message: "没有找到任何可备份的文件".into(),
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..Default::default()
        });
    }

    events::emit_log(
        app,
        "ok",
        format!(
            "预扫描完成：共 {} 个文件、{} 个目录{}；完整拷贝 {}，续传 {}，跳过 {}，覆盖 {}{}",
            plan.items.len(),
            plan.dirs.len(),
            if plan.stats.filtered > 0 {
                format!("（已过滤系统元数据 {} 项）", plan.stats.filtered)
            } else {
                String::new()
            },
            plan.stats.copy,
            plan.stats.resume,
            plan.stats.skip,
            plan.stats.overwrite,
            if plan.stats.conflict > 0 {
                format!("，待询问 {}", plan.stats.conflict)
            } else {
                String::new()
            },
        ),
    );
    events::emit_plan(
        app,
        &PlanSummary {
            copy: plan.stats.copy,
            resume: plan.stats.resume,
            skip: plan.stats.skip,
            overwrite: plan.stats.overwrite,
            conflict: plan.stats.conflict,
            total_bytes: plan.stats.total_bytes,
            filtered: plan.stats.filtered,
        },
    );

    // ---------------- 2. Dry Run 到此结束 ----------------
    if req.dry_run {
        emit_dry_run_preview(app, &plan.items);
        events::emit_status(app, events::ST_DONE);
        events::emit_log(
            app,
            "ok",
            format!(
                "Dry Run 结束：预计写入 {}，未执行任何写操作。",
                human_bytes(plan.stats.total_bytes)
            ),
        );
        return Ok(JobEnd {
            ok: true,
            dry_run: true,
            skipped: plan.items.len() as u64,
            total_bytes: plan.stats.total_bytes,
            elapsed_secs: started.elapsed().as_secs_f64(),
            message: "试运行完成，未写入任何数据".into(),
            ..Default::default()
        });
    }

    // ---------------- 3. 创建目标目录树 ----------------
    events::emit_log(app, "info", format!("创建目标目录树（{} 个目录）…", plan.dirs.len()));
    for d in &plan.dirs {
        if st.cancelled() {
            break;
        }
        if let Err(e) = copy::ensure_dir(d) {
            events::emit_log(app, "error", format!("创建目录失败 {}：{e}", path_to_string(d)));
        }
    }

    // ---------------- 4. 拷贝阶段 ----------------
    let outcome = if req.options.skip_copy {
        events::emit_log(
            app,
            "warn",
            "已勾选「跳过拷贝」：本次不写入任何数据，直接进入校验阶段（只核验目标里已有的同名文件）。",
        );
        CopyOutcome::default()
    } else {
        events::emit_status(app, events::ST_COPYING);
        run_copy_phase(app, st, &mut plan, &req.options, &req.sources)?
    };

    // ---------------- 5. 校验阶段 ----------------
    let mut end = JobEnd {
        ok: true,
        aborted: outcome.aborted,
        copied: outcome.copied,
        resumed: outcome.resumed,
        overwritten: outcome.overwritten,
        skipped: outcome.skipped,
        failed: outcome.errors,
        total_bytes: outcome.written_bytes,
        elapsed_secs: started.elapsed().as_secs_f64(),
        ..Default::default()
    };
    if outcome.errors > 0 {
        events::emit_log(
            app,
            "warn",
            format!("拷贝阶段共有 {} 个文件失败（明细见上方日志与结果列表）。", outcome.errors),
        );
    }

    if outcome.aborted {
        events::emit_log(app, "warn", "任务被用户终止，跳过校验阶段。");
    } else if req.options.skip_verify {
        events::emit_log(
            app,
            "info",
            "已勾选「跳过校验」：本次任务不做内容哈希校验（跳过的文件已登记到结果表）。",
        );
        emit_verify_skipped_results(app, &plan.items, &outcome);
    } else if req.options.verify_after_copy {
        events::emit_status(app, events::ST_VERIFYING);
        let (idx, pres, _unverified_skips) =
            build_verify_set(app, &plan.items, &req.options, &outcome.failed_idx);
        end.pass = pres;
        let total: u64 = idx.iter().map(|&i| plan.items[i].size.saturating_mul(2)).sum();
        if idx.is_empty() {
            events::emit_log(app, "info", "没有需要校验的文件。");
        } else {
            let vs = verify::run_verify(
                app,
                &plan.items,
                &idx,
                req.options.hash_algo,
                &st.cancel,
                total,
                req.options.debug_chunk_hash,
            );
            end.pass += vs.pass;
            end.failed += vs.failed + vs.errors;
        }
    } else {
        events::emit_log(app, "info", "已按选项跳过校验阶段。");
    }

    // ---------------- 6. 收尾 ----------------
    end.elapsed_secs = started.elapsed().as_secs_f64();
    end.ok = !end.aborted && end.failed == 0;
    events::emit_status(app, if end.aborted { events::ST_IDLE } else { events::ST_DONE });
    events::emit_log(
        app,
        if end.ok { "ok" } else { "warn" },
        format!(
            "任务{}：拷贝 {} / 续传 {} / 覆盖 {} / 跳过 {}，写入 {}，耗时 {:.1}s{}",
            if end.aborted { "中止" } else { "完成" },
            end.copied,
            end.resumed,
            end.overwritten,
            end.skipped,
            human_bytes(end.total_bytes),
            end.elapsed_secs,
            if end.failed > 0 {
                format!("，校验失败 {}", end.failed)
            } else {
                String::new()
            }
        ),
    );
    Ok(end)
}

// ================================================================ 拷贝阶段

/// 错误弹窗无操作多少秒后自动跳过当前文件（前端用它显示倒计时，后端到点也按此超时）
const ERROR_DIALOG_TIMEOUT_SECS: u64 = 120;

/// 把 IO 错误粗分类 —— 结果表「说明」列用 `[分类]` 前缀展示，便于按类排查
/// （例如一批文件全是「文件名非法」，就知道该去改名而不是怀疑磁盘）。
fn classify_io_error(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind as K;
    // 先看原始 errno（比 ErrorKind 更精确，跨平台一致）
    match e.raw_os_error() {
        Some(92) => return "文件名非法",   // EILSEQ：目标文件系统拒这个名字
        Some(28) => return "磁盘空间不足", // ENOSPC
        Some(13) => return "权限不足",     // EACCES
        Some(2) => return "路径不存在",    // ENOENT
        Some(5) => return "目标只读或权限不足", // EIO / EROFS 常见组合场景
        _ => {}
    }
    match e.kind() {
        K::PermissionDenied => "权限不足",
        K::NotFound => "路径不存在",
        K::AlreadyExists => "目标已存在",
        K::Interrupted => "已取消",
        K::UnexpectedEof => "文件意外截断",
        K::WriteZero => "写入失败（磁盘可能已满）",
        _ => "IO 错误",
    }
}

#[derive(Default)]
struct CopyOutcome {
    copied: u64,
    resumed: u64,
    overwritten: u64,
    skipped: u64,
    errors: u64,
    written_bytes: u64,
    aborted: bool,
    /// 拷贝阶段已经失败并单独报过错的条目下标（校验阶段不再重复统计）
    failed_idx: Vec<usize>,
}

fn run_copy_phase(
    app: &AppHandle,
    st: &AppState,
    plan: &mut scan::Plan,
    opts: &JobOptions,
    sources: &[String],
) -> Result<CopyOutcome, String> {
    let mut out = CopyOutcome::default();
    let mut sticky: Option<Sticky> = None;
    let mut meter = RateMeter::new();
    let mut throttle = Throttle::new(150);
    let started = Instant::now();

    let files_total = plan.items.len() as u64;
    let mut files_done: u64 = 0;

    // ---- 按源分组统计：界面中间栏是「一行一个源」的传输列表 ----
    let mut src_prog: Vec<SourceProgress> = sources
        .iter()
        .enumerate()
        .map(|(i, p)| SourceProgress {
            index: i,
            path: p.clone(),
            state: "waiting".into(),
            ..Default::default()
        })
        .collect();

    // ---- 预计算总量：按「文件完整长度」计（续传时已存在的部分预先算作已完成）----
    let mut bytes_total: u64 = 0;
    let mut bytes_done: u64 = 0;
    for it in plan.items.iter() {
        // 文件数按归属计入（含最终会跳过的），字节只算真要写盘的
        if let Some(sp) = src_prog.get_mut(it.src_idx) {
            sp.files_total += 1;
        }
        if it.action.needs_write() {
            bytes_total = bytes_total.saturating_add(it.size);
            if let Some(sp) = src_prog.get_mut(it.src_idx) {
                sp.bytes_total = sp.bytes_total.saturating_add(it.size);
                if it.action == PlannedAction::Resume {
                    sp.bytes_done = sp.bytes_done.saturating_add(it.existing_size);
                }
            }
            if it.action == PlannedAction::Resume {
                bytes_done = bytes_done.saturating_add(it.existing_size);
            }
        }
    }

    for i in 0..plan.items.len() {
        if st.cancelled() {
            out.aborted = true;
            break;
        }
        // 关键：拷贝/哈希/校验一律用原始字节路径（src_path/dst_path），
        // 绝不用 lossy 后的 src/dst 字符串 —— 非法 UTF-8 字节会被替换成 `�`，
        // 导致 File::open 打不开真实文件。src/dst 只用于日志与前端展示。
        let src = plan.items[i].src_path.clone();
        let dst = plan.items[i].dst_path.clone();
        // 遇到非法 UTF-8 文件名 → 打警告日志标记，但**继续拷贝**（不阻断）。
        warn_if_non_utf8(app, &src, &dst);
        let size = plan.items[i].size;
        let existing = plan.items[i].existing_size;
        let mut action = plan.items[i].action;

        // 这个文件归属哪个源。串行执行 ⇒ 下标更小的源必然已经处理完
        let si = plan.items[i].src_idx;
        for sp in src_prog.iter_mut() {
            if sp.index < si && sp.state != "done" && sp.state != "failed" {
                sp.state = "done".into();
                sp.current_file.clear();
            }
        }
        if let Some(sp) = src_prog.get_mut(si) {
            sp.state = "active".into();
            sp.current_file = plan.items[i].src.clone();
        }

        // ---- 处理「待询问」的冲突 ----
        if action == PlannedAction::Conflict {
            let suggested = plan.items[i].suggested;
            let decision = match sticky {
                Some(Sticky::SkipAll) => PlannedAction::Skip,
                Some(Sticky::OverwriteAll) => apply_overwrite(suggested),
                None => {
                    let reply = events::ask_user(
                        app,
                        st,
                        events::EV_CONFLICT,
                        ConflictAsk {
                            src: plan.items[i].src.clone(),
                            dst: plan.items[i].dst.clone(),
                            src_size: size,
                            dst_size: existing,
                            suggested: action_name(suggested).to_string(),
                            reason: plan.items[i].reason.clone(),
                        },
                        0, // 冲突不自动超时：跳过 / 覆盖都是不可逆决定，必须用户明确选择
                    );
                    match reply {
                        Some(UserReply::Skip) => PlannedAction::Skip,
                        Some(UserReply::Overwrite) => apply_overwrite(suggested),
                        Some(UserReply::SkipAll) => {
                            sticky = Some(Sticky::SkipAll);
                            events::emit_log(app, "warn", "已选择「全部跳过」，后续冲突不再询问。");
                            PlannedAction::Skip
                        }
                        Some(UserReply::OverwriteAll) => {
                            sticky = Some(Sticky::OverwriteAll);
                            events::emit_log(app, "warn", "已选择「全部覆盖」，后续冲突不再询问。");
                            apply_overwrite(suggested)
                        }
                        // 弹窗期间点了取消
                        None => {
                            out.aborted = true;
                            break;
                        }
                        _ => PlannedAction::Skip,
                    }
                }
            };
            action = decision;
            // 修正总量估算
            let old_needed = plan.items[i].needed_bytes;
            bytes_total = bytes_total.saturating_sub(old_needed);
            bytes_total = bytes_total.saturating_add(if action.needs_write() { size } else { 0 });
            if action == PlannedAction::Resume {
                bytes_done = bytes_done.saturating_add(existing);
            }
            // 同一个修正要同步到该源的统计上，否则界面那一行的进度条会算错
            if let Some(sp) = src_prog.get_mut(si) {
                sp.bytes_total = sp.bytes_total.saturating_sub(old_needed);
                sp.bytes_total = sp
                    .bytes_total
                    .saturating_add(if action.needs_write() { size } else { 0 });
                if action == PlannedAction::Resume {
                    sp.bytes_done = sp.bytes_done.saturating_add(existing);
                }
            }
            plan.items[i].needed_bytes = if action.needs_write() { size } else { 0 };
        }

        // ---- 跳过 ----
        if action == PlannedAction::Skip {
            if plan.items[i].hash_checked {
                events::emit_log(
                    app,
                    "info",
                    format!("跳过（预扫描已确认 {} 一致）：{}", opts.hash_algo.label(), plan.items[i].src),
                );
            } else if plan.items[i].action == PlannedAction::Conflict {
                events::emit_log(app, "warn", format!("按用户选择跳过：{}", plan.items[i].src));
            } else {
                events::emit_log(app, "info", format!("跳过：{}", plan.items[i].src));
            }
            out.skipped += 1;
            files_done += 1;
            if let Some(sp) = src_prog.get_mut(si) {
                sp.files_done += 1;
            }
            plan.items[i].final_action = Some(PlannedAction::Skip);
            continue;
        }

        // ---- 决定拷贝模式 ----
        let mode = match action {
            PlannedAction::Resume => CopyMode::Resume,
            PlannedAction::Overwrite => CopyMode::Overwrite,
            _ => CopyMode::Fresh,
        };
        // `base` 仍要可变：posix 报告 restarted 时会把预记的已完成字节退回
        let mut base = if mode == CopyMode::Resume { existing } else { 0 };

        // 续传前缀校验已下沉到 posix::copy_data_fork（与拷贝同一个读循环，
        // 避免「校验读一套、拷贝读一套」造成语义分叉，也避免重复读两遍前缀）。
        if st.cancelled() {
            out.aborted = true;
            break;
        }

        plan.items[i].final_action = Some(action);
        let action_label = action_name(action).to_string();
        events::emit_log(
            app,
            "info",
            format!(
                "[{}] {} （{}）",
                action_label,
                path_to_string(&src),
                human_bytes(size)
            ),
        );

        // base 已经算作「进度内的已完成字节」
        if base > 0 {
            bytes_done = bytes_done.saturating_add(base);
            if let Some(sp) = src_prog.get_mut(si) {
                sp.bytes_done = sp.bytes_done.saturating_add(base);
            }
        }

        // ---- 真正拷贝 ----
        let mut file_written: u64 = 0;
        // 拷贝可能因为「目标文件名含 APFS 会拒绝的未分配 Unicode 码位」而 EILSEQ，
        // 此时自动用清洗后的路径重试一次（替换被拒码位为 U+FFFD），并在 plan 里
        // 记下「实际拷到的目标路径」，确保后续 verify 能找到。
        let mut current_dst = dst.clone();
        let mut res = {
            let mut on_bytes = |delta: u64| -> bool {
                meter.add(delta);
                file_written = file_written.saturating_add(delta);
                bytes_done = bytes_done.saturating_add(delta);
                if let Some(sp) = src_prog.get_mut(si) {
                    sp.bytes_done = sp.bytes_done.saturating_add(delta);
                }
                if throttle.ready() {
                    emit_copy_progress(
                        app,
                        &mut meter,
                        files_total,
                        files_done,
                        bytes_total,
                        bytes_done,
                        &plan.items[i].src,
                        base + file_written,
                        size,
                        started,
                        &src_prog,
                    );
                }
                !st.cancelled()
            };
            copy::copy_file_with(
                &src,
                &current_dst,
                mode,
                base,                       // 快照长度提示（实测不符会被 posix 拒绝并重写）
                opts.resume_prefix_check,   // 关掉校验 → posix 退化为从头重写，不盲拼
                &st.cancel,
                &mut on_bytes,
            )
        };

        // ---- 自动重试：遇 EILSEQ 且目标路径含被拒码位 ----
        let mut retried = false;
        let should_retry = matches!(res, Err(ref e) if e.raw_os_error() == Some(92))
            && crate::util::path_has_rejected_codepoint(&current_dst)
            && !st.cancelled();
        if should_retry {
            let sanitized = sanitize_path_for_filesystem(&current_dst);
            if sanitized != current_dst {
                events::emit_log(
                    app,
                    "warn",
                    format!(
                        "目标路径含 APFS 会拒绝的未分配 Unicode 码位，自动用清洗后的路径重试：\n  原：{}\n  新：{}",
                        escape_path_bytes(&current_dst),
                        path_to_string(&sanitized),
                    ),
                );
                current_dst = sanitized.clone();
                let mut on_bytes_retry = |delta: u64| -> bool {
                    meter.add(delta);
                    file_written = file_written.saturating_add(delta);
                    bytes_done = bytes_done.saturating_add(delta);
                    if let Some(sp) = src_prog.get_mut(si) {
                        sp.bytes_done = sp.bytes_done.saturating_add(delta);
                    }
                    if throttle.ready() {
                        emit_copy_progress(
                            app,
                            &mut meter,
                            files_total,
                            files_done,
                            bytes_total,
                            bytes_done,
                            &plan.items[i].src,
                            base + file_written,
                            size,
                            started,
                            &src_prog,
                        );
                    }
                    !st.cancelled()
                };
                res = copy::copy_file_with(
                    &src,
                    &current_dst,
                    mode,
                    base,
                    opts.resume_prefix_check,
                    &st.cancel,
                    &mut on_bytes_retry,
                );
                if res.is_ok() {
                    retried = true;
                    // 用 sanitized 覆盖 plan，让 verify 找得到
                    plan.items[i].dst_path = sanitized.clone();
                } else {
                    // 重试也失败 → 恢复原 dst 让错误信息打印原路径
                    current_dst = dst.clone();
                }
            }
        }
        // 用最终目标路径回填 dst（保持后续 Ok 分支里 path_to_string(&dst) 也指向实际成功的路径）
        let dst = current_dst;

        match res {
            Ok(o) => {
                if retried {
                    // 自动改名重试成功 → 后续 verify 用的是 dst_path（已覆盖），这里只补一行 OK 日志
                    events::emit_log(
                        app,
                        "info",
                        format!(
                            "目标路径清洗后写入完成：{}（已自动替换未分配 Unicode 码位为 U+FFFD）",
                            path_to_string(&dst)
                        ),
                    );
                }
                if o.restarted {
                    // 本想续传但实际改为从头重写 → 之前记账的 base 需要退回
                    bytes_done = bytes_done.saturating_sub(base);
                    if let Some(sp) = src_prog.get_mut(si) {
                        sp.bytes_done = sp.bytes_done.saturating_sub(base);
                    }
                    base = 0;
                    // 区分原因：开着校验说明是「前缀真的对不上」（多半是旧版本残留）；
                    // 关着校验说明是我们主动保守重写。两种都不再拼出坏文件。
                    events::emit_log(
                        app,
                        "warn",
                        if o.prefix_checked {
                            format!(
                                "目标已存在部分与源不一致（可能是旧版本残留），已改为从头完整重写：{}",
                                path_to_string(&dst)
                            )
                        } else {
                            format!(
                                "续传前校验未开启 → 为避免拼接出错误内容，已改为从头完整重写：{}",
                                path_to_string(&dst)
                            )
                        },
                    );
                } else if o.prefix_checked && o.base > 0 {
                    events::emit_log(
                        app,
                        "info",
                        format!(
                            "续传：已校验前缀 {} 一致，从该位置继续写入。",
                            human_bytes(o.base)
                        ),
                    );
                }
                out.written_bytes = out.written_bytes.saturating_add(o.written);

                // ---- 写出后的收尾（都是尽力而为，失败只记日志，不影响拷贝结论）----
                //
                // ① 清掉 macOS 隔离属性 com.apple.quarantine。
                //    不清的话，目标文件在 Mac 上会被 Gatekeeper 当成「下载来的不可信文件」，
                //    直接双击可能提示「已损坏」；对素材盘来说这是纯干扰。
                //    它属于 **xattr（元数据）**，删掉不影响 Data Fork 内容，也就不会影响校验值。
                match crate::posix::remove_quarantine(&dst) {
                    Ok(true) => events::emit_log(
                        app,
                        "info",
                        format!("已清除隔离属性 {}：{}", crate::posix::QUARANTINE_XATTR, path_to_string(&dst)),
                    ),
                    Ok(false) => {} // 本来就没有 —— 正常情况，不刷日志
                    Err(e) => events::emit_log(
                        app,
                        "warn",
                        format!("清除隔离属性失败（不影响拷贝结果）：{} —— {e}", path_to_string(&dst)),
                    ),
                }

                // ② 可选：复制源的元数据（权限 / 时间戳 / xattr）。
                //    默认关闭。⚠️ 资源分支永不复制（见 posix::copy_metadata 的说明）。
                if opts.copy_metadata {
                    for w in crate::posix::copy_metadata(&src, &dst) {
                        if let Some(n) = w.strip_prefix("__COPIED_XATTRS__") {
                            events::emit_log(app, "info", format!("已复制 {n} 个扩展属性（xattr）"));
                        } else {
                            events::emit_log(app, "warn", format!("元数据：{w}"));
                        }
                    }
                }

                match action {
                    PlannedAction::Resume => out.resumed += 1,
                    PlannedAction::Overwrite => out.overwritten += 1,
                    _ => out.copied += 1,
                }
                events::emit_log(
                    app,
                    "ok",
                    format!(
                        "写入完成 {}{}",
                        human_bytes(o.written),
                        if base > 0 {
                            format!("（续传自 {}）", human_bytes(base))
                        } else {
                            String::new()
                        }
                    ),
                );
            }
            Err(e) => {
                if st.cancelled() || e.kind() == std::io::ErrorKind::Interrupted {
                    out.aborted = true;
                    events::emit_log(app, "warn", "任务已取消，停止拷贝。");
                    break;
                }
                out.written_bytes = out.written_bytes.saturating_add(file_written);
                // 这个文件本次没成功拷过去 → 不参与后续校验（避免把「半截文件」报成校验失败）
                out.failed_idx.push(i);
                let err_text = e.to_string();
                let err_kind = classify_io_error(&e);
                // 把源文件名里的不可见/非法字节转义出来，帮用户一眼看出「名字本身」的问题
                let name_hint = crate::namecheck::check_name(Path::new(&src).file_name().unwrap_or_default())
                    .map(|(disp, _, _)| format!("（文件名含非法/不可见字符，转义后：「{disp}」）"))
                    .unwrap_or_default();
                // EILSEQ 时把目标路径的原始字节转义出来 —— 这是定位「真凶字符」的关键证据
                let dst_escaped = if e.raw_os_error() == Some(92) {
                    format!("\n  目标路径字节：{}", escape_path_bytes(&dst))
                } else {
                    String::new()
                };
                events::emit_log(
                    app,
                    "error",
                    format!(
                        "拷贝失败（{err_kind}）：{} —— {err_text}{name_hint}（目标：{}{}）",
                        path_to_string(&src),
                        path_to_string(&dst),
                        dst_escaped,
                    ),
                );
                // 弹窗询问；120 秒无操作 → 自动跳过当前文件，继续下一个（不终止整个任务）
                let reply = events::ask_user(
                    app,
                    st,
                    events::EV_COPY_ERROR,
                    CopyErrorAsk {
                        src: path_to_string(&src),
                        dst: path_to_string(&dst),
                        // 把字节转义也送进弹窗：EILSEQ 时即便日志面板被截，弹窗也是完整的。
                        // 用 err_text 双重兜底（包含 byte sequence 子串即视作 EILSEQ），
                        // 因为某些情况下 raw_os_error() 被包装后返回 None。
                        error: if e.raw_os_error() == Some(92)
                            || err_text.to_lowercase().contains("byte sequence")
                        {
                            format!(
                                "[{err_kind}] {err_text}\n  目标路径字节：{}",
                                escape_path_bytes(&dst)
                            )
                        } else {
                            format!("[{err_kind}] {err_text}")
                        },
                        done: files_done,
                        total: files_total,
                        timeout_secs: ERROR_DIALOG_TIMEOUT_SECS,
                    },
                    ERROR_DIALOG_TIMEOUT_SECS,
                );

                // 超时（后端自动返回 Skip）与用户点「跳过此文件」都算「跳过」：
                // 结果表归到「跳过」栏，说明列带上错误归类，便于批量排查。
                let timed_out = matches!(reply, Some(UserReply::Skip));
                let skip_file = matches!(reply, Some(UserReply::Continue) | Some(UserReply::Skip));
                let label = if timed_out {
                    format!("弹窗 {} 秒无操作，自动跳过", ERROR_DIALOG_TIMEOUT_SECS)
                } else if skip_file {
                    "已跳过".to_string()
                } else {
                    "拷贝失败".to_string()
                };
                if let Some(sp) = src_prog.get_mut(si) {
                    sp.state = if skip_file { "skipped" } else { "failed" }.into();
                }
                if skip_file {
                    out.skipped += 1;
                    events::emit_log(
                        app,
                        "warn",
                        format!(
                            "{label}：{}（继续处理下一个文件）",
                            path_to_string(&src)
                        ),
                    );
                } else {
                    out.errors += 1;
                }
                events::emit_file_result(
                    app,
                    &FileResult {
                        path: path_to_string(&src),
                        target: path_to_string(&dst),
                        size,
                        status: if skip_file { "skip".into() } else { "error".into() },
                        src_hash: String::new(),
                        dst_hash: String::new(),
                        message: format!("[{err_kind}] {label}：{err_text}{name_hint}"),
                    },
                );
                if !skip_file {
                    out.aborted = true;
                    events::emit_log(app, "warn", "已选择终止全部任务。");
                    break;
                }
            }
        }

        files_done += 1;
        if let Some(sp) = src_prog.get_mut(si) {
            sp.files_done += 1;
        }
        // 文件结束时的收尾进度上报
        emit_copy_progress(
            app,
            &mut meter,
            files_total,
            files_done,
            bytes_total,
            bytes_done,
            &plan.items[i].src,
            size,
            size,
            started,
            &src_prog,
        );
        throttle.force();
    }

    // 收尾：还挂在「排队中 / 进行中」的源统一收口（取消时会剩下一些），
    // 否则界面那几行会永远停在转圈状态
    for sp in src_prog.iter_mut() {
        if sp.state == "waiting" || sp.state == "active" {
            sp.state = "done".into();
            sp.current_file.clear();
        }
    }

    // 推送一次最终进度，避免进度条停在 99%
    emit_copy_phase_end(app, bytes_total, bytes_done, started, &src_prog);
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn emit_copy_progress(
    app: &AppHandle,
    meter: &mut RateMeter,
    files_total: u64,
    files_done: u64,
    bytes_total: u64,
    bytes_done: u64,
    current: &str,
    file_done: u64,
    file_total: u64,
    started: Instant,
    sources: &[SourceProgress],
) {
    let speed = meter.speed();
    let remaining = bytes_total.saturating_sub(bytes_done);
    let eta = if speed > 1.0 { remaining as f64 / speed } else { 0.0 };
    events::emit_progress(
        app,
        &ProgressReport {
            phase: "copy".into(),
            files_total,
            files_done,
            bytes_total,
            bytes_done,
            speed_bps: speed,
            eta_secs: eta,
            current_file: current.to_string(),
            current_file_done: file_done,
            current_file_total: file_total,
            elapsed_secs: started.elapsed().as_secs_f64(),
            sources: sources.to_vec(),
        },
    );
}

fn emit_copy_phase_end(
    app: &AppHandle,
    bytes_total: u64,
    bytes_done: u64,
    started: Instant,
    sources: &[SourceProgress],
) {
    events::emit_progress(
        app,
        &ProgressReport {
            phase: "copy".into(),
            files_total: 0,
            files_done: 0,
            bytes_total,
            bytes_done,
            speed_bps: 0.0,
            eta_secs: 0.0,
            current_file: String::new(),
            current_file_done: 0,
            current_file_total: 0,
            elapsed_secs: started.elapsed().as_secs_f64(),
            sources: sources.to_vec(),
        },
    );
}

// ================================================================ 校验集

/// 决定哪些条目需要（或在语义上无需）进入校验阶段。
/// 返回 `(需要读取校验的下标, 预扫描已确认通过的数量, 未校验直接跳过的数量)`
fn build_verify_set(
    app: &AppHandle,
    items: &[PlanItem],
    opts: &JobOptions,
    failed_idx: &[usize],
) -> (Vec<usize>, u64, u64) {
    let mut idx = Vec::new();
    let mut pre_pass: u64 = 0;
    let mut skipped: u64 = 0;
    let failed: std::collections::HashSet<usize> = failed_idx.iter().copied().collect();

    for (i, it) in items.iter().enumerate() {
        // 拷贝阶段已经失败并报过错的，不再重复统计
        if failed.contains(&i) {
            skipped += 1;
            continue;
        }
        match it.final_action.unwrap_or(it.action) {
            PlannedAction::Skip => {
                if it.hash_checked {
                    // 预扫描已经比对过内容哈希，直接判定通过
                    pre_pass += 1;
                    events::emit_file_result(
                        app,
                        &FileResult {
                            path: it.src.clone(),
                            target: it.dst.clone(),
                            size: it.size,
                            status: "pass".into(),
                            src_hash: String::new(),
                            dst_hash: String::new(),
                            message: format!("跳过（预扫描 {} 已一致）", opts.hash_algo.label()),
                        },
                    );
                } else if opts.quick_scan && opts.verify_after_copy {
                    // 快速扫描跳过的文件 → 补做全量校验
                    idx.push(i);
                } else {
                    skipped += 1;
                    events::emit_file_result(
                        app,
                        &FileResult {
                            path: it.src.clone(),
                            target: it.dst.clone(),
                            size: it.size,
                            status: "skip".into(),
                            src_hash: String::new(),
                            dst_hash: String::new(),
                            message: "按选择跳过，未校验".into(),
                        },
                    );
                }
            }
            PlannedAction::Copy | PlannedAction::Resume | PlannedAction::Overwrite => {
                if opts.skip_copy {
                    // 跳过拷贝模式：这些文件本次并没有写入（目标不存在或需重写），无从校验
                    skipped += 1;
                    events::emit_file_result(
                        app,
                        &FileResult {
                            path: it.src.clone(),
                            target: it.dst.clone(),
                            size: it.size,
                            status: "skip".into(),
                            src_hash: String::new(),
                            dst_hash: String::new(),
                            message: "[跳过拷贝] 本次未写入该文件（目标无对应文件或需重写），未校验".into(),
                        },
                    );
                } else if opts.verify_after_copy {
                    idx.push(i);
                }
            }
            PlannedAction::Conflict => {
                // 理论上不会残留（运行时已全部解决），防御性按跳过处理
                skipped += 1;
            }
        }
    }
    (idx, pre_pass, skipped)
}

/// 勾选「跳过校验」时，把每个文件都登记成「跳过」结果 —— 用户要求跳过的任务
/// 也要在结果表的「跳过」栏里看得到，而不是一整轮跑完结果表空着。
fn emit_verify_skipped_results(app: &AppHandle, items: &[PlanItem], outcome: &CopyOutcome) {
    let failed: std::collections::HashSet<usize> = outcome.failed_idx.iter().copied().collect();
    for (i, it) in items.iter().enumerate() {
        // 拷贝阶段已经单独报过的（跳过 / 失败）不再重复登记
        if failed.contains(&i) {
            continue;
        }
        let act = it.final_action.unwrap_or(it.action);
        let msg = if act == PlannedAction::Skip && it.hash_checked {
            "[跳过校验] 预扫描阶段已确认与目标内容一致".to_string()
        } else {
            "[跳过校验] 已按选项跳过内容校验".to_string()
        };
        events::emit_file_result(
            app,
            &FileResult {
                path: it.src.clone(),
                target: it.dst.clone(),
                size: it.size,
                status: "skip".into(),
                src_hash: String::new(),
                dst_hash: String::new(),
                message: msg,
            },
        );
    }
}

/// Dry Run 结果预览：把「将会发生什么」逐条列出来
fn emit_dry_run_preview(app: &AppHandle, items: &[PlanItem]) {
    for it in items {
        let (status, message) = match it.action {
            PlannedAction::Copy => ("copy", format!("【Dry Run】将完整拷贝 · {}", it.reason)),
            PlannedAction::Resume => (
                "copy",
                format!(
                    "【Dry Run】将断点续传（续写 {}）· {}",
                    human_bytes(it.needed_bytes),
                    it.reason
                ),
            ),
            PlannedAction::Overwrite => ("copy", format!("【Dry Run】将覆盖 · {}", it.reason)),
            PlannedAction::Skip => ("skip", format!("【Dry Run】将跳过 · {}", it.reason)),
            PlannedAction::Conflict => (
                "skip",
                format!("【Dry Run】存在同名冲突，运行时会弹出询问 · {}", it.reason),
            ),
        };
        events::emit_file_result(
            app,
            &FileResult {
                path: it.src.clone(),
                target: it.dst.clone(),
                size: it.size,
                status: status.into(),
                src_hash: String::new(),
                dst_hash: String::new(),
                message,
            },
        );
    }
}

// ================================================================ 小工具

fn action_name(a: PlannedAction) -> &'static str {
    a.label()
}

/// 源/目标路径里有非法 UTF-8 字节时，打一条警告日志标记这个路径，
/// 但**继续任务、不阻断**。文件内容与文件名编码无关 —— 名字有非法字节 ≠ 文件损坏。
///
/// 只在 Unix 上检测（`OsStr` 是原始字节；Windows 的 `OsStr` 是 UTF-16，恒为合法 Unicode）。
fn warn_if_non_utf8(app: &AppHandle, src: &Path, dst: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bad_src = std::str::from_utf8(src.as_os_str().as_bytes()).is_err();
        let bad_dst = std::str::from_utf8(dst.as_os_str().as_bytes()).is_err();
        if bad_src || bad_dst {
            events::emit_log(
                app,
                "warn",
                format!(
                    "文件名含非法字节（编码异常），已按原始字节继续拷贝：{}",
                    path_to_string(if bad_src { src } else { dst })
                ),
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (app, src, dst);
    }
}

/// 用户选了「覆盖」时的最终动作：
/// - 目标更小（半截文件）→ 仍是断点续传，把剩下的补齐，避免白读一遍已有数据
/// - 其余情况一律完整覆盖重写
/// 用户在冲突弹窗点了「覆盖此文件 / 全部覆盖」时，到底该做什么。
///
/// ⚠️ 这里**必须**返回 `Overwrite`（从头重写），不能返回 `Resume`。
/// 早期版本写的是「建议是续传就保持续传」，于是按钮写着「覆盖此文件」，
/// 实际执行的是**追加**：只要目标里那段内容与源不一致，就会拼出一个
/// **长度恰好等于源、内容却是错的**文件 —— stat 大小一样、SHA-256 不一致。
/// 这正是用户报的那类故障，所以语义已收紧为「说覆盖就是覆盖」。
fn apply_overwrite(_suggested: PlannedAction) -> PlannedAction {
    PlannedAction::Overwrite
}

/// 取得路径所在卷的剩余空间（字节）。失败返回 0。
fn free_space(path: &Path) -> u64 {
    // 复用 walk 的存在性回溯，避免路径不存在时直接失败
    let probe = fsinfo::find_existing_ancestor(path).unwrap_or_else(|| path.to_path_buf());
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut root = [0u16; 261];
        let wide: Vec<u16> = probe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
            if GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) == 0 {
                return 0;
            }
            let mut avail: u64 = 0;
            if GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                return 0;
            }
            avail
        }
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = CString::new(probe.as_os_str().as_bytes()) else {
            return 0;
        };
        unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c.as_ptr(), &mut st) != 0 {
                return 0;
            }
            (st.f_bavail as u64).saturating_mul(st.f_frsize as u64)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = probe;
        0
    }
}

/// 供命令层使用的剩余空间查询
pub fn query_free_space(path: &str) -> u64 {
    free_space(Path::new(path))
}

/// 供命令层使用：顺带校验目标目录可写
pub fn ensure_target_writable(target: &Path) -> Result<(), String> {
    let probe = fsinfo::find_existing_ancestor(target)
        .ok_or_else(|| format!("目标路径不存在：{}", path_to_string(target)))?;
    if !walk::is_dir(&probe) {
        return Err(format!("目标不是文件夹：{}", path_to_string(&probe)));
    }
    let meta = std::fs::metadata(&probe).map_err(|e| format!("无法访问目标目录：{e}"))?;
    if meta.permissions().readonly() {
        return Err(format!("目标目录为只读：{}", path_to_string(&probe)));
    }
    Ok(())
}

/// 供命令层使用：当前是否已有任务在跑
pub fn busy(st: &AppState) -> bool {
    st.is_busy()
}

#[cfg(test)]
mod tests {
    use super::classify_io_error;

    #[test]
    fn classify_maps_eilseq_to_filename_kind() {
        // 92 = EILSEQ：目标文件系统拒这个名字（APFS 未分配码位 / NTFS 冒号等）
        let e = std::io::Error::from_raw_os_error(92);
        assert_eq!(classify_io_error(&e), "文件名非法");
    }

    #[test]
    fn classify_maps_common_errnos() {
        assert_eq!(classify_io_error(&std::io::Error::from_raw_os_error(28)), "磁盘空间不足");
        assert_eq!(classify_io_error(&std::io::Error::from_raw_os_error(13)), "权限不足");
        assert_eq!(classify_io_error(&std::io::Error::from_raw_os_error(2)), "路径不存在");
    }

    #[test]
    fn classify_falls_back_to_error_kind() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(classify_io_error(&e), "权限不足");
        let e2 = std::io::Error::new(std::io::ErrorKind::WriteZero, "zero");
        assert_eq!(classify_io_error(&e2), "写入失败（磁盘可能已满）");
        let e3 = std::io::Error::new(std::io::ErrorKind::Other, "misc");
        assert_eq!(classify_io_error(&e3), "IO 错误");
    }

    #[test]
    fn error_dialog_timeout_is_two_minutes() {
        assert_eq!(super::ERROR_DIALOG_TIMEOUT_SECS, 120);
    }
}
