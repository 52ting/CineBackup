//! 磁盘文件系统类型探测
//!
//! - **macOS / Unix**：`statfs(2)` 取 `f_fstypename`（apfs / hfs / exfat / msdos / ntfs …）
//! - **Windows**：`GetVolumePathNameW` 定位卷挂载点，再 `GetVolumeInformationW` 取文件系统名
//!   （NTFS / FAT32 / exFAT / ReFS …）
//!
//! 全部为只读的系统调用，不会挂载/修改任何卷。

use std::path::{Path, PathBuf};

/// 对外的唯一入口：返回人类可读的文件系统名，失败返回 "未知"
pub fn fs_type_of(path: &Path) -> String {
    // 路径可能还不存在（比如刚选的空目录、或者跨平台加载的任务），
    // 逐级向上找到第一个真实存在的祖先再探测，保证能拿到所在卷。
    let probe = find_existing_ancestor(path).unwrap_or_else(|| PathBuf::from(path));
    match fs_type_raw(&probe) {
        Some(raw) => prettify(&raw),
        None => "未知".to_string(),
    }
}

/// 向上回溯，找到第一个存在的路径
pub fn find_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(path);
    while let Some(p) = cur {
        if p.exists() {
            return Some(p.to_path_buf());
        }
        cur = p.parent();
    }
    None
}

/// 该文件系统是否「在 macOS 上只能只读」——用于前端黄色警告
pub fn is_readonly_on_macos(fs: &str) -> bool {
    fs.eq_ignore_ascii_case("ntfs")
}

// ---------------------------------------------------------------- 原始探测

#[cfg(target_os = "macos")]
fn fs_type_raw(path: &Path) -> Option<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    unsafe {
        let mut buf: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut buf) != 0 {
            return None;
        }
        // f_fstypename: [c_char; 16]
        let raw: Vec<u8> = buf
            .f_fstypename
            .iter()
            .take_while(|&&ch| ch != 0)
            .map(|&ch| ch as u8)
            .collect();
        if raw.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&raw).into_owned())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn fs_type_raw(path: &Path) -> Option<String> {
    // 非 macOS 的 unix（本工具不发布 Linux 版）——尝试 statfs 的 f_type 数值
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    unsafe {
        let mut buf: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut buf) != 0 {
            return None;
        }
        Some(format!("fs_type={:#x}", buf.f_type as u64))
    }
}

#[cfg(windows)]
fn fs_type_raw(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetVolumeInformationW, GetVolumePathNameW,
    };

    // 1) 先拿到该路径所属卷的挂载点，例如 "D:\"
    let mut root = [0u16; 261]; // MAX_PATH
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
    if ok == 0 {
        return None;
    }

    // 2) 取文件系统名
    let mut fs_name = [0u16; 64];
    let mut serial: u32 = 0;
    let mut max_comp: u32 = 0;
    let mut flags: u32 = 0;
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(), // 卷标，不需要
            0,
            &mut serial,
            &mut max_comp,
            &mut flags,
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let len = fs_name.iter().position(|&c| c == 0).unwrap_or(fs_name.len());
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&fs_name[..len]))
}

#[cfg(not(any(unix, windows)))]
fn fs_type_raw(_path: &Path) -> Option<String> {
    None
}

// ---------------------------------------------------------------- 名称美化

/// 把系统返回的裸名字翻译成界面上好读的写法。
/// 供 `disks.rs` 复用，保证「磁盘列表」和「路径探测」两处显示完全一致。
pub fn prettify(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    // Paragon / Tuxera 等第三方 NTFS 驱动会带前缀
    let l: &str = if let Some(s) = lower.strip_prefix("ufsd_") {
        s
    } else if let Some(s) = lower.strip_prefix("tuxera_") {
        s
    } else {
        lower.as_str()
    };
    match l {
        "apfs" => "APFS".into(),
        "hfs" => "HFS+".into(),
        "exfat" => "exFAT".into(),
        "msdos" | "vfat" | "fat32" => "FAT32".into(),
        "fat16" => "FAT16".into(),
        "ntfs" | "ntfs3" | "fuseblk" => "NTFS".into(),
        "refs" => "ReFS".into(),
        "smbfs" | "cifs" => "SMB/CIFS".into(),
        "nfs" | "nfs4" => "NFS".into(),
        "webdav" => "WebDAV".into(),
        "udf" => "UDF".into(),
        "cd9660" | "iso9660" => "ISO9660".into(),
        "tmpfs" | "devfs" => "临时/系统卷".into(),
        other => {
            // 未知类型原样大写首字母返回，便于排查
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => "未知".into(),
            }
        }
    }
}

// ---------------------------------------------------------------- 单元测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prettify_maps_known_names() {
        assert_eq!(prettify("apfs"), "APFS");
        assert_eq!(prettify("exfat"), "exFAT");
        assert_eq!(prettify("msdos"), "FAT32");
        assert_eq!(prettify("NTFS"), "NTFS");
        assert_eq!(prettify("ufsd_NTFS"), "NTFS");
    }

    #[test]
    fn probe_current_dir_works() {
        let p = std::env::current_dir().unwrap();
        let fs = fs_type_of(&p);
        assert!(!fs.is_empty());
        println!("current dir fs = {fs}");
    }
}
