//! 复现 + 固定「两个同名源 → 撞同一个目标路径」的**实际行为**。
//!
//! 背景（用户问「为什么提示 SHA-256 失败」，排查时发现的相邻风险）：
//! 目标路径的拼法是 `目标根 / 源目录名 / 相对路径`（见 scan.rs 的 `dst_root`），
//! 所以两个**尾级名字相同**的源会映射到同一个目标路径：
//!
//!   D:\CardA\DCIM\100MEDIA\clip.mp4  ─┐
//!                                     ├─→ 目标\DCIM\100MEDIA\clip.mp4
//!   D:\CardB\DCIM\100MEDIA\clip.mp4  ─┘
//!
//! `push_unique` 用「目标路径字符串」去重：**保留先来的，后来的整条源被丢弃**，
//! 只在日志里留一句 warn（`目标路径重复，已忽略该源：…`）。
//!
//! ⚠️ 这条测试的价值不在「撞车会导致校验失败」（它不会 —— 撞车被拦下了），
//! 而在**固定住这个行为**：多卡备份时目录都叫 DCIM，第二张卡会被静默跳过，
//! 只有一行 warn。哪天改成「自动改名落盘」或「直接报错」，这条测试会红。
//!
//! 运行：cargo test --test dup_target -- --nocapture

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use cinebackup_lib::copy::{self, CopyMode};
use cinebackup_lib::events::ScanProgress;
use cinebackup_lib::hash::{self, HashAlgo};
use cinebackup_lib::scan;
use cinebackup_lib::types::{JobOptions, PlannedAction};

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cinebackup_dup_{name}"));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// 确定性伪随机内容（不用全零，避免掩盖「写到错误偏移」这类 bug）
fn make_data(len: usize, seed: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x = seed | 1;
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push((x & 0xFF) as u8);
    }
    v
}

/// 搭一个「两张卡、同名目录、同名文件」的现场，返回 (源列表, 目标根)
fn two_cards(tag: &str) -> (Vec<String>, PathBuf) {
    let root = tmpdir(tag);
    let target = root.join("out");
    let mut sources = Vec::new();
    for (card, seed) in [("CardA", 11u64), ("CardB", 99u64)] {
        let media = root.join(card).join("DCIM").join("100MEDIA");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("clip.mp4"), make_data(4096, seed)).unwrap();
        sources.push(root.join(card).join("DCIM").to_string_lossy().into_owned());
    }
    fs::create_dir_all(&target).unwrap();
    (sources, target)
}

fn build(sources: &[String], target: &PathBuf) -> scan::Plan {
    let cancel = AtomicBool::new(false);
    let opts = JobOptions::default();
    let mut cb = |_sp: &ScanProgress| {};
    scan::build_plan(sources, target, &opts, &cancel, &mut cb).expect("计划构建不该失败")
}

/// 行为 ①：两个同名源目录 → **只保留第一个**，第二个整条被丢弃并留 warn。
#[test]
fn t1_second_same_basename_source_is_dropped_with_a_warning() {
    let (sources, target) = two_cards("dirs");
    let plan = build(&sources, &target);

    println!("计划条目数 = {}", plan.items.len());
    for it in &plan.items {
        println!("  源 {}  →  目标 {}", it.src, it.dst);
    }
    for w in &plan.warnings {
        println!("  [warn] {w}");
    }

    assert_eq!(
        plan.items.len(),
        1,
        "两个源各一个文件，但目标路径相撞 → 应只保留 1 条"
    );
    assert!(
        plan.items[0].src.contains("CardA"),
        "保留的应是**先来的**那个源（CardA），实际：{}",
        plan.items[0].src
    );
    assert_eq!(plan.items[0].action, PlannedAction::Copy);
    assert!(
        plan.warnings.iter().any(|w| w.contains("目标路径重复")),
        "必须留下「目标路径重复」的 warn —— 否则第二张卡就彻底静默丢了。实际 warnings：{:?}",
        plan.warnings
    );
}

/// 行为 ②：撞车被拦下后，剩下那一条能正常拷 + 校验通过
/// （所以「SHA-256 不一致」**不是**撞车造成的 —— 撞车是「少备了一份」，不是「备错了」）
#[test]
fn t2_surviving_item_copies_and_verifies_clean() {
    let (sources, target) = two_cards("e2e");
    let plan = build(&sources, &target);
    assert_eq!(plan.items.len(), 1);

    let cancel = AtomicBool::new(false);
    let it = &plan.items[0];
    let mode = if it.action == PlannedAction::Copy {
        CopyMode::Fresh
    } else {
        CopyMode::Overwrite
    };
    copy::copy_file_simple(&it.src_path, &it.dst_path, mode, &cancel).unwrap();

    let (a, na) = hash::hash_file(&it.src_path, HashAlgo::Sha256, &cancel, |_| {}).unwrap();
    let (b, nb) = hash::hash_file(&it.dst_path, HashAlgo::Sha256, &cancel, |_| {}).unwrap();
    assert_eq!(a, b, "正常拷贝后校验应一致");
    assert_eq!(na, nb);

    println!("✅ 撞车被拦下后，剩下的那条拷贝 + 校验都正常");
}

