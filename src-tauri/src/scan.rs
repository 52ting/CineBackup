//! 冲突预扫描 —— 拷贝前先把「会发生什么」全部算清楚
//!
//! 产出 `Plan`：每个文件的源路径、目标路径、预判动作、需要写入的字节数。
//! 大目录扫描期间持续回调 `ScanProgress`，前端进度条实时刷新，UI 不卡死。
//!
//! ## 预判规则（对应断点续传规则）
//! ```text
//! 目标不存在                → Copy      完整拷贝
//! 目标存在，大小不一致       → Resume    断点续传（从目标末尾续写）
//! 目标存在，大小一致
//!    ├ 内容哈希相同         → Skip      跳过
//!    └ 内容哈希不同         → Overwrite 覆盖
//! ```
//!
//! 内容哈希用哪种算法由 `JobOptions.hash_algo` 决定（默认 SHA-256）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::events::ScanProgress;
use crate::hash;
use crate::types::{JobOptions, PlanItem, PlanStats, PlannedAction};
use crate::util::{file_name_of, join_relative, path_to_string};
use crate::walk::{self, PathKind};

#[derive(Debug)]
pub struct Plan {
    pub items: Vec<PlanItem>,
    /// 需要在目标端创建的目录（含源文件夹本身的层级）
    pub dirs: Vec<PathBuf>,
    pub stats: PlanStats,
    /// 非致命警告（源缺失、重复目标等）
    pub warnings: Vec<String>,
    pub cancelled: bool,
}

/// 扫描时每处理一项回调一次；由调用方做节流后再推送到 UI
pub type ScanCb<'a> = &'a mut dyn FnMut(&ScanProgress);

/// 构建拷贝计划。返回 `Err` 表示任务本身不合法（如目标目录在源目录内部）。
pub fn build_plan(
    sources: &[String],
    target: &Path,
    opts: &JobOptions,
    cancel: &AtomicBool,
    progress: ScanCb<'_>,
) -> Result<Plan, String> {
    let mut plan = Plan {
        items: Vec::new(),
        dirs: Vec::new(),
        stats: PlanStats::default(),
        warnings: Vec::new(),
        cancelled: false,
    };

    // ---------- 安全校验：目标目录不能位于任何源目录内部 ----------
    let target_canon = std::fs::canonicalize(target).ok();
    if let Some(ref tc) = target_canon {
        for s in sources {
            let sp = Path::new(s);
            if walk::is_dir(sp) {
                if let Ok(sc) = std::fs::canonicalize(sp) {
                    if tc == &sc {
                        return Err(format!("目标文件夹与源文件夹相同：{}", path_to_string(sp)));
                    }
                    if tc.starts_with(&sc) {
                        return Err(format!(
                            "目标文件夹位于源文件夹内部：\n  源：{}\n  目标：{}\n\
                             这会导致备份内容被反复自我拷贝，请更换目标位置。",
                            path_to_string(&sc),
                            path_to_string(tc)
                        ));
                    }
                }
            }
        }
    }

    // ---------- 阶段一：枚举所有源的文件与目录 ----------
    // 每项带上「归属的源下标」，界面才能按源分条显示各自的进度
    let mut raw: Vec<(PathBuf, PathBuf, u64, usize)> = Vec::new(); // (src, dst, size, src_idx)
    let mut seen_dst: HashSet<String> = HashSet::new();
    let mut sp = ScanProgress {
        phase: "enum".into(),
        ..Default::default()
    };

    for (src_idx, src_str) in sources.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            plan.cancelled = true;
            return Ok(plan);
        }
        let src_root = PathBuf::from(src_str);
        match walk::classify(&src_root) {
            PathKind::Missing => {
                plan.warnings.push(format!("源不存在，已跳过：{src_str}"));
                continue;
            }
            PathKind::Other => {
                plan.warnings.push(format!("不支持的源类型，已跳过：{src_str}"));
                continue;
            }
            PathKind::File => {
                let dst = target.join(file_name_of(&src_root));
                let size = walk::file_size(&src_root);
                push_unique(&mut raw, &mut seen_dst, &mut plan, &src_root, dst, size, src_idx);
                sp.files_seen += 1;
                sp.current = path_to_string(&src_root);
                progress(&sp);
            }
            PathKind::Dir => {
                // 目录源：整体复制到 目标/目录名/...
                let base_name = file_name_of(&src_root);
                let dst_root = target.join(&base_name);
                plan.dirs.push(dst_root.clone());

                // 先在本地收集，闭包结束后统一做去重入列（避免同时可变借用 plan 的多个部分）
                let mut local: Vec<(PathBuf, PathBuf, u64, usize)> = Vec::new();
                let outcome = walk::walk_tree(&src_root, cancel, |p, is_dir| {
                    if is_dir {
                        let rel = walk::relative_to(&src_root, p);
                        let d = if rel.as_os_str().is_empty() {
                            dst_root.clone()
                        } else {
                            join_relative(&dst_root, &rel)
                        };
                        plan.dirs.push(d);
                        sp.dirs_seen += 1;
                    } else {
                        let rel = walk::relative_to(&src_root, p);
                        let d = join_relative(&dst_root, &rel);
                        let size = walk::file_size(p);
                        sp.files_seen += 1;
                        sp.current = path_to_string(p);
                        local.push((p.to_path_buf(), d, size, src_idx));
                        progress(&sp);
                    }
                });

                for (p, d, size, si) in local {
                    push_unique(&mut raw, &mut seen_dst, &mut plan, &p, d, size, si);
                }

                plan.stats.filtered += outcome.filtered;
                for e in outcome.errors.iter().take(20) {
                    plan.warnings.push(e.clone());
                }
                if outcome.cancelled {
                    plan.cancelled = true;
                    return Ok(plan);
                }
            }
        }
    }

    // ---------- 阶段二：逐项判断冲突 ----------
    sp.phase = "check".into();
    sp.total = raw.len() as u64;
    sp.checked = 0;
    progress(&sp);

    for (src, dst, size, src_idx) in raw.into_iter() {
        if cancel.load(Ordering::Relaxed) {
            plan.cancelled = true;
            return Ok(plan);
        }
        sp.checked += 1;
        sp.current = path_to_string(&src);
        progress(&sp);

        let existing_size = match std::fs::metadata(&dst) {
            Ok(m) if m.is_file() => m.len(),
            _ => {
                // 目标不存在 → 完整拷贝
                plan.items.push(PlanItem {
                    src: path_to_string(&src),
                    dst: path_to_string(&dst),
                    size,
                    existing_size: 0,
                    action: PlannedAction::Copy,
                    suggested: PlannedAction::Copy,
                    reason: "目标不存在".into(),
                    hash_checked: false,
                    final_action: None,
                    needed_bytes: size,
                    src_idx,
                });
                continue;
            }
        };

        // 目标存在 —— 按断点续传规则预判
        let (suggested, reason, hash_checked) = if existing_size != size {
            (
                PlannedAction::Resume,
                format!(
                    "大小不一致（源 {} vs 目标 {}）→ 断点续传",
                    crate::util::human_bytes(size),
                    crate::util::human_bytes(existing_size)
                ),
                false,
            )
        } else if opts.quick_scan {
            (
                PlannedAction::Skip,
                "大小一致（快速扫描：跳过哈希比对）".into(),
                false,
            )
        } else {
            // 大小一致 → 必须用哈希判定（默认 SHA-256，可切 xxHash64）
            let algo = opts.hash_algo;
            let mut hashed: u64 = 0;
            let same = hash::files_identical(&src, &dst, algo, cancel, |n| {
                hashed += n;
            });
            sp.bytes_hashed = sp.bytes_hashed.saturating_add(hashed);
            match same {
                Ok(true) => (
                    PlannedAction::Skip,
                    format!("大小一致且 {} 相同", algo.label()),
                    true,
                ),
                Ok(false) => (
                    PlannedAction::Overwrite,
                    format!("大小一致但 {} 不同", algo.label()),
                    false,
                ),
                Err(e) => {
                    // 读取失败（权限 / 盘掉线）：保守按「覆盖」处理，交给拷贝阶段报错
                    plan.warnings
                        .push(format!("哈希比对失败，按覆盖处理：{} （{e}）", path_to_string(&src)));
                    (PlannedAction::Overwrite, format!("哈希比对失败：{e}"), false)
                }
            }
        };

        let action = if opts.ask_on_conflict {
            PlannedAction::Conflict // 交给运行时弹窗决定
        } else {
            suggested
        };

        let needed = match suggested {
            PlannedAction::Resume => size.saturating_sub(existing_size),
            PlannedAction::Skip => 0,
            _ => size,
        };

        plan.items.push(PlanItem {
            src: path_to_string(&src),
            dst: path_to_string(&dst),
            size,
            existing_size,
            action,
            suggested,
            reason,
            hash_checked,
            final_action: None,
            needed_bytes: needed,
            src_idx,
        });
    }

    // ---------- 汇总 ----------
    plan.dirs.sort();
    plan.dirs.dedup();
    let mut st = PlanStats {
        filtered: plan.stats.filtered,
        ..Default::default()
    };
    for it in &plan.items {
        match it.action {
            PlannedAction::Conflict => {
                st.conflict += 1;
                st.total_bytes = st.total_bytes.saturating_add(it.needed_bytes);
            }
            PlannedAction::Copy => {
                st.copy += 1;
                st.total_bytes = st.total_bytes.saturating_add(it.needed_bytes);
            }
            PlannedAction::Resume => {
                st.resume += 1;
                st.total_bytes = st.total_bytes.saturating_add(it.needed_bytes);
            }
            PlannedAction::Skip => st.skip += 1,
            PlannedAction::Overwrite => {
                st.overwrite += 1;
                st.total_bytes = st.total_bytes.saturating_add(it.needed_bytes);
            }
        }
    }
    plan.stats = st;
    Ok(plan)
}

/// 目标路径去重：同一个目标被两个源命中时，只保留第一个，其余忽略并警告
fn push_unique(
    raw: &mut Vec<(PathBuf, PathBuf, u64, usize)>,
    seen: &mut HashSet<String>,
    plan: &mut Plan,
    src: &Path,
    dst: PathBuf,
    size: u64,
    src_idx: usize,
) {
    let key = path_to_string(&dst);
    if seen.contains(&key) {
        plan.warnings
            .push(format!("目标路径重复，已忽略该源：{} → {}", path_to_string(src), key));
        return;
    }
    seen.insert(key);
    raw.push((src.to_path_buf(), dst, size, src_idx));
}
