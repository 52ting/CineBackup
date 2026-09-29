//! 文件名预检 —— 在开拷之前，找出「目标文件系统存不下」的名字
//!
//! 背景：拷贝时报 `Illegal byte sequence (os error 92)`（POSIX errno 92 = EILSEQ）
//! 是**目标文件系统拒绝了这个路径名**，不是文件坏了、不是磁盘满、不是权限不足。
//! `copy.rs` 的分块 `read` / `write_all` / `flush` / `sync_all` 都不会返回 EILSEQ，
//! 能返回的只有路径名层的 `File::create` / `OpenOptions::open` / `create_dir_all`。
//!
//! 所以与其拷到一半才失败，不如在预扫描阶段就把这些名字揪出来，一次性报给用户。
//! 判定规则与 `tools/audit_names.py` 保持一致（六类）：
//!
//! - `not_utf8`      名字不是合法 UTF-8（Windows/FAT/SMB 盘带过来的 GBK 等）
//! - `win_illegal`   含 NTFS/exFAT 不允许的字符 `< > : " / \ | ? *`
//!                   头号嫌疑是冒号 `:`：HFS+/APFS 允许且 Finder 显示成 `/`，
//!                   Mac 上看着没问题，拷到 NTFS/exFAT 就 EILSEQ
//! - `ctrl_char`     含控制字符（不可见，最容易被忽略）
//! - `trailing_dot`  以空格或点结尾（Windows 侧会被静默改写）
//! - `too_long_bytes` 名字超过 255 字节
//! - `win_reserved`  是 Windows 保留设备名（CON / PRN / AUX / NUL / COM1-9 / LPT1-9）
//!
//! 注意：这里判的是**名字的字节层面**（`OsStr` 的原始字节），而不是 `to_string_lossy()`
//! 之后的结果 —— 后者会把非法字节替换成 U+FFFD，反而看不出问题在哪。

/// 单条问题描述（用于日志 / 弹窗）
#[derive(Debug, Clone)]
pub struct NameIssue {
    /// 源路径（`to_string_lossy`，可能含 �）
    pub path: String,
    /// 有问题的那个名字片段（转义后，不可见字符显示为 \uXXXX）
    pub name_display: String,
    /// 命中的问题类别（可多个）
    pub reasons: Vec<&'static str>,
    /// 建议的安全改名（各平台都能存下的名字）
    pub suggestion: String,
}

/// 各问题的中文解释（日志里展开）
pub fn reason_label(r: &str) -> &'static str {
    match r {
        "not_utf8" => "名字不是合法 UTF-8",
        "win_illegal" => "含目标文件系统不允许的字符（如冒号 :）",
        "ctrl_char" => "含不可见控制字符",
        "trailing_dot" => "以空格或点结尾",
        "too_long_bytes" => "名字超过 255 字节",
        "win_reserved" => "是 Windows 保留设备名",
        _ => "未知问题",
    }
}

const WIN_ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

fn win_reserved(stem_upper: &str) -> bool {
    if matches!(stem_upper, "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    // 按字符安全地切出前 3 个字符（不能按字节 split_at，多字节字符会切在中间）
    let mut chars = stem_upper.chars();
    let head: String = chars.by_ref().take(3).collect();
    let tail: String = chars.collect();
    if (head == "COM" || head == "LPT") && !tail.is_empty() && tail.chars().all(|c| ('1'..='9').contains(&c)) {
        return true;
    }
    false
}

fn is_ctrl(ch: char) -> bool {
    matches!(ch, '\u{0000}'..='\u{001f}' | '\u{007f}')
}

/// 把名字里的不可见 / 敏感字符转义出来，避免「看着正常其实有鬼」
fn visible(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if WIN_ILLEGAL.contains(&ch) || is_ctrl(ch) || ch == '\u{fffd}' {
            out.push_str(&format!("\\u{:04x}", ch as u32));
        } else {
            out.push(ch);
        }
    }
    out
}

/// 生成一个「各平台都安全」的建议名（只用于展示，不落盘）
fn safe_name(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if WIN_ILLEGAL.contains(&ch) {
            out.push(if ch == ':' { '：' } else { '_' });
        } else if is_ctrl(ch) || ch == '\u{fffd}' {
            continue;
        } else {
            out.push(ch);
        }
    }
    let mut s = out.trim_end_matches([' ', '.']).to_string();
    if s.is_empty() {
        s = "unnamed".to_string();
    }
    // 按字节截断，且不要截出半个 UTF-8 字符
    if s.len() > 255 {
        let mut cut = 255;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if win_reserved(&stem) {
        s.insert(0, '_');
    }
    s
}

/// 检查单个名字（`OsStr`）。
///
/// 返回 `None` 表示名字干净；`Some((name_display, reasons, suggestion))` 表示有问题。
///
/// 注意跨平台差异：Windows 的 `OsStr` 本身是 UTF-16（合法 Unicode，至多含代理对），
/// 所以「非法 UTF-8」只可能来自 Unix 侧（Mac/Linux 上 `OsStr` 是原始字节）。
pub fn check_name(raw: &std::ffi::OsStr) -> Option<(String, Vec<&'static str>, String)> {
    let mut reasons: Vec<&'static str> = Vec::new();

    // Unix 下先判「是否合法 UTF-8」—— 这是 Mac 上 EILSEQ 的第二大来源
    #[cfg(unix)]
    let text = {
        use std::os::unix::ffi::OsStrExt;
        let b = raw.as_bytes();
        match std::str::from_utf8(b) {
            Ok(t) => t.to_string(),
            Err(_) => {
                reasons.push("not_utf8");
                String::from_utf8_lossy(b).into_owned()
            }
        }
    };
    #[cfg(windows)]
    let text = raw.to_string_lossy().into_owned();

    if text.chars().any(|c| WIN_ILLEGAL.contains(&c)) {
        reasons.push("win_illegal");
    }
    if text.chars().any(is_ctrl) {
        reasons.push("ctrl_char");
    }
    if text.ends_with(' ') || text.ends_with('.') {
        reasons.push("trailing_dot");
    }
    if text.len() > 255 {
        // 用字符数近似字节数：一个非 ASCII 字符的 UTF-8 只可能更长，255 字符必然超 255 字节
        reasons.push("too_long_bytes");
    }
    let stem = text.split('.').next().unwrap_or("").trim().to_ascii_uppercase();
    if win_reserved(&stem) {
        reasons.push("win_reserved");
    }

    if reasons.is_empty() {
        return None;
    }
    Some((visible(&text), reasons, safe_name(&text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[cfg(unix)]
    fn os_from_bytes(b: &[u8]) -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(b.to_vec())
    }

    #[test]
    fn clean_names_pass() {
        assert!(check_name(OsStr::new("cup.psd")).is_none());
        assert!(check_name(OsStr::new("杯垫.psd")).is_none());
        assert!(check_name(OsStr::new("2024_9_17 拍摄.psd")).is_none());
    }

    #[test]
    fn colon_is_flagged() {
        let (_, reasons, _) = check_name(OsStr::new("2024_9_17 10:30 拍摄.psd")).unwrap();
        assert!(reasons.contains(&"win_illegal"));
    }

    #[test]
    fn trailing_space_or_dot_flagged() {
        assert!(check_name(OsStr::new("bad.psd ")).is_some());
        assert!(check_name(OsStr::new("bad.psd.")).is_some());
    }

    #[test]
    fn control_char_flagged() {
        assert!(check_name(OsStr::new("line\nbreak.psd")).is_some());
    }

    #[test]
    fn reserved_name_flagged() {
        assert!(check_name(OsStr::new("CON.psd")).is_some());
        assert!(check_name(OsStr::new("LPT3.txt")).is_some());
    }

    #[test]
    fn too_long_flagged() {
        let long = "a".repeat(300) + ".bin";
        assert!(check_name(OsStr::new(&long)).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_flagged() {
        let raw = os_from_bytes(b"bad\xff\xfe.psd");
        let (_, reasons, _) = check_name(&raw).unwrap();
        assert!(reasons.contains(&"not_utf8"));
    }

    #[test]
    fn suggestion_is_clean() {
        let (_, _, sug) = check_name(OsStr::new("2024_9_17 10:30 拍摄.psd")).unwrap();
        assert!(check_name(OsStr::new(&sug)).is_none());
    }
}
