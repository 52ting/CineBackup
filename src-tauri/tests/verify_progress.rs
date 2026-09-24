//! 校验阶段**进度上报频率**的回归测试
//!
//! 背景（真机 bug，0.4.4 及以前）：
//! `verify.rs` 只在一个文件**读完**之后才发一次进度。一个文件要读「源 + 目标」
//! 两遍，2 GB 的素材在 64 MB/s 上就是 60 秒以上 —— 界面在这段时间里完全静止，
//! 用户看到的就是「校验时底下进度条数据都不会动」。
//! 更糟的是，那次收尾上报还要过 150ms 的节流窗口：小文件几百毫秒能连读好几个，
//! 它们的进度会被整段吞掉（结果表在涨、进度条不动）。
//!
//! 两条约定，各一条测试（用 `progress_ms` 的两个极端把两种情形分开）：
//! - `t14`：大文件**边读边发**（间隔设 0 → 每个 4 MiB 块都上报，并存在中间态）
//! - `t15`：每个文件收尾都**强制**上报一条（间隔设得极大 → 边界上报一条不缺）
//!
//! 运行： cargo test --test verify_progress -- --nocapture

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use cinebackup_lib::events::{ProgressReport, ScanProgress};
use cinebackup_lib::hash::HashAlgo;
use cinebackup_lib::scan::{self, Plan};
use cinebackup_lib::types::{FileResult, JobOptions};
use cinebackup_lib::verify::{run_verify_with, VerifySinks, VerifyStats};

// ---------------------------------------------------------------- 脚手架

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("cinebackup-vp-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

/// 建一个「源目录 + 空目标目录」的计划，然后把源原样拷到目标，
/// 造出「拷贝阶段已完成、等着校验」的状态。
fn plan_and_mirror(tag: &str, files: &[(&str, Vec<u8>)]) -> (PathBuf, Plan) {
    let root = tmpdir(tag);
    let src_dir = root.join("src");
    let tgt_dir = root.join("tgt");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&tgt_dir).unwrap();
    for (name, data) in files {
        fs::write(src_dir.join(name), data).unwrap();
    }

    let cancel = AtomicBool::new(false);
    // quick_scan 打开：预扫描不做哈希比对（这里只关心校验阶段的进度，不关心判定）
    let opts = JobOptions {
        quick_scan: true,
        ..JobOptions::default()
    };
    let mut cb = |_sp: &ScanProgress| {};
    let plan = scan::build_plan(
        &[src_dir.to_string_lossy().into_owned()],
        &tgt_dir,
        &opts,
        &cancel,
        &mut cb,
    )
    .expect("建计划失败");

    for it in &plan.items {
        let dst = PathBuf::from(&it.dst);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::copy(&it.src, &dst).unwrap();
    }
    (root, plan)
}

/// 跑一次校验，把「进度」出口全部收集回来。`progress_ms` 见 [`run_verify_with`]。
fn verify_collect(plan: &Plan, progress_ms: u64) -> (VerifyStats, Vec<ProgressReport>) {
    let cancel = AtomicBool::new(false);
    let idx: Vec<usize> = (0..plan.items.len()).collect();
    let total: u64 = plan.items.iter().map(|i| i.size.saturating_mul(2)).sum();

    let mut logs: Vec<String> = Vec::new();
    let mut results: Vec<FileResult> = Vec::new();
    let mut reports: Vec<ProgressReport> = Vec::new();
    let stats;
    {
        let mut log = |_lv: &str, m: String| logs.push(m);
        let mut result = |r: &FileResult| results.push(r.clone());
        let mut progress = |p: &ProgressReport| reports.push(p.clone());
        stats = run_verify_with(
            &plan.items,
            &idx,
            HashAlgo::Sha256,
            &cancel,
            total,
            progress_ms,
            VerifySinks {
                log: &mut log,
                result: &mut result,
                progress: &mut progress,
            },
        );
    }
    assert_eq!(results.len(), plan.items.len(), "每个文件都该出一条结果");
    (stats, reports)
}

// ---------------------------------------------------------------- t14 / t15

/// 一个 8 MiB 的文件 = 2 个 4 MiB 块；源 + 目标各一遍 → 4 次读取回调。
/// 间隔设 0（节流关掉）时上报条数必须 ≫ 1，且必须存在「文件还没读完」的中间态。
#[test]
fn t14_verify_reports_progress_while_reading_a_big_file() {
    let (root, plan) = plan_and_mirror("t14", &[("big.bin", vec![0x5Au8; 8 * 1024 * 1024])]);
    assert_eq!(plan.items.len(), 1, "应识别出 1 个文件");
    let total: u64 = plan.items[0].size.saturating_mul(2);

    let (stats, reports) = verify_collect(&plan, 0);
    assert_eq!(stats.pass, 1, "内容一致，应通过");
    assert_eq!(stats.bytes, total, "应把源与目标都完整读一遍");

    println!("[t14] 8 MiB 文件：上报 {} 条进度", reports.len());
    assert!(
        reports.len() >= 4,
        "只上报了 {} 条 —— 说明进度是「每个文件一条」而不是「边读边发」",
        reports.len()
    );

    // 关键断言：读到一半时就该有上报（current_file_done 落在 0 与总量之间）
    let mid = reports
        .iter()
        .find(|p| p.current_file_done > 0 && p.current_file_done < p.current_file_total);
    assert!(
        mid.is_some(),
        "没有任何中间态上报 —— 大文件读到一半时界面上什么都看不到"
    );
    // 中间态的文件计数必须是「不含正在读的这个」（i 而不是 i+1），否则会虚报完成
    assert_eq!(
        mid.unwrap().files_done,
        0,
        "读到一半不能把当前文件算成已完成"
    );

    // 最后一条是收尾：文件计数 +1、字节数走满
    let last = reports.last().unwrap();
    assert_eq!(last.files_done, 1, "收尾上报应把文件计数推到 1");
    assert_eq!(last.bytes_done, total, "收尾时字节数应等于总量");
    assert_eq!(
        last.current_file_done, last.current_file_total,
        "收尾时当前文件应读满"
    );
    assert_eq!(last.phase, "verify");
    assert!(last.current_file.ends_with("big.bin"), "当前文件名应带上");

    let _ = root;
}

/// 10 个小文件 + 极大的节流间隔：中间态一律被窗口挡住，只剩收尾的**强制**上报。
/// 所以「每个文件的边界」都必须出现一次 —— 一个都不能少。
#[test]
fn t15_every_file_boundary_forces_a_progress_report() {
    let names: Vec<String> = (0..10).map(|i| format!("f{i:02}.bin")).collect();
    let owned: Vec<(String, Vec<u8>)> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), vec![i as u8 + 1; 64 * 1024]))
        .collect();
    let refs: Vec<(&str, Vec<u8>)> = owned.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();

    let (root, plan) = plan_and_mirror("t15", &refs);
    assert_eq!(plan.items.len(), 10, "应识别出 10 个文件");

    let (stats, reports) = verify_collect(&plan, 60_000);
    assert_eq!(stats.pass, 10, "10 个文件都应通过");

    let done: BTreeSet<u64> = reports.iter().map(|p| p.files_done).collect();
    println!(
        "[t15] 10 个小文件（间隔 60s）：上报 {} 条，files_done 覆盖 {:?}",
        reports.len(),
        done
    );

    // 收尾上报必须一条不缺：files_done 要能取遍 1..=10
    for k in 1..=10u64 {
        assert!(
            done.contains(&k),
            "第 {k} 个文件收尾没有上报进度（files_done 只覆盖 {done:?}）—— \
             小文件的进度被节流窗口吞了，界面会「结果表在涨、进度条不动」"
        );
    }
    let total: u64 = plan.items.iter().map(|i| i.size.saturating_mul(2)).sum();
    let last = reports.last().unwrap();
    assert_eq!(last.files_done, 10);
    assert_eq!(last.bytes_done, total, "收尾时字节数应等于总量");
    assert_eq!(
        last.current_file_done, last.current_file_total,
        "收尾时当前文件应读满"
    );

    let _ = root;
}
