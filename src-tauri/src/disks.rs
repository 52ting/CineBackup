//! 磁盘 / 卷枚举 —— 「自动拉取硬盘」的后端实现
//!
//! - **Windows**：`GetLogicalDriveStringsW` 列出全部盘符 → `GetDriveTypeW` 判介质类型 →
//!   `GetVolumeInformationW` 取卷标 / 文件系统 / 只读标志 → `GetDiskFreeSpaceExW` 取容量。
//! - **macOS**：根卷 + `/Volumes/*` 下每个挂载点，用 `statfs(2)` 取文件系统名、容量与只读标志。
//!
//! 全部是只读的查询调用，不挂载、不弹出、不写入任何卷。
//! 未插入介质的光驱 / 读卡器会被自动跳过（`GetVolumeInformationW` 直接失败）。

use std::path::Path;

/// 一个可用的磁盘 / 卷
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskInfo {
    /// 稳定标识：Windows 为 `C:`，macOS 为规范化挂载点
    pub id: String,
    /// 可直接当路径用的挂载点：`C:\` / `/Volumes/素材盘`
    pub mount: String,
    /// 卷标（可能为空，由前端兜底显示）
    pub label: String,
    /// 文件系统：NTFS / exFAT / FAT32 / APFS / HFS+ …
    pub fs: String,
    /// 总容量（字节），未知为 0
    pub total: u64,
    /// 剩余可用（字节），未知为 0
    pub free: u64,
    /// 介质类型：fixed / removable / network / optical / ramdisk / volume / system / unknown
    pub kind: String,
    /// 是否可写（只读卷为 false）
    pub writable: bool,
}

impl DiskInfo {
    /// 卷标签兜底：空标签时用盘符 / 挂载点拼一个可读名字
    pub fn display_label(&self) -> String {
        if !self.label.trim().is_empty() {
            return self.label.clone();
        }
        match self.kind.as_str() {
            "system" => "系统盘".to_string(),
            "optical" => "光驱".to_string(),
            "network" => "网络盘".to_string(),
            _ => format!("本地磁盘 {}", self.id),
        }
    }
}

/// 对外唯一入口：列出本机当前所有可访问的磁盘 / 卷
pub fn list_disks() -> Vec<DiskInfo> {
    let mut v = platform::list();
    // 排序：固定盘在前，其次可移动，最后网络 / 光驱；同类按挂载点字母序
    v.sort_by(|a, b| {
        rank(&a.kind)
            .cmp(&rank(&b.kind))
            .then_with(|| a.id.to_lowercase().cmp(&b.id.to_lowercase()))
    });
    v
}

fn rank(kind: &str) -> u8 {
    match kind {
        "fixed" => 0,
        "volume" => 1,
        "removable" => 2,
        "system" => 3,
        "ramdisk" => 4,
        "network" => 5,
        "optical" => 6,
        _ => 7,
    }
}

/// 路径是否落在这个卷上（用于前端高亮「源 / 目标所在的盘」）
pub fn volume_of(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    if cfg!(windows) {
        let b = s.as_bytes();
        if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            return Some(format!("{}:", (b[0] as char).to_ascii_uppercase()));
        }
        return None;
    }
    let disks = platform::list();
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut best: Option<(usize, String)> = None;
    for d in disks {
        let mp = std::path::PathBuf::from(&d.mount);
        let mp = std::fs::canonicalize(&mp).unwrap_or(mp);
        if target.starts_with(&mp) {
            let len = mp.as_os_str().len();
            if best.as_ref().map(|(l, _)| len > *l).unwrap_or(true) {
                best = Some((len, d.id));
            }
        }
    }
    best.map(|(_, id)| id)
}

// ================================================================ Windows

#[cfg(windows)]
mod platform {
    use super::DiskInfo;

    // Win32 SDK 定义（windows-sys 未把 DRIVE_* 常量放进 FileSystem 模块，直接写数值更稳）
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;
    /// FILE_READ_ONLY_VOLUME —— 卷本身被标记为只读
    const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

    pub fn list() -> Vec<DiskInfo> {
        use windows_sys::Win32::Storage::FileSystem::{
            GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
        };

        // 一次拿到 "C:\\\0D:\\\0E:\\\0\0"
        let mut buf = [0u16; 1024];
        let n = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) };
        if n == 0 {
            return Vec::new();
        }

        // 按 NUL 切分成盘符列表
        let mut roots: Vec<String> = Vec::new();
        let mut cur: Vec<u16> = Vec::new();
        for &c in buf.iter() {
            if c == 0 {
                if cur.is_empty() {
                    break;
                }
                roots.push(String::from_utf16_lossy(&cur));
                cur.clear();
            } else {
                cur.push(c);
            }
        }

        let mut out = Vec::new();
        for root in roots {
            let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();

            let drive_type = unsafe { GetDriveTypeW(wide.as_ptr()) };
            let kind = match drive_type {
                DRIVE_REMOVABLE => "removable",
                DRIVE_FIXED => "fixed",
                DRIVE_REMOTE => "network",
                DRIVE_CDROM => "optical",
                DRIVE_RAMDISK => "ramdisk",
                _ => "unknown",
            };

            // 卷标 + 文件系统 + 标志位
            let mut label = [0u16; 128];
            let mut fs_name = [0u16; 64];
            let mut serial: u32 = 0;
            let mut max_comp: u32 = 0;
            let mut flags: u32 = 0;
            let ok = unsafe {
                GetVolumeInformationW(
                    wide.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    &mut serial,
                    &mut max_comp,
                    &mut flags,
                    fs_name.as_mut_ptr(),
                    fs_name.len() as u32,
                )
            };
            // 失败通常意味着「没插介质」的光驱 / 读卡器，直接跳过
            if ok == 0 {
                continue;
            }

            let mut free_to_caller: u64 = 0;
            let mut total: u64 = 0;
            let mut total_free: u64 = 0;
            let sp = unsafe {
                GetDiskFreeSpaceExW(
                    wide.as_ptr(),
                    &mut free_to_caller,
                    &mut total,
                    &mut total_free,
                )
            };
            if sp == 0 {
                total = 0;
                total_free = 0;
            }

            let id = root.trim_end_matches('\\').to_string();
            let readonly = flags & FILE_READ_ONLY_VOLUME != 0;

            out.push(DiskInfo {
                id,
                mount: root.clone(),
                label: wide_to_string(&label),
                fs: crate::fsinfo::prettify(&wide_to_string(&fs_name)),
                total,
                free: total_free,
                kind: kind.to_string(),
                // 光驱 / 只读卷不允许作为目标
                writable: !readonly && kind != "optical",
            });
        }
        out
    }

    /// 定长 UTF-16 缓冲 → String（首个 NUL 截断）
    fn wide_to_string(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len]).trim().to_string()
    }
}

// ================================================================ macOS

#[cfg(target_os = "macos")]
mod platform {
    use super::DiskInfo;
    use std::collections::HashSet;
    use std::ffi::CString;
    use std::path::Path;

    /// MNT_RDONLY —— statfs 的 f_flags 位
    const MNT_RDONLY: u32 = 0x0000_0001;

    pub fn list() -> Vec<DiskInfo> {
        let mut out: Vec<DiskInfo> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        if let Some(d) = probe("/", "system") {
            seen.insert(d.id.clone());
            out.push(d);
        }

        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_dir() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                // 跳过 .DS_Store 之类的隐藏项 / 元数据挂载
                if name.starts_with('.') {
                    continue;
                }
                let Some(d) = probe(&p.to_string_lossy(), "volume") else {
                    continue;
                };
                if !seen.insert(d.id.clone()) {
                    continue;
                }
                out.push(d);
            }
        }
        out
    }

    /// 用 statfs 读一个挂载点的文件系统名 / 容量 / 只读标志
    fn probe(mount: &str, default_kind: &str) -> Option<DiskInfo> {
        let c = CString::new(mount).ok()?;
        unsafe {
            let mut st: libc::statfs = std::mem::zeroed();
            if libc::statfs(c.as_ptr(), &mut st) != 0 {
                return None;
            }

            let fs_raw = cstr16_to_string(&st.f_fstypename);
            let fs = crate::fsinfo::prettify(&fs_raw);
            let bsize = st.f_bsize as u64;
            let total = (st.f_blocks as u64).saturating_mul(bsize);
            let free = (st.f_bavail as u64).saturating_mul(bsize);
            let readonly = (st.f_flags as u32) & MNT_RDONLY != 0;

            // 网络 / 外置卷按文件系统名细分类型
            let kind = match fs_raw.as_str() {
                "smbfs" | "cifs" | "nfs" | "nfs4" | "webdav" | "afpfs" => "network",
                _ => default_kind,
            };

            // 规范化挂载点，作为稳定 id
            let canon = std::fs::canonicalize(Path::new(mount))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| mount.to_string());

            let label = if mount == "/" {
                String::new()
            } else {
                Path::new(mount)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };

            Some(DiskInfo {
                id: canon,
                mount: mount.to_string(),
                label,
                fs,
                total,
                free,
                kind: kind.to_string(),
                writable: !readonly && libc::access(c.as_ptr(), libc::W_OK) == 0,
            })
        }
    }

    fn cstr16_to_string(buf: &[std::os::raw::c_char]) -> String {
        let bytes: Vec<u8> = buf
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

// ================================================================ 其他平台

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::DiskInfo;

    /// 本工具不发布 Linux 版；如被移植，从 `/proc/mounts` 补实现即可
    pub fn list() -> Vec<DiskInfo> {
        Vec::new()
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::DiskInfo;

    pub fn list() -> Vec<DiskInfo> {
        Vec::new()
    }
}

// ================================================================ 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_is_non_empty_on_dev_machine() {
        let d = list_disks();
        println!("检测到 {} 个卷：", d.len());
        for x in &d {
            println!(
                "  [{}] {}  {}  剩余 {} / 共 {}  可写={}",
                x.kind,
                x.display_label(),
                x.fs,
                x.free,
                x.total,
                x.writable
            );
        }
        assert!(!d.is_empty(), "至少应该检测到一个卷");
        // 每个卷都必须是可用的绝对路径，且文件系统名不为空
        for x in &d {
            assert!(!x.mount.is_empty());
            assert!(!x.fs.is_empty(), "{} 的文件系统探测失败", x.mount);
        }
    }

    #[test]
    fn sorts_fixed_first() {
        let mut v = vec![
            DiskInfo {
                id: "Z:".into(),
                mount: "Z:\\".into(),
                label: String::new(),
                fs: "NTFS".into(),
                total: 0,
                free: 0,
                kind: "network".into(),
                writable: true,
            },
            DiskInfo {
                id: "C:".into(),
                mount: "C:\\".into(),
                label: String::new(),
                fs: "NTFS".into(),
                total: 0,
                free: 0,
                kind: "fixed".into(),
                writable: true,
            },
        ];
        v.sort_by(|a, b| rank(&a.kind).cmp(&rank(&b.kind)));
        assert_eq!(v[0].kind, "fixed");
    }

    #[test]
    fn label_fallback() {
        let d = DiskInfo {
            id: "E:".into(),
            mount: "E:\\".into(),
            label: String::new(),
            fs: "exFAT".into(),
            total: 0,
            free: 0,
            kind: "fixed".into(),
            writable: true,
        };
        assert_eq!(d.display_label(), "本地磁盘 E:");
    }

    #[test]
    fn volume_of_matches_root() {
        let cwd = std::env::current_dir().unwrap();
        let v = volume_of(&cwd);
        println!("current dir volume = {v:?}");
        assert!(v.is_some());
    }
}
