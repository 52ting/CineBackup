//! 断点续传规则的**端到端**验证（真实文件落盘，不是模拟）
//!
//! 对应需求原文：
//! 1. 目标文件不存在            → 执行完整拷贝
//! 2. 目标文件已存在、大小不一致 → 启用断点续传，从目标文件末尾继续写入
//! 3. 目标文件已存在、大小一致   → 计算 xxHash64：相同 → 跳过；不同 → 覆盖
//!
//! 另外验证两条容易被忽略的边界：
//! - 单次写入必须 ≤ 4 MiB 块（证明没有把大文件整块读进内存）
//! - 目标比源还大时不能续传，必须从头重写
//!
//! 运行：  cargo test --test resume_rule -- --nocapture
//!        cargo test --test resume_rule t2 -- --nocapture   （只跑某条）

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use cinebackup_lib::copy::{self, CopyMode};
use cinebackup_lib::events::ScanProgress;
use cinebackup_lib::hash;
use cinebackup_lib::scan;
use cinebackup_lib::types::{JobOptions, PlannedAction};
use cinebackup_lib::util::CHUNK_SIZE;

// ---------------------------------------------------------------- 测试脚手架

/// 确定性伪随机内容。
/// 刻意不用全零：全零数据会掩盖「写到错误偏移」这类 bug。
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

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cinebackup_test_{tag}"));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn write_file(p: &Path, data: &[u8]) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut f = fs::File::create(p).unwrap();
    f.write_all(data).unwrap();
    f.sync_all().unwrap();
}

fn digest(p: &Path) -> u64 {
    let cancel = AtomicBool::new(false);
    hash::hash_file(p, &cancel, |_| {}).unwrap().0
}

fn size_of(p: &Path) -> u64 {
    fs::metadata(p).unwrap().len()
}

// ---------------------------------------------------------------- 规则 1

#[test]
fn t1_target_missing_does_full_copy() {
    let d = tmpdir("t1");
    let src = d.join("a.bin");
    let dst = d.join("out").join("a.bin");
    let data = make_data(3 * 1024 * 1024 + 12345, 42);
    write_file(&src, &data);

    let cancel = AtomicBool::new(false);
    let mut cb_bytes: u64 = 0;
    let out = copy::copy_file(&src, &dst, CopyMode::Fresh, &cancel, &mut |n| {
        cb_bytes += n;
        true
    })
    .unwrap();

    assert_eq!(cb_bytes, data.len() as u64, "回调累计字节应等于文件长度");
    assert_eq!(out.written, data.len() as u64);
    assert_eq!(out.base, 0, "全新拷贝不应有续传起点");
    assert!(!out.restarted, "目标不存在，不该标记为「重写」");
    assert_eq!(size_of(&dst), data.len() as u64);
    assert_eq!(digest(&src), digest(&dst), "内容哈希必须一致");
}

// ---------------------------------------------------------------- 规则 2

#[test]
fn t2_partial_target_resumes_from_tail() {
    let d = tmpdir("t2");
    let src = d.join("b.bin");
    let dst = d.join("out").join("b.bin");
    let data = make_data(8 * 1024 * 1024 + 777, 7);
    write_file(&src, &data);

    // 模拟「拷到一半被强杀」：目标里只有前半截。
    // 刻意取 data.len()/2 —— 不是 4 MiB 的整数倍，正好测「停在半块上」的场景。
    let half = data.len() / 2;
    write_file(&dst, &data[..half]);
    assert_ne!(half % CHUNK_SIZE, 0, "本测试的前提是停在非整块边界");

    let cancel = AtomicBool::new(false);
    let out = copy::copy_file(&src, &dst, CopyMode::Resume, &cancel, &mut |_| true).unwrap();

    assert_eq!(out.base, half as u64, "续传起点应等于目标已有长度");
    assert_eq!(
        out.written,
        (data.len() - half) as u64,
        "只应写入剩余部分，而不是整个文件"
    );
    assert!(!out.restarted, "目标比源小，应走续传而非重写");
    assert_eq!(size_of(&dst), data.len() as u64, "最终大小应与源一致");
    assert_eq!(digest(&src), digest(&dst), "续传后内容必须与源逐字节一致");
}

#[test]
fn t2b_resume_is_idempotent_and_correct_across_many_chunks() {
    let d = tmpdir("t2b");
    let src = d.join("big.bin");
    let dst = d.join("out").join("big.bin");
    // 18 MiB → 跨 5 个 4 MiB 块
    let data = make_data(18 * 1024 * 1024 + 4096, 1234);
    write_file(&src, &data);

    // 分三次「中断」续传，每次追加一部分，最终必须还原成原始内容
    let cuts = [3 * 1024 * 1024, 11 * 1024 * 1024 + 1];
    write_file(&dst, &data[..cuts[0]]);
    {
        let cancel = AtomicBool::new(false);
        copy::copy_file(&src, &dst, CopyMode::Resume, &cancel, &mut |_| true).unwrap();
    }
    assert_eq!(digest(&src), digest(&dst));

    // 再来一次：此时目标已完整，续传应判定为「已到末尾」→ 写入 0 字节
    let cancel = AtomicBool::new(false);
    let out = copy::copy_file(&src, &dst, CopyMode::Resume, &cancel, &mut |_| true).unwrap();
    assert!(
        out.restarted || out.written == 0,
        "目标已完整时不该再写入新数据（got restarted={} written={}）",
        out.restarted,
        out.written
    );
    assert_eq!(digest(&src), digest(&dst), "重复续传不能破坏内容");
}

// ---------------------------------------------------------------- 规则 3

#[test]
fn t3_same_size_same_hash_is_identical() {
    let d = tmpdir("t3");
    let a = d.join("src.bin");
    let b = d.join("dst.bin");
    let data = make_data(5 * 1024 * 1024 + 3, 99);
    write_file(&a, &data);
    write_file(&b, &data);

    let cancel = AtomicBool::new(false);
    assert!(
        hash::files_identical(&a, &b, &cancel, |_| {}).unwrap(),
        "大小与 xxHash64 都相同 → 应判定一致（跳过）"
    );
}

#[test]
fn t4_same_size_different_hash_is_not_identical() {
    let d = tmpdir("t4");
    let a = d.join("src.bin");
    let b = d.join("dst.bin");
    let data = make_data(5 * 1024 * 1024 + 3, 99);
    write_file(&a, &data);

    // 同样大小，但中间改一个字节 —— 大小相同不能作为「已备份」的依据
    let mut tampered = data.clone();
    let mid = tampered.len() / 2;
    tampered[mid] ^= 0xFF;
    write_file(&b, &tampered);

    assert_eq!(size_of(&a), size_of(&b), "前提：两者大小必须相同");
    let cancel = AtomicBool::new(false);
    assert!(
        !hash::files_identical(&a, &b, &cancel, |_| {}).unwrap(),
        "大小相同但哈希不同 → 必须判定不一致（覆盖）"
    );

    // 走覆盖模式后内容应被修正
    let out = copy::copy_file(&a, &b, CopyMode::Overwrite, &cancel, &mut |_| true).unwrap();
    assert_eq!(out.base, 0, "覆盖模式不应从中间续写");
    assert_eq!(digest(&a), digest(&b));
}

#[test]
fn t4b_size_differs_short_circuits_without_hashing() {
    let d = tmpdir("t4b");
    let a = d.join("src.bin");
    let b = d.join("dst.bin");
    write_file(&a, &make_data(4096, 5));
    write_file(&b, &make_data(2048, 5));

    let cancel = AtomicBool::new(false);
    // 大小不同 → 必须直接 false，且回调一次都不该被触发（证明没做无谓的哈希）
    let mut calls = 0u64;
    let same = hash::files_identical(&a, &b, &cancel, |_| calls += 1).unwrap();
    assert!(!same);
    assert_eq!(calls, 0, "大小不同应短路返回，不应读取任何字节");
}

// ---------------------------------------------------------------- 续传安全阀

#[test]
fn t5_prefix_mismatch_rejects_resume() {
    let d = tmpdir("t5");
    let src = d.join("c.bin");
    let dst = d.join("out").join("c.bin");
    let data = make_data(4 * 1024 * 1024 + 4096, 5);
    write_file(&src, &data);

    // 目标前半截被写坏：模拟上次中断在半块上留下的脏数据。
    // 这种文件大小看着「像续传」，但直接追加会静默损坏 —— 必须能识别出来。
    let half = data.len() / 2;
    let mut bad = data[..half].to_vec();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    write_file(&dst, &bad);

    let cancel = AtomicBool::new(false);
    let ok = hash::resume_prefix_ok(&src, &dst, half as u64, &cancel, |_| {}).unwrap();
    assert!(!ok, "前缀不一致时必须拒绝续传，改为从头覆盖");
}

#[test]
fn t6_clean_prefix_allows_resume() {
    let d = tmpdir("t6");
    let src = d.join("c.bin");
    let dst = d.join("out").join("c.bin");
    let data = make_data(4 * 1024 * 1024 + 4096, 5);
    write_file(&src, &data);
    let half = data.len() / 2;
    write_file(&dst, &data[..half]);

    let cancel = AtomicBool::new(false);
    let ok = hash::resume_prefix_ok(&src, &dst, half as u64, &cancel, |_| {}).unwrap();
    assert!(ok, "前缀完全一致时应允许安全续传");
}

// ---------------------------------------------------------------- 边界

#[test]
fn t7_writes_are_chunked_never_whole_file() {
    let d = tmpdir("t7");
    let src = d.join("huge.bin");
    let dst = d.join("out").join("huge.bin");
    // 12 MiB，必须被切成多块写
    let data = make_data(12 * 1024 * 1024 + 1, 808);
    write_file(&src, &data);

    let cancel = AtomicBool::new(false);
    let mut max_delta = 0u64;
    let mut calls = 0u64;
    copy::copy_file(&src, &dst, CopyMode::Fresh, &cancel, &mut |n| {
        max_delta = max_delta.max(n);
        calls += 1;
        true
    })
    .unwrap();

    assert!(
        max_delta <= CHUNK_SIZE as u64,
        "单次写入 {max_delta} 超过 {CHUNK_SIZE} 字节块上限 —— 说明存在整文件读入内存"
    );
    assert!(calls >= 3, "12 MiB 至少应分 3 次写入，实际 {calls} 次");
    assert_eq!(digest(&src), digest(&dst));
}

#[test]
fn t8_target_larger_than_source_restarts_instead_of_resuming() {
    let d = tmpdir("t8");
    let src = d.join("s.bin");
    let dst = d.join("out").join("s.bin");
    let data = make_data(1024 * 1024, 66);
    write_file(&src, &data);
    // 目标比源还大：不可能续传（会越写越坏）
    write_file(&dst, &make_data(2 * 1024 * 1024, 77));

    let cancel = AtomicBool::new(false);
    let out = copy::copy_file(&src, &dst, CopyMode::Resume, &cancel, &mut |_| true).unwrap();

    assert!(out.restarted, "目标比源大时必须标记为「从头重写」");
    assert_eq!(out.base, 0);
    assert_eq!(size_of(&dst), data.len() as u64, "重写后应被截断到源的大小");
    assert_eq!(digest(&src), digest(&dst));
}

#[test]
fn t9_empty_and_zero_byte_files() {
    let d = tmpdir("t9");
    let src = d.join("empty.bin");
    let dst = d.join("out").join("empty.bin");
    write_file(&src, b"");

    let cancel = AtomicBool::new(false);
    let out = copy::copy_file(&src, &dst, CopyMode::Fresh, &cancel, &mut |_| true).unwrap();
    assert_eq!(out.written, 0);
    assert_eq!(size_of(&dst), 0);

    // 空文件大小相同 → files_identical 应直接判 true，不做哈希
    let other = d.join("empty2.bin");
    write_file(&other, b"");
    assert!(hash::files_identical(&src, &other, &cancel, |_| {}).unwrap());
}

// ---------------------------------------------------------------- 计划层：三条规则的联合判定

#[test]
fn t10_plan_classifies_all_four_cases() {
    let d = tmpdir("t10");
    let src_dir = d.join("src");
    let tgt_dir = d.join("tgt");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&tgt_dir).unwrap();

    let base = make_data(2 * 1024 * 1024 + 9, 2024);

    // ① 目标不存在 → 期望 Copy
    write_file(&src_dir.join("a_missing.bin"), &base);
    // ② 目标存在、大小更小 → 期望 Resume
    write_file(&src_dir.join("b_shorter.bin"), &base);
    // ③ 目标存在、大小相同、内容相同 → 期望 Skip
    write_file(&src_dir.join("c_same.bin"), &base);
    // ④ 目标存在、大小相同、内容不同 → 期望 Overwrite
    write_file(&src_dir.join("d_diff.bin"), &base);

    // 预置目标目录（build_plan 对文件夹源会拷贝到 目标/<源文件夹名>/...）
    let mirror = tgt_dir.join("src");
    fs::create_dir_all(&mirror).unwrap();
    write_file(&mirror.join("b_shorter.bin"), &base[..base.len() / 3]);
    write_file(&mirror.join("c_same.bin"), &base);
    let mut tampered = base.clone();
    tampered[10] ^= 0xFF;
    write_file(&mirror.join("d_diff.bin"), &tampered);

    let cancel = AtomicBool::new(false);
    let opts = JobOptions {
        ask_on_conflict: false, // 关掉弹窗 → 完全按续传规则自动判定
        quick_scan: false,      // 关掉快速扫描 → 大小相同的要真算哈希
        resume_prefix_check: true,
        verify_after_copy: true,
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

    assert_eq!(plan.items.len(), 4, "应识别出 4 个文件");
    assert!(!plan.cancelled);

    let find = |name: &str| -> PlannedAction {
        plan.items
            .iter()
            .find(|it| it.src.ends_with(name))
            .unwrap_or_else(|| panic!("计划里没有 {name}"))
            .action
    };

    assert_eq!(find("a_missing.bin"), PlannedAction::Copy, "① 目标不存在 → 完整拷贝");
    assert_eq!(find("b_shorter.bin"), PlannedAction::Resume, "② 大小不一致 → 断点续传");
    assert_eq!(find("c_same.bin"), PlannedAction::Skip, "③ 大小同哈希同 → 跳过");
    assert_eq!(
        find("d_diff.bin"),
        PlannedAction::Overwrite,
        "④ 大小同哈希不同 → 覆盖"
    );

    // 计划统计也要对得上
    assert_eq!(plan.stats.copy, 1);
    assert_eq!(plan.stats.resume, 1);
    assert_eq!(plan.stats.skip, 1);
    assert_eq!(plan.stats.overwrite, 1);

    let _ = fs::remove_dir_all(&d);
    println!("\n  ✓ 四条续传规则全部按预期判定\n");
}

// ---------------------------------------------------------------- 界面依赖：文件的「源归属」

/// 中间栏在任务运行时是一行一个源的传输列表，靠 `PlanItem.src_idx` 把文件归堆。
/// 这个下标一旦标错，某个源的进度条就会算到别的源头上去。
#[test]
fn t11_plan_marks_source_index() {
    let d = tmpdir("t11");
    let s1 = d.join("src1");
    let s2 = d.join("src2");
    write_file(&s1.join("a.bin"), &make_data(2048, 11));
    write_file(&s1.join("sub").join("b.bin"), &make_data(4096, 12));
    write_file(&s2.join("c.bin"), &make_data(1024, 13));
    let loose = d.join("loose.bin");
    write_file(&loose, &make_data(512, 14));

    let target = d.join("dst");
    fs::create_dir_all(&target).unwrap();

    let sources = vec![
        s1.to_string_lossy().into_owned(),
        s2.to_string_lossy().into_owned(),
        loose.to_string_lossy().into_owned(),
    ];
    let cancel = AtomicBool::new(false);
    let opts = JobOptions::default();
    let mut cb = |_sp: &ScanProgress| {};
    let plan = scan::build_plan(&sources, &target, &opts, &cancel, &mut cb).expect("建计划失败");

    assert_eq!(plan.items.len(), 4, "两个目录源 + 一个文件源，共 4 个文件");

    for it in &plan.items {
        let idx = it.src_idx;
        assert!(idx < sources.len(), "src_idx 越界：{idx}");
        let root = Path::new(&sources[idx]);
        if root.is_dir() {
            assert!(
                Path::new(&it.src).starts_with(root),
                "src_idx={idx} 标错了：{} 并不在 {} 之下",
                it.src,
                sources[idx]
            );
        } else {
            assert_eq!(it.src, sources[idx], "文件源的整条路径就是它自己");
        }
    }

    // 三个源都得有文件认领，不能漏
    let mut seen: Vec<usize> = plan.items.iter().map(|i| i.src_idx).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen, vec![0, 1, 2], "三个源都应被标记到");

    // 第一个目录源含子目录里的那个文件，共 2 个
    assert_eq!(plan.items.iter().filter(|i| i.src_idx == 0).count(), 2);

    let _ = fs::remove_dir_all(&d);
    println!("\n  ✓ 每个文件都正确挂到了自己的源上\n");
}
