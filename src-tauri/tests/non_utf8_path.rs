//! 回归测试：文件名含非法 UTF-8 字节（Mac APFS 冒号 `:` 显示为 `/`，
//! 或从 FAT/SMB 盘带过来的 GBK 名）时，程序必须**照样拷贝 + 校验**，
//! 不能因为 `to_string_lossy()` 把路径变成 `�` 而打不开文件（EILSEQ 真凶之一）。
//!
//! 这个测试只能在 Unix 上跑：Windows 的 `OsStr` 底层是 UTF-16，
//! 根本造不出「非法 UTF-8 字节」的文件名（这也正是这类问题只出现在 Mac 侧的原因）。
//!
//! 运行： cargo test --test non_utf8_path -- --nocapture

#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use std::os::unix::ffi::OsStringExt;

use cinebackup_lib::copy::{self, CopyMode};
use cinebackup_lib::events::ScanProgress;
use cinebackup_lib::hash::{self, HashAlgo};
use cinebackup_lib::scan;
use cinebackup_lib::types::JobOptions;

/// 用原始字节构造一个「非法 UTF-8」的文件名（`0xFF` / `0xFE` 不是合法 UTF-8）
fn bad_name(base: &str) -> std::ffi::OsString {
    let mut b = base.as_bytes().to_vec();
    b.extend_from_slice(&[0xFF, 0xFE]); // 塞进非法字节
    std::ffi::OsString::from_vec(b)
}

#[test]
fn copies_file_with_non_utf8_name() {
    let root = std::env::temp_dir().join("cinebackup_nonutf8_test");
    let _ = fs::remove_dir_all(&root);

    let src_dir = root.join("src");
    let dst_dir = root.join("dst");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&dst_dir).unwrap();

    // 源文件：文件名带非法 UTF-8 字节
    let src_file = src_dir.join(bad_name("shot_"));
    let data = b"0123456789abcdef".repeat(100); // 1.6 KB
    let mut f = fs::File::create(&src_file).unwrap();
    f.write_all(&data).unwrap();
    f.sync_all().unwrap();

    let cancel = AtomicBool::new(false);
    let opts = JobOptions::default();
    let sources = vec![src_dir.to_string_lossy().into_owned()];

    // 1. 预扫描能正常枚举出这个文件（路径经 lossy 后文件名含 �，但 src_path 保留原始字节）
    let mut last = ScanProgress::default();
    let plan = scan::build_plan(&sources, &dst_dir, &opts, &cancel, &mut |sp| {
        last = sp.clone();
    })
    .expect("预扫描应成功");
    assert_eq!(plan.items.len(), 1, "应枚举到 1 个文件");
    assert_eq!(last.files_seen, 1);

    let item = &plan.items[0];
    // src_path 必须是「原始字节」路径，能命中真实文件；lossy 的 src 字符串则含 �
    assert!(item.src_path.exists(), "原始字节路径应能 stat 到真实文件");
    assert!(item.src.contains('\u{fffd}'), "lossy 字符串应把非法字节换成 U+FFFD（这正是 bug 根源）");

    // 2. 用原始字节路径拷贝 —— 之前这里用 Path::new(&lossy字符串) 会 EILSEQ/找不到
    let outcome = copy::copy_file_simple(&item.src_path, &item.dst_path, CopyMode::Fresh, &cancel)
        .expect("非法 UTF-8 文件名必须照样能拷贝");
    assert_eq!(outcome.written as usize, data.len());
    assert!(item.dst_path.exists(), "目标文件应已落盘");

    // 3. 校验阶段：哈希只校验内容，不校验文件名 —— 用原始字节路径能读到并比对通过
    let (h_src, n_src) = hash::hash_file(&item.src_path, HashAlgo::Sha256, &cancel, |_| {}).unwrap();
    let (h_dst, n_dst) = hash::hash_file(&item.dst_path, HashAlgo::Sha256, &cancel, |_| {}).unwrap();
    assert_eq!(n_src as usize, data.len());
    assert_eq!(n_src, n_dst, "源和目标读取的字节数应一致");
    assert_eq!(h_src, h_dst, "内容哈希应一致（与文件名无关）");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn non_utf8_name_detected_by_namecheck() {
    // 顺带钉住：namecheck 能把「非 UTF-8」当作问题报出来（供日志警告）
    let raw = bad_name("x");
    let hit = cinebackup_lib::namecheck::check_name(&raw);
    assert!(hit.is_some(), "非法 UTF-8 名字应被 namecheck 识别");
    let (_, reasons, _) = hit.unwrap();
    assert!(reasons.contains(&"not_utf8"), "应命中 not_utf8 类别");
}
