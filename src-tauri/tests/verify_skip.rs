//! 「跳过当前正在校验的文件」的回归测试
//!
//! 需求：校验过程中能跳过**单个**文件 —— 大文件（尤其网络盘上几百 GB 的）
//! 读一半就够了，用户不该被迫等它读完、更不该为了它取消整个任务。
//!
//! 三条约定，各一条测试：
//! - `t16`：文件**开始前**就点跳过 → 该文件记 `skip`，**不计入 failed / errors**
//! - `t17`：读到**一半**点跳过 → 立刻停手（后续文件不受影响，不会连锁跳过）
//! - `t18`：跳过的文件必须在结果表里是「跳过」栏那一类，且统计口径正确
//!
//! 运行： cargo test --test verify_skip -- --nocapture

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cinebackup_lib::events::{ProgressReport, ScanProgress};
use cinebackup_lib::hash::HashAlgo;
use cinebackup_lib::scan::{self, Plan};
use cinebackup_lib::types::{FileResult, JobOptions};
use cinebackup_lib::verify::{run_verify_with_opts, VerifySinks, VerifyStats};

// ---------------------------------------------------------------- 脚手架

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "cinebackup-vskip-{tag}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

/// 建「源目录 + 目标目录」，并把源原样拷到目标（造出「拷贝已完成、等着校验」的状态）
fn plan_and_mirror(tag: &str, files: &[(&str, Vec<u8>)]) -> (PathBuf, Plan) {
    let root = tmpdir(tag);
    let src_dir = root.join("src");
    let tgt_dir = root.join("tgt");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&tgt_dir).unwrap();
    for (name, data) in files {
        let p = src_dir.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, data).unwrap();
    }

    let cancel = AtomicBool::new(false);
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

/// 跑一次校验并把结果收回来。`on_progress` 可在读到一半时做点什么（比如模拟点「跳过」）。
/// `skip_folder`：传入「跳过整个文件夹」的前缀（`None` 表示不使用该功能）。
fn verify_collect<F>(
    plan: &Plan,
    skip: &AtomicBool,
    skip_folder: &Mutex<Option<PathBuf>>,
    mut on_progress: F,
) -> (VerifyStats, Vec<FileResult>)
where
    F: FnMut(&ProgressReport),
{
    let cancel = AtomicBool::new(false);
    let idx: Vec<usize> = (0..plan.items.len()).collect();
    let total: u64 = plan.items.iter().map(|i| i.size.saturating_mul(2)).sum();

    let mut logs: Vec<String> = Vec::new();
    let mut results: Vec<FileResult> = Vec::new();
    let stats;
    {
        let mut log = |_lv: &str, m: String| logs.push(m);
        let mut result = |r: &FileResult| results.push(r.clone());
        let mut progress = |p: &ProgressReport| on_progress(p);
        stats = run_verify_with_opts(
            &plan.items,
            &idx,
            HashAlgo::Sha256,
            &cancel,
            skip,
            skip_folder,
            total,
            0,     // 节流关掉：每个 4 MiB 块都上报，以便中途触发跳过
            false, // 分片调试关闭
            VerifySinks {
                log: &mut log,
                result: &mut result,
                progress: &mut progress,
            },
        );
    }
    for l in &logs {
        println!("[log] {l}");
    }
    assert_eq!(results.len(), plan.items.len(), "每个文件都该出一条结果");
    (stats, results)
}

// ---------------------------------------------------------------- t16 / t17 / t18

/// 文件开始前就点了跳过 → 记 `skip`，**不计入 failed / errors**，其余文件正常校验。
#[test]
fn t16_skip_before_start_marks_skip_and_keeps_others_verified() {
    let (_root, plan) = plan_and_mirror(
        "t16",
        &[
            ("a.bin", vec![0x11u8; 4096]),
            ("b.bin", vec![0x22u8; 4096]),
            ("c.bin", vec![0x33u8; 4096]),
        ],
    );
    assert_eq!(plan.items.len(), 3);

    // 预先置位：会让**第一个**文件被跳过，随后标志被消费，剩下两个正常校验
    let skip = Arc::new(AtomicBool::new(true));
    let (stats, results) = verify_collect(&plan, &skip, &Mutex::new(None), |_| {});

    println!(
        "[t16] pass={} failed={} errors={} skipped={}",
        stats.pass, stats.failed, stats.errors, stats.skipped
    );
    assert_eq!(stats.skipped, 1, "应正好跳过 1 个文件");
    assert_eq!(stats.pass, 2, "其余 2 个文件应正常校验通过");
    assert_eq!(stats.failed, 0, "跳过**不算**失败");
    assert_eq!(stats.errors, 0, "跳过**不算**读取错误");

    let skipped: Vec<&FileResult> = results.iter().filter(|r| r.status == "skip").collect();
    assert_eq!(skipped.len(), 1);
    assert!(
        skipped[0].message.contains("用户手动跳过"),
        "说明里要写明是人为跳过，便于与「校验通过」区分：{}",
        skipped[0].message
    );
    // 跳过的那条不能带哈希值（没读过）
    assert!(skipped[0].src_hash.is_empty() && skipped[0].dst_hash.is_empty());
}

/// 读到一半点跳过 → 立刻停手；**后续文件不受影响**（不能连锁跳过）。
#[test]
fn t17_skip_mid_file_stops_early_without_cascading() {
    // 第一个文件 20 MiB（5 个 4 MiB 块，够中途打断），第二个是普通小文件
    let (_root, plan) = plan_and_mirror(
        "t17",
        &[
            ("big.bin", vec![0x7Au8; 20 * 1024 * 1024]),
            ("small.bin", vec![0x5Cu8; 8192]),
        ],
    );
    assert_eq!(plan.items.len(), 2);

    let skip = Arc::new(AtomicBool::new(false));
    let s2 = skip.clone();
    // 第一次收到进度（= 正在读第一个文件）就把 skip 置上，只置一次
    let mut armed = true;
    let (stats, results) = verify_collect(&plan, &skip, &Mutex::new(None), move |_p| {
        if armed {
            armed = false;
            s2.store(true, Ordering::SeqCst);
        }
    });

    println!(
        "[t17] pass={} failed={} errors={} skipped={}",
        stats.pass, stats.failed, stats.errors, stats.skipped
    );
    assert_eq!(stats.skipped, 1, "只应跳过被打断的那一个文件");
    assert_eq!(
        stats.pass, 1,
        "第二个文件必须**继续正常校验** —— 不能因为一次跳过就停掉整轮"
    );
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.errors, 0);

    // 跳过的是大文件，且读了「一部分」而不是 0
    let sk = results.iter().find(|r| r.status == "skip").expect("应有跳过行");
    let big = plan.items.iter().find(|i| i.size > 1024 * 1024).unwrap();
    assert_eq!(sk.path, big.src, "被跳过的应是那个大文件");
    assert!(
        sk.message.contains("已读"),
        "中途跳过应报出已读量，便于判断卡在哪：{}",
        sk.message
    );
}

/// 统计口径与「跳过栏」归类：`status=skip` 既不是 pass 也不是 fail/error。
/// 前端据 `status` 归到「跳过」栏；这里把归类的输入钉住，避免以后改坏。
#[test]
fn t18_skip_status_is_neither_pass_nor_fail() {
    let (_root, plan) = plan_and_mirror("t18", &[("only.bin", vec![0x99u8; 2048])]);

    let skip = Arc::new(AtomicBool::new(true));
    let (stats, results) = verify_collect(&plan, &skip, &Mutex::new(None), |_| {});

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, "skip", "前端只认这个字符串来归到「跳过」栏");
    assert_eq!(stats.skipped, 1);
    assert_eq!(stats.pass, 0);
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.errors, 0);
}

// ---------------------------------------------------------------- t19：跳过整个文件夹

/// 选「跳过整个文件夹」→ 该目录（含子目录）下所有文件都记 `skip`，
/// 目录外的文件**不受影响**继续正常校验；且文件夹跳过是**持续**生效的
/// （不像单文件跳过那样消费一次就没了）。
#[test]
fn t19_skip_folder_skips_whole_tree_and_keeps_others() {
    let (_root, plan) = plan_and_mirror(
        "t19",
        &[
            ("sub/a.bin", vec![0x11u8; 4096]),
            ("sub/deep/b.bin", vec![0x22u8; 4096]), // 子目录里的也要跳
            ("root1.bin", vec![0x33u8; 4096]),       // 目录外：必须正常校验
            ("root2.bin", vec![0x44u8; 4096]),       // 目录外：必须正常校验
        ],
    );
    assert_eq!(plan.items.len(), 4);

    // 目标文件夹就是源根目录下的 sub/ —— 前端会从「当前文件」路径取父目录传进来
    let folder = _root.join("src").join("sub");
    let skip = Arc::new(AtomicBool::new(false));
    let (stats, results) = verify_collect(&plan, &skip, &Mutex::new(Some(folder)), |_| {});

    println!(
        "[t19] pass={} failed={} errors={} skipped={}",
        stats.pass, stats.failed, stats.errors, stats.skipped
    );
    assert_eq!(stats.skipped, 2, "sub 及其子目录下的 2 个文件都应被跳过");
    assert_eq!(stats.pass, 2, "目录外的 2 个文件应正常校验通过");
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.errors, 0);

    let skipped: Vec<&FileResult> = results.iter().filter(|r| r.status == "skip").collect();
    assert_eq!(skipped.len(), 2);
    for r in &skipped {
        assert!(
            r.path.replace('\\', "/").contains("sub/"),
            "被跳过的都应属于 sub 目录：{}",
            r.path
        );
        assert!(r.src_hash.is_empty() && r.dst_hash.is_empty(), "跳过的没读过，不带哈希");
    }
    let passed: Vec<&FileResult> = results.iter().filter(|r| r.status == "pass").collect();
    assert_eq!(passed.len(), 2);
    for r in &passed {
        assert!(
            !r.path.replace('\\', "/").contains("sub/"),
            "目录外文件必须照常校验：{}",
            r.path
        );
    }
}
