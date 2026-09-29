//! 回归测试：文件名含「合法 UTF-8 但 APFS 会拒绝的未分配 / 非字符 Unicode 码位」
//! （例如 U+0378、U+FFFE）—— 拷贝流程必须能自动清洗并继续，而不是 EILSEQ 卡死。
//!
//! 关键背景（来自 Apple APFS FAQ）：
//! > APFS doesn't allow files to be created with filenames that contain
//! > unassigned codepoints in the Unicode 9.0 standard.
//!
//! 这类字符：
//! - 是合法 UTF-8（`from_utf8` 通过、`warn_if_non_utf8` 不会报警）
//! - 界面渲染**不可见**（log/弹窗看着像「正常」）
//! - 用户在终端手打的是干净字节 → `cp` 成功
//! - 应用带原始字节走 `File::create` → APFS 拒绝 → EILSEQ
//!
//! 修复链路：
//! 1. `unassigned::is_rejected(cp) -> bool` 二分查表
//! 2. `util::path_has_rejected_codepoint` 探测路径里是否含被拒码位
//! 3. `util::sanitize_path_for_filesystem` 把被拒码位替换成 U+FFFD（`�`），
//!    APFS 接受 U+FFFD（已分配字符），文件能创建成功
//! 4. `util::escape_path_bytes` 把字节以 `\xHH` 形式转义出来，EILSEQ 时打进日志
//! 5. `engine.rs` 遇 EILSEQ 且目标路径含被拒码位 → 自动用清洗后的路径重试一次
//!
//! 这些单测跨平台跑（不依赖真文件系统拒绝未分配码位的行为，只测清洗逻辑），
//! 加上 `namecheck::tests::unassigned_codepoint_flagged` 内联单测即可。
//!
//! ⚠️ e2e 验证（重命名后真实 `File::create`）只能在 macOS 跑；本测试只覆盖修复链路。

use cinebackup_lib::util::{escape_path_bytes, path_has_rejected_codepoint, sanitize_path_for_filesystem};
use cinebackup_lib::unassigned::{first_rejected_in, is_rejected};

#[test]
fn detects_unassigned_codepoint_in_string() {
    // U+0378 自 Unicode 1.1 起就是未分配
    let s = "Cam A\u{0378}/A.mxf";
    let (idx, cp) = first_rejected_in(s).expect("应能探测到被拒码位");
    assert_eq!(cp, 0x0378);
    // idx 是字节偏移。"Cam A" 占 5 字节；U+0378 在 UTF-8 是 CE B8 共 2 字节
    assert_eq!(idx, 5);
    assert_eq!(&s[idx..idx + 2], "\u{0378}");
}

#[test]
fn rejects_known_unassigned_and_noncharacters() {
    // 未分配
    assert!(is_rejected(0x0378));
    assert!(is_rejected(0xFDD0)); // 非字符区
    // 非字符 (Unicode 永久保留，永远不被分配)
    assert!(is_rejected(0xFFFE));
    assert!(is_rejected(0xFFFF));
    assert!(is_rejected(0x1_FFFE));
    assert!(is_rejected(0x10_FFFF));
}

#[test]
fn accepts_normal_and_assigned_codepoints() {
    assert!(!is_rejected(0x4E2D)); // 中
    assert!(!is_rejected(0x00A0)); // NBSP（已分配）
    assert!(!is_rejected(0x1F600)); // emoji
    assert!(!is_rejected(0x300A)); // 《
    assert!(!is_rejected(0xE000)); // 私用区（已分配，APFS 接受）
}

#[test]
fn clean_path_has_no_rejected_codepoint() {
    let p1 = std::path::Path::new(if cfg!(windows) {
        "C:\\Volumes\\X\\Cam A\\clip.mxf"
    } else {
        "/Volumes/X/Cam A/clip.mxf"
    });
    assert!(!path_has_rejected_codepoint(p1));
    let p2 = std::path::Path::new(if cfg!(windows) {
        "C:\\Volumes\\X\\2024_9_17 \u{62cd}\u{62e5}.psd"
    } else {
        "/Volumes/X/2024_9_17 \u{62cd}\u{62e5}.psd"
    });
    assert!(!path_has_rejected_codepoint(p2));
}

#[cfg(unix)]
#[test]
fn dirty_path_is_detected() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let mut bytes = b"/Volumes/foobar/Cam A".to_vec();
    let mut unassigned = [0u8; 2];
    let cp = char::from_u32(0x0378).unwrap().encode_utf8(&mut unassigned);
    bytes.extend_from_slice(cp.as_bytes());
    bytes.extend_from_slice(b"/clip.mxf");
    let p = std::path::PathBuf::from(OsString::from_vec(bytes));
    assert!(path_has_rejected_codepoint(&p));
}

#[test]
fn sanitize_replaces_unassigned_with_replacement_char() {
    let p = std::path::Path::new(if cfg!(windows) {
        "C:\\Volumes\\X\\Cam A\u{0378}\\clip.mxf"
    } else {
        "/Volumes/X/Cam A\u{0378}/clip.mxf"
    });
    let out = sanitize_path_for_filesystem(p);
    // 不再含被拒码位
    assert!(!path_has_rejected_codepoint(&out));
    // 原结构保留（盘符、CAM A、最后）
    let s = out.to_string_lossy();
    assert!(s.contains("Cam A"), "应保留 Cam A 部分, 实际：{}", s);
    assert!(s.contains("clip.mxf"), "应保留 clip.mxf, 实际：{}", s);
    // 中间应是 U+FFFD（替换字符）
    assert!(s.contains('\u{fffd}'), "应含 U+FFFD 替换字符, 实际：{}", s);
}

#[test]
fn sanitize_returns_identical_path_when_clean() {
    let p = std::path::Path::new(if cfg!(windows) {
        "C:\\Volumes\\X\\Cam A\\clip.mxf"
    } else {
        "/Volumes/X/Cam A/clip.mxf"
    });
    let out = sanitize_path_for_filesystem(p);
    // 干净路径走"原样返回"分支，字符串展示应保留原结构
    let orig = p.to_string_lossy();
    assert_eq!(out.to_string_lossy(), orig);
}

#[test]
fn escape_path_bytes_renders_unprintable() {
    let p = std::path::Path::new(if cfg!(windows) {
        "C:\\Volumes\\X\\Cam A\u{0378}\\clip.mxf"
    } else {
        "/Volumes/X/Cam A\u{0378}/clip.mxf"
    });
    let s = escape_path_bytes(p);
    // U+0378 编码为 CD B8（11 位 → 3 字节 UTF-8 首位填 110 01101 10 111000），应被转义成 \xCD\xB8
    assert!(s.contains("\\xCD\\xB8"), "应包含 \\xCD\\xB8 (U+0378 的 UTF-8 编码), 实际：{}", s);
    // 普通可打印字符原样保留
    assert!(s.contains("Cam A"));
    assert!(s.contains("clip.mxf"));
}

#[test]
fn escape_path_bytes_handles_ascii_cleanly() {
    // ASCII-only 路径应无任何 \xHH
    let p = std::path::Path::new(if cfg!(windows) {
        "C:\\Volumes\\X\\clip.mxf"
    } else {
        "/Volumes/X/clip.mxf"
    });
    let s = escape_path_bytes(p);
    assert!(!s.contains("\\x"), "ASCII 路径不应有转义字节, 实际：{}", s);
    assert!(s.contains("clip.mxf"));
}