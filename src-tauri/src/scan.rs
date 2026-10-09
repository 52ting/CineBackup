//! 冲突预扫描 —— 拷贝前先把「会发生什么」全部算清楚
//!
//! 产出 `Plan`：每个文件的源路径、目标路径、预判动作、需要写入的字节数。
//! 大目录扫描期间持续回调 `ScanProgress`，前端进度条实时刷新，UI 不卡死。
//!
//! ## 预判规则（对应断点续传规则）
//! ```text
//! 目标不存在                → Copy      完整拷贝
//! 目标存在，目标更小         → Resume    断点续传（从目标末尾续写）
//! 目标存在，目标比源还大     → Overwrite 从头完整重写（绝不可能是同一份的半截）
//! 目标存在，大小一致
//!    ├ 内容哈希相同         → Skip      跳过
//!    └ 内容哈希不同         → Overwrite 覆盖
//! ```
//!
//! ⚠️ 预判必须与 `posix::copy_data_fork` 的运行时判定一致：
//! 「目标比源还大」在拷贝层会强制从头重写，因此这里**不能**再预判成 Resume，
//! 否则 Dry Run 显示「续传」而实际动作是「重写」，进度统计也会错（needed_bytes 为 0）。
//!
//! 内容哈希用哪种算法由 `JobOptions.hash_algo` 决定（默认 SHA-256）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::events::ScanProgress;
use crate::hash;
use crate::types::{JobOptions, PlanItem, PlanStats, PlannedAction};
use crate::util::{file_name_of, file_name_raw, join_relative, path_to_string};
// 仅在 macOS 预清洗（APFS 拒绝未分配码位）时用到，避免 Windows 构建报 unused import
#[cfg(target_os = "macos")]
use crate::util::escape_path_bytes;
use crate::walk::{self, PathKind};

#[derive(Debug)]
pub struct Plan {
    pub items: Vec<PlanItem>,
    /// 需要在目标端创建的目录（含源文件夹本身的层级）
    pub dirs: Vec<PathBuf>,
    pub stats: PlanStats,
    /// 非致命警告（源缺失、重复目标等）
    pub warnings: Vec<String>,
    /// 目标文件系统大概率存不下的名字（开拷前一次性报给用户）
    pub name_issues: Vec<crate::namecheck::NameIssue>,
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
        name_issues: Vec::new(),
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
                // 用原始字节拼目标路径：file_name_of 会把非法字节换成 �，导致 dst 从一开始就污染
                let dst = target.join(file_name_raw(&src_root));
                // APFS 预清洗：源名含未分配 Unicode 码位时，目标名提前换成可写名字，
                // 避免「每次重跑都因名字对不上而重复拷贝」（见 pre_sanitize_dst 说明）
                let dst = pre_sanitize_dst(&mut plan, &src_root, dst);
                let size = walk::file_size(&src_root);
                // 只查「文件名」这一层，别把盘符/绝对路径前缀（Windows 的 C:）也当坏名字
                let leaf = file_name_of(&src_root);
                check_name_recursive(&mut plan, Path::new(&leaf));
                push_unique(&mut raw, &mut seen_dst, &mut plan, &src_root, dst, size, src_idx);
                sp.files_seen += 1;
                sp.current = path_to_string(&src_root);
                progress(&sp);
            }
            PathKind::Dir => {
                // 目录源：整体复制到 目标/目录名/...（原始字节）
                let base_name = file_name_raw(&src_root);
                let dst_root = target.join(&base_name);
                // APFS 预清洗：目录名含被拒码位同样会 EILSEQ，且 walk 子路径继承清洗后的根
                let dst_root = pre_sanitize_dst(&mut plan, &src_root, dst_root);
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
                        if !rel.as_os_str().is_empty() {
                            check_name_recursive(&mut plan, &rel);
                        }
                        sp.dirs_seen += 1;
                    } else {
                        let rel = walk::relative_to(&src_root, p);
                        let d = join_relative(&dst_root, &rel);
                        let size = walk::file_size(p);
                        check_name_recursive(&mut plan, &rel);
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
                    src_path: src.clone(),
                    dst_path: dst.clone(),
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
        // ⚠️ 用 `<` / `>` 而非 `!=` 细分：目标比源还大时绝不可能续传（posix 会强制重写），
        // 这里必须预判成 Overwrite，否则 Dry Run / 进度统计（needed_bytes）与实际动作不一致。
        let (suggested, reason, hash_checked) = if existing_size < size {
            (
                PlannedAction::Resume,
                format!(
                    "目标更小（半截文件，源 {} vs 目标 {}）→ 断点续传",
                    crate::util::human_bytes(size),
                    crate::util::human_bytes(existing_size)
                ),
                false,
            )
        } else if existing_size > size {
            (
                PlannedAction::Overwrite,
                format!(
                    "目标比源还大（源 {} vs 目标 {}）→ 从头完整重写",
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
                    // 读取失败：先分清是哪一侧读不了，不能笼统「按覆盖」。
                    // 源读不了 → 拷贝阶段必然还会再报错，这里按 Copy 预判，让真实错误
                    //            在拷贝时暴露，而不是把「源已坏」误报成「覆盖目标」。
                    // 目标读不了 → 覆盖是合理的（反正要重写），但明确标注是目标侧问题。
                    if std::fs::File::open(&src).is_err() {
                        plan.warnings.push(format!(
                            "源文件读取失败：{} （{e}），拷贝阶段将重试并报错",
                            path_to_string(&src)
                        ));
                        (PlannedAction::Copy, format!("源读取失败：{e}"), false)
                    } else {
                        plan.warnings.push(format!(
                            "目标文件读取失败，按覆盖处理：{} （{e}）",
                            path_to_string(&src)
                        ));
                        (PlannedAction::Overwrite, format!("目标读取失败：{e}"), false)
                    }
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
            src_path: src.clone(),
            dst_path: dst.clone(),
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

/// 逐个路径组件检查名字，命中的问题汇总进 `plan.name_issues`。
///
/// 检查的是「相对源根」的每一段（文件名 + 各级父目录名），因为目标路径就是
/// `目标根 + 这段相对路径`，源侧名字有问题目标侧必然继承同一份。
/// 同一段坏名字（如同一目录下的多级父目录名）只记录一次，靠 `name_display` 去重。
fn check_name_recursive(plan: &mut Plan, rel: &Path) {
    for comp in rel.components() {
        let os = comp.as_os_str();
        if os.is_empty() {
            continue;
        }
        if let Some((name_display, reasons, suggestion)) = crate::namecheck::check_name(os) {
            // 用「那段名字本身」作为去重键：同名坏目录下的多个文件只报一次
            if plan
                .name_issues
                .iter()
                .any(|i| i.name_display == name_display && i.reasons == reasons)
            {
                continue;
            }
            plan.name_issues.push(crate::namecheck::NameIssue {
                path: path_to_string(rel),
                name_display,
                reasons,
                suggestion,
            });
        }
    }
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
            .push(format!("目标路径重复（同名冲突）：已忽略该源，保留更早加入的版本：{} → {}", path_to_string(src), key));
        return;
    }
    seen.insert(key);
    raw.push((src.to_path_buf(), dst, size, src_idx));
}

/// APFS 目标名预清洗（疑点：清洗映射缺失）
///
/// 问题背景：macOS 的 APFS 会拒绝「合法 UTF-8 里的未分配 Unicode 码位」的文件名，
/// 写入时抛 EILSEQ（errno 92）。旧逻辑是拷贝中途失败 → 自动换用清洗名（U+FFFD）重试，
/// 但**清洗后的名字没有持久化到任何地方**：下次重跑同一任务时，源文件名（原始码位）
/// 在目标盘上永远找不到自己上次写的文件（那是 U+FFFD 版），于是每跑一次都重复完整拷贝。
///
/// 修复：预扫描阶段（建计划时）就把目标路径换成清洗名，让「计划 → 拷贝 → 校验 → 下次
/// 重跑」全程使用同一个名字。两个源清洗后撞名时由 `push_unique` 去重兜底（后到的被忽略
/// 并警告），不会再出现「两个不同源各自清洗后写到同一个文件、互相覆盖」。
///
/// 仅 macOS 需要（APFS 才拒绝未分配码位）；Windows NTFS 接受这些码位，保留原名。
#[cfg(target_os = "macos")]
fn pre_sanitize_dst(plan: &mut Plan, src: &Path, dst: PathBuf) -> PathBuf {
    if crate::util::path_has_rejected_codepoint(&dst) {
        let cleaned = crate::util::sanitize_path_for_filesystem(&dst);
        if cleaned != dst {
            plan.warnings.push(format!(
                "源路径含 APFS 会拒绝的未分配 Unicode 码位，目标将写入清洗后的名字：{} → {}",
                escape_path_bytes(src),
                path_to_string(&cleaned)
            ));
            return cleaned;
        }
    }
    dst
}

#[cfg(not(target_os = "macos"))]
fn pre_sanitize_dst(_plan: &mut Plan, _src: &Path, dst: PathBuf) -> PathBuf {
    dst
}
