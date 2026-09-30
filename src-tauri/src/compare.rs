//! 对比校验 —— 独立的「两边是否一致」检查，不参与备份任务
//!
//! 与备份流程的区别：
//! - 备份是「源 → 目标」的单向写入 + 事后校验
//! - 对比是**只读**的：同时看左右两边，回答「一致 / 不一致 / 只有一边有」
//!
//! 支持两种输入组合（其它组合直接报错）：
//! - 文件 ↔ 文件：比大小，大小相同再比内容哈希
//! - 文件夹 ↔ 文件夹：按**相对路径**配对，逐项分类
//!
//! 判定顺序（省时间的短路优先）：
//! 1. 只有一边有 → `left_only` / `right_only`（不用读内容）
//! 2. 两边都有但**大小不同** → `different`（不用读内容）
//! 3. 大小相同 → 快速模式直接判 `same`，否则读两边算哈希比对
//!
//! ⚠️ 哈希比对要**把两边各完整读一遍**，和 `verify.rs` 是同一个账：
//! 对比 300 GB 的两份就是读 600 GB。快速模式（`quick`）就是为了跳过这笔开销。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::hash::{self, HashAlgo};
use crate::util::{path_to_string, Throttle};
use crate::walk::{self, PathKind};

/// 对比选项
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareOptions {
    #[serde(default)]
    pub hash_algo: HashAlgo,
    /// 快速模式：大小相同即视为一致，**不读内容**
    #[serde(default)]
    pub quick: bool,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self {
            hash_algo: HashAlgo::default(),
            quick: false,
        }
    }
}

/// 单条对比结果
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompareItem {
    /// 相对路径（文件夹对比时）/ 文件名（文件对比时）
    pub rel: String,
    /// "same" | "different" | "left_only" | "right_only" | "error"
    pub status: String,
    pub left_size: u64,
    pub right_size: u64,
    /// 内容哈希（十六进制）；快速模式或只在一侧时为空
    pub left_hash: String,
    pub right_hash: String,
    pub message: String,
}

/// 整体结果
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompareResult {
    pub left: String,
    pub right: String,
    /// "file" | "dir"
    pub mode: String,
    /// 本次用的哈希算法名（快速模式下标注）
    pub algo: String,
    pub quick: bool,
    pub items: Vec<CompareItem>,
    pub same: u64,
    pub different: u64,
    pub left_only: u64,
    pub right_only: u64,
    pub errors: u64,
    pub left_bytes: u64,
    pub right_bytes: u64,
    /// 本次比对实际读了多少字节（哈希阶段）
    pub hashed_bytes: u64,
    pub elapsed_secs: f64,
    pub cancelled: bool,
    /// 两边是否完全一致（无 different / *_only / errors）
    pub ok: bool,
    pub message: String,
}

/// 进度上报（大目录枚举 / 大量哈希时用）
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompareProgress {
    /// "enum" 枚举 | "hash" 比对内容
    pub phase: String,
    pub files_total: u64,
    pub files_done: u64,
    pub current: String,
    pub bytes_hashed: u64,
}

/// 枚举一侧的文件：相对路径 → (绝对路径, 大小)
///
/// 用 `PathBuf`（原始字节）作 key 而不是 lossy 字符串 —— 否则两个不同名字的
/// 非法字节文件名会被 U+FFFD 撞成同一个 key，对比结果就错了。
fn collect(root: &Path, cancel: &AtomicBool) -> BTreeMap<PathBuf, (PathBuf, u64)> {
    let mut map: BTreeMap<PathBuf, (PathBuf, u64)> = BTreeMap::new();
    walk::walk_tree(root, cancel, |p, is_dir| {
        if is_dir {
            return;
        }
        // 系统元数据（.DS_Store / ._* 等）不算差异，否则每次对比都一堆假阳性
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if walk::should_skip_file(name) {
                return;
            }
        }
        let rel = match p.strip_prefix(root) {
            Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
            _ => return, // 根本身（或异常情况）不作为条目
        };
        map.insert(rel, (p.to_path_buf(), walk::file_size(p)));
    });
    map
}

/// 对比两份（文件 ↔ 文件、文件夹 ↔ 文件夹）
pub fn run_compare(
    left: &Path,
    right: &Path,
    opts: &CompareOptions,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(&CompareProgress),
) -> CompareResult {
    let started = Instant::now();
    let mut res = CompareResult {
        left: path_to_string(left),
        right: path_to_string(right),
        algo: if opts.quick {
            "快速模式（仅比大小）".to_string()
        } else {
            opts.hash_algo.label().to_string()
        },
        quick: opts.quick,
        ..Default::default()
    };

    let lk = walk::classify(left);
    let rk = walk::classify(right);

    // ---- 前置校验：路径必须存在，且类型一致 ----
    for (p, k, side) in [(left, lk, "左侧"), (right, rk, "右侧")] {
        if k == PathKind::Missing {
            res.message = format!("{side}路径不存在或不可访问：{}", path_to_string(p));
            return res;
        }
    }
    let both_file = lk == PathKind::File && rk == PathKind::File;
    let both_dir = lk == PathKind::Dir && rk == PathKind::Dir;
    if !both_file && !both_dir {
        res.message = format!(
            "两边类型不一致：左侧是{}，右侧是{}。请让两边同为文件或同为文件夹。",
            kind_label(lk),
            kind_label(rk)
        );
        return res;
    }
    res.mode = if both_file { "file" } else { "dir" }.to_string();

    // ---- 收集待对比条目 ----
    let mut items: Vec<CompareItem> = Vec::new();
    let mut pending_hash: Vec<(String, PathBuf, PathBuf, u64)> = Vec::new(); // (rel, 左, 右, 大小)

    if both_file {
        let ls = walk::file_size(left);
        let rs = walk::file_size(right);
        let rel = left
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_to_string(left));
        let mut it = CompareItem {
            rel,
            left_size: ls,
            right_size: rs,
            ..Default::default()
        };
        if ls != rs {
            it.status = "different".into();
            it.message = format!("大小不同（左 {} / 右 {}）", crate::util::human_bytes(ls), crate::util::human_bytes(rs));
        } else if opts.quick {
            it.status = "same".into();
            it.message = "大小相同（快速模式，未比对内容）".into();
        } else {
            pending_hash.push((it.rel.clone(), left.to_path_buf(), right.to_path_buf(), ls));
        }
        items.push(it);
    } else {
        // 目录：枚举两边
        on_progress(&CompareProgress {
            phase: "enum".into(),
            current: path_to_string(left),
            ..Default::default()
        });
        let lmap = collect(left, cancel);
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            res.cancelled = true;
            res.message = "对比已被取消".into();
            res.elapsed_secs = started.elapsed().as_secs_f64();
            return res;
        }
        on_progress(&CompareProgress {
            phase: "enum".into(),
            current: path_to_string(right),
            files_total: lmap.len() as u64,
            ..Default::default()
        });
        let rmap = collect(right, cancel);

        let keys: BTreeSet<&PathBuf> = lmap.keys().chain(rmap.keys()).collect();
        for k in keys {
            let rel = k.to_string_lossy().into_owned();
            match (lmap.get(k), rmap.get(k)) {
                (Some((_, ls)), None) => {
                    items.push(CompareItem {
                        rel,
                        status: "left_only".into(),
                        left_size: *ls,
                        message: "仅左侧存在".into(),
                        ..Default::default()
                    });
                }
                (None, Some((_, rs))) => {
                    items.push(CompareItem {
                        rel,
                        status: "right_only".into(),
                        right_size: *rs,
                        message: "仅右侧存在".into(),
                        ..Default::default()
                    });
                }
                (Some((lp, ls)), Some((rp, rs))) => {
                    let mut it = CompareItem {
                        rel,
                        left_size: *ls,
                        right_size: *rs,
                        ..Default::default()
                    };
                    if ls != rs {
                        it.status = "different".into();
                        it.message = format!(
                            "大小不同（左 {} / 右 {}）",
                            crate::util::human_bytes(*ls),
                            crate::util::human_bytes(*rs)
                        );
                    } else if opts.quick {
                        it.status = "same".into();
                        it.message = "大小相同（快速模式，未比对内容）".into();
                    } else {
                        pending_hash.push((it.rel.clone(), lp.clone(), rp.clone(), *ls));
                    }
                    items.push(it);
                }
                (None, None) => {}
            }
        }
    }

    // ---- 内容比对（只对大小相同、非快速模式的条目）----
    let hash_total = pending_hash.len() as u64;
    let mut hashed_bytes: u64 = 0;
    let mut throttle = Throttle::new(150);
    if !pending_hash.is_empty() {
        for (i, (rel, lp, rp, _sz)) in pending_hash.iter().enumerate() {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                res.cancelled = true;
                break;
            }
            let mut cb = |delta: u64| {
                hashed_bytes = hashed_bytes.saturating_add(delta);
                if throttle.ready() {
                    on_progress(&CompareProgress {
                        phase: "hash".into(),
                        files_total: hash_total,
                        files_done: i as u64,
                        current: rel.clone(),
                        bytes_hashed: hashed_bytes,
                    });
                }
            };
            // 两边各完整读一遍（这就是「对比 300 GB 要读 600 GB」的账）
            let hl = hash::hash_file(lp, opts.hash_algo, cancel, &mut cb);
            let hr = hash::hash_file(rp, opts.hash_algo, cancel, &mut cb);
            let slot = items.iter_mut().find(|it| it.rel == *rel);
            match (hl, hr) {
                (Ok((a, _)), Ok((b, _))) => {
                    if let Some(it) = slot {
                        it.left_hash = a.hex().to_string();
                        it.right_hash = b.hex().to_string();
                        if a == b {
                            it.status = "same".into();
                            it.message = format!("内容一致（{} 相同）", opts.hash_algo.label());
                        } else {
                            it.status = "different".into();
                            it.message = "内容不一致（大小相同但哈希不同）".into();
                        }
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    if let Some(it) = slot {
                        it.status = "error".into();
                        it.message = format!("读取失败：{e}");
                    }
                }
            }
            // 文件边界强制上报一次（否则小文件连读多个会被节流窗口整段吞掉）
            on_progress(&CompareProgress {
                phase: "hash".into(),
                files_total: hash_total,
                files_done: (i + 1) as u64,
                current: rel.clone(),
                bytes_hashed: hashed_bytes,
            });
            throttle.force();
        }
    }

    // ---- 汇总 ----
    for it in &items {
        match it.status.as_str() {
            "same" => res.same += 1,
            "different" => res.different += 1,
            "left_only" => res.left_only += 1,
            "right_only" => res.right_only += 1,
            _ => res.errors += 1,
        }
        res.left_bytes = res.left_bytes.saturating_add(it.left_size);
        res.right_bytes = res.right_bytes.saturating_add(it.right_size);
    }
    res.hashed_bytes = hashed_bytes;
    res.items = items;
    res.elapsed_secs = started.elapsed().as_secs_f64();
    res.ok = !res.cancelled
        && res.different == 0
        && res.left_only == 0
        && res.right_only == 0
        && res.errors == 0;
    if res.cancelled {
        res.message = "对比已被取消（结果只覆盖已处理的部分）".into();
    } else if res.ok {
        res.message = if res.quick {
            format!("两边一致：{} 项全部匹配（快速模式仅比大小）", res.same)
        } else {
            format!("两边一致：{} 项内容完全相同", res.same)
        };
    } else {
        res.message = format!(
            "存在差异：不一致 {} · 仅左侧 {} · 仅右侧 {} · 读取失败 {}",
            res.different, res.left_only, res.right_only, res.errors
        );
    }
    res
}

fn kind_label(k: PathKind) -> &'static str {
    match k {
        PathKind::File => "文件",
        PathKind::Dir => "文件夹",
        PathKind::Missing => "不存在的路径",
        // 管道 / 设备 / 套接字等特殊文件：既不是普通文件也不是目录，对比无意义
        PathKind::Other => "特殊文件（管道 / 设备等）",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicBool;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cinebackup_cmp_{name}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn run(l: &Path, r: &Path, quick: bool) -> CompareResult {
        let cancel = AtomicBool::new(false);
        run_compare(l, r, &CompareOptions { hash_algo: HashAlgo::Sha256, quick }, &cancel, |_| {})
    }

    #[test]
    fn two_files_identical_and_different() {
        let d = tmpdir("two_files");
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        fs::write(&a, b"hello world").unwrap();

        // 内容相同
        fs::write(&b, b"hello world").unwrap();
        let r = run(&a, &b, false);
        assert!(r.ok, "{}", r.message);
        assert_eq!(r.same, 1);
        assert_eq!(r.items[0].status, "same");
        assert_eq!(r.items[0].left_hash, r.items[0].right_hash);

        // 大小不同 → different（且不读内容）
        fs::write(&b, b"hello world, longer").unwrap();
        let r2 = run(&a, &b, false);
        assert!(!r2.ok);
        assert_eq!(r2.different, 1);
        assert!(r2.items[0].left_hash.is_empty(), "大小不同不该算哈希");

        // 大小相同、内容不同 → different（哈希不同）
        fs::write(&b, b"HELLO WORLD").unwrap();
        let r3 = run(&a, &b, false);
        assert!(!r3.ok);
        assert_eq!(r3.different, 1);
        assert!(!r3.items[0].left_hash.is_empty());
        assert_ne!(r3.items[0].left_hash, r3.items[0].right_hash);

        // 快速模式：大小相同就判一致（即使内容不同）
        let r4 = run(&a, &b, true);
        assert!(r4.ok, "快速模式应把大小相同判为一致");
        assert_eq!(r4.same, 1);
    }

    #[test]
    fn dirs_classify_all_five_cases() {
        let l = tmpdir("dir_l");
        let r = tmpdir("dir_r");
        // 相同
        fs::write(l.join("same.txt"), b"abc").unwrap();
        fs::write(r.join("same.txt"), b"abc").unwrap();
        // 内容不同（大小也不同）
        fs::write(l.join("diff.txt"), b"aaa").unwrap();
        fs::write(r.join("diff.txt"), b"bbbbb").unwrap();
        // 仅左
        fs::write(l.join("left.bin"), b"L").unwrap();
        // 仅右
        fs::write(r.join("right.bin"), b"R").unwrap();
        // 子目录里的同名文件
        fs::create_dir_all(l.join("sub")).unwrap();
        fs::create_dir_all(r.join("sub")).unwrap();
        fs::write(l.join("sub/nested.txt"), b"nested").unwrap();
        fs::write(r.join("sub/nested.txt"), b"nested").unwrap();

        let res = run(&l, &r, false);
        assert!(!res.ok);
        assert_eq!(res.same, 2, "same.txt + sub/nested.txt");
        assert_eq!(res.different, 1);
        assert_eq!(res.left_only, 1);
        assert_eq!(res.right_only, 1);
        assert_eq!(res.errors, 0);
        // 所有条目都应在 items 里
        assert_eq!(res.items.len(), 5);
        // 相对路径用 / 分隔（展示用）
        assert!(res.items.iter().any(|i| i.rel.contains("nested.txt")));
    }

    #[test]
    fn mixed_kinds_is_an_error_not_a_panic() {
        let d = tmpdir("mixed");
        let f = d.join("f.txt");
        fs::write(&f, b"x").unwrap();
        let sub = d.join("sub");
        fs::create_dir_all(&sub).unwrap();

        let res = run(&f, &sub, false);
        assert!(!res.ok);
        assert_eq!(res.items.len(), 0);
        assert!(res.message.contains("类型不一致"), "实际：{}", res.message);
    }

    #[test]
    fn missing_path_reports_clearly() {
        let d = tmpdir("missing");
        let f = d.join("f.txt");
        fs::write(&f, b"x").unwrap();
        let res = run(&f, &d.join("nope.txt"), false);
        assert!(!res.ok);
        assert!(res.message.contains("不存在"), "实际：{}", res.message);
    }

    #[test]
    fn empty_dirs_are_identical() {
        let l = tmpdir("empty_l");
        let r = tmpdir("empty_r");
        let res = run(&l, &r, false);
        assert!(res.ok, "两个空目录应判一致：{}", res.message);
        assert_eq!(res.same, 0);
    }
}
