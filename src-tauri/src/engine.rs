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
//! xxHash64 全量校验
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
use crate::hash;
use crate::scan;
use crate::state::AppState;
use crate::types::{
    FileResult, JobEnd, JobOptions, JobRequest, PlanItem, PlannedAction, UserReply,
};
use crate::util::{human_bytes, path_to_string, RateMeter, Throttle};
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
    events::emit_status(app, events::ST_COPYING);
    let outcome = run_copy_phase(app, st, &mut plan, &req.options, &req.sources)?;

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
    } else if req.options.verify_after_copy {        events::emit_status(app, events::ST_VERIFYING);
        let (idx, pres, _unverified_skips) =
            build_verify_set(app, &plan.items, &req.options, &outcome.failed_idx);
        end.pass = pres;
        let total: u64 = idx.iter().map(|&i| plan.items[i].size.saturating_mul(2)).sum();
        if idx.is_empty() {
            events::emit_log(app, "info", "没有需要校验的文件。");
        } else {
            let vs = verify::run_verify(app, &plan.items, &idx, &st.cancel, total);
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
        let src = std::path::PathBuf::from(&plan.items[i].src);
        let dst = std::path::PathBuf::from(&plan.items[i].dst);
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
                    format!("跳过（预扫描已确认 xxHash64 一致）：{}", plan.items[i].src),
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
        let mut mode = match action {
            PlannedAction::Resume => CopyMode::Resume,
            PlannedAction::Overwrite => CopyMode::Overwrite,
            _ => CopyMode::Fresh,
        };
        let mut base = if mode == CopyMode::Resume { existing } else { 0 };

        // 续传前的前缀校验：确认目标里已写入的部分和源的前 N 字节完全一致
        if mode == CopyMode::Resume && opts.resume_prefix_check && existing > 0 {
            events::emit_log(
                app,
                "info",
                format!(
                    "续传前校验已写入部分（{}）：{}",
                    human_bytes(existing),
                    path_to_string(&dst)
                ),
            );
            let ok = hash::resume_prefix_ok(&src, &dst, existing, &st.cancel, |_| {});
            match ok {
                Ok(true) => {}
                Ok(false) => {
                    events::emit_log(
                        app,
                        "warn",
                        format!(
                            "已写入部分与源不一致（可能上次中断在半块上），改为从头覆盖重写：{}",
                            path_to_string(&dst)
                        ),
                    );
                    mode = CopyMode::Overwrite;
                    base = 0;
                }
                Err(e) => {
                    events::emit_log(
                        app,
                        "warn",
                        format!("前缀校验失败（{e}），改为从头覆盖重写：{}", path_to_string(&dst)),
                    );
                    mode = CopyMode::Overwrite;
                    base = 0;
                }
            }
            if st.cancelled() {
                out.aborted = true;
                break;
            }
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
        let res = {
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
            copy::copy_file(&src, &dst, mode, &st.cancel, &mut on_bytes)
        };

        match res {
            Ok(o) => {
                if o.restarted {
                    // 本想续传但目标不可续 → 之前记账的 base 需要退回
                    bytes_done = bytes_done.saturating_sub(base);
                    if let Some(sp) = src_prog.get_mut(si) {
                        sp.bytes_done = sp.bytes_done.saturating_sub(base);
                    }
                    base = 0;
                    events::emit_log(
                        app,
                        "warn",
                        format!(
                            "目标文件不可续传（大小异常），已完整重写：{}",
                            path_to_string(&dst)
                        ),
                    );
                }
                out.written_bytes = out.written_bytes.saturating_add(o.written);
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
                // 单个文件失败 → 让用户决定继续还是终止
                out.errors += 1;
                out.failed_idx.push(i);
                out.written_bytes = out.written_bytes.saturating_add(file_written);
                if let Some(sp) = src_prog.get_mut(si) {
                    sp.state = "failed".into();
                }
                let err_text = e.to_string();
                events::emit_log(
                    app,
                    "error",
                    format!("拷贝失败：{} —— {err_text}", path_to_string(&src)),
                );
                events::emit_file_result(
                    app,
                    &FileResult {
                        path: path_to_string(&src),
                        target: path_to_string(&dst),
                        size,
                        status: "error".into(),
                        src_hash: String::new(),
                        dst_hash: String::new(),
                        message: format!("拷贝失败：{err_text}"),
                    },
                );
                let reply = events::ask_user(
                    app,
                    st,
                    events::EV_COPY_ERROR,
                    CopyErrorAsk {
                        src: path_to_string(&src),
                        dst: path_to_string(&dst),
                        error: err_text,
                        done: files_done,
                        total: files_total,
                    },
                );
                match reply {
                    Some(UserReply::Continue) => {
                        events::emit_log(app, "warn", "已选择继续处理下一个文件。");
                    }
                    _ => {
                        out.aborted = true;
                        events::emit_log(app, "warn", "已选择终止全部任务。");
                        break;
                    }
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
                    // 预扫描已经比对过 xxHash64，直接判定通过
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
                            message: "跳过（预扫描 xxHash64 已一致）".into(),
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
                if opts.verify_after_copy {
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

/// 用户选了「覆盖」时的最终动作：
/// - 目标更小（半截文件）→ 仍是断点续传，把剩下的补齐，避免白读一遍已有数据
/// - 其余情况一律完整覆盖重写
fn apply_overwrite(suggested: PlannedAction) -> PlannedAction {
    match suggested {
        PlannedAction::Resume => PlannedAction::Resume,
        _ => PlannedAction::Overwrite,
    }
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
