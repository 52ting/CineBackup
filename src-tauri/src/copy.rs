//! 原生分块文件拷贝 —— 不调用 rsync，纯 Rust 实现，内置断点续传
//!
//! ## 断点续传规则（与需求 3 的冲突弹窗配合）
//! 拷贝前对比源文件与目标同名文件：
//! 1. 目标不存在           → `CopyMode::Fresh`   完整拷贝
//! 2. 目标存在，大小不一致  → `CopyMode::Resume`  从目标文件末尾继续写入
//! 3. 目标存在，大小一致    → 由调用方先算内容哈希（默认 SHA-256）：
//!    - 相同 → 直接跳过（不进本模块）
//!    - 不同 → `CopyMode::Overwrite` 覆盖
//!
//! ## 内存安全
//! 固定 4 MiB 缓冲区循环读写；`sync_all()` 在每个文件写完后调用，
//! 保证硬盘被拔掉时已写部分真实落盘（下次可续传）。

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::util::CHUNK_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// 目标不存在 → 创建并完整写入
    Fresh,
    /// 目标已存在且更小 → 从目标末尾继续追加
    Resume,
    /// 目标已存在 → 截断后完整重写
    Overwrite,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CopyOutcome {
    /// 本次新写入的字节数
    pub written: u64,
    /// 续传起点（已存在的字节数，速度统计时不计入）
    pub base: u64,
    /// 本想续传但目标不可续（比源大 / 为 0 字节）而改为重写
    pub restarted: bool,
    /// 写完后目标文件的实际长度
    pub dst_len: u64,
}

fn cancelled_err() -> io::Error {
    io::Error::new(ErrorKind::Interrupted, "任务已取消")
}

/// 确保父目录存在（目标目录树按需创建）
pub fn ensure_parent(dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// 创建一个目录（dry run 时由调用方跳过）
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// 核心：把一个文件拷贝到目标路径。
///
/// - `on_bytes(delta) -> bool`：每写完一块回调；返回 `false` 表示请求中止。
/// - 全程检查 `cancel`，可保证「取消」在秒级内生效。
pub fn copy_file(
    src: &Path,
    dst: &Path,
    mode: CopyMode,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64) -> bool,
) -> io::Result<CopyOutcome> {
    let src_len = fs::metadata(src)?.len();
    ensure_parent(dst)?;

    let mut out = CopyOutcome::default();
    let mut dst_file: File;

    match mode {
        CopyMode::Fresh | CopyMode::Overwrite => {
            dst_file = File::create(dst)?; // 不存在则创建；存在则截断
        }
        CopyMode::Resume => {
            let dst_len = fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
            if dst_len == 0 || dst_len >= src_len {
                // 目标文件为空或比源还大 —— 无法续传，只能从头重写
                out.restarted = dst_len > 0;
                dst_file = File::create(dst)?;
            } else {
                out.base = dst_len;
                dst_file = OpenOptions::new().append(true).open(dst)?;
            }
        }
    }

    let mut src_file = File::open(src)?;
    if out.base > 0 {
        src_file.seek(SeekFrom::Start(out.base))?;
    }

    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut written: u64 = 0;

    let result: io::Result<()> = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err(cancelled_err());
        }
        let n = match src_file.read(&mut buf) {
            Ok(0) => break Ok(()), // 正常读到文件末尾
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => break Err(e),
        };
        if let Err(e) = dst_file.write_all(&buf[..n]) {
            break Err(e);
        }
        written += n as u64;
        if !on_bytes(n as u64) {
            break Err(cancelled_err());
        }
    };

    // 无论成功或中断，都把已写入的数据刷到磁盘 —— 这正是断点续传的基础
    let mut flush_res = dst_file.flush();
    if flush_res.is_ok() {
        flush_res = dst_file.sync_all();
    }
    drop(dst_file);

    if let Err(e) = result {
        // 错误路径下调用方拿不到 CopyOutcome，所以这里不回写 out。
        // 已写入的字节数由调用方的进度回调自行累计（见 engine.rs 的 file_written），
        // 磁盘上真实落了多少则以目标文件当前长度为准 —— 这正是下次续传的依据。
        //
        // 冲刷失败也不能掩盖真正的错误，故只丢弃 flush_res。
        let _ = flush_res;
        return Err(e);
    }
    flush_res?;

    out.written = written;
    out.dst_len = fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
    Ok(out)
}

/// 简易无进度回调的拷贝（Dry Run 之外的内部使用）
pub fn copy_file_simple(src: &Path, dst: &Path, mode: CopyMode, cancel: &AtomicBool) -> io::Result<CopyOutcome> {
    copy_file(src, dst, mode, cancel, &mut |_| true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join("cinebackup_copy_test");
        let _ = fs::create_dir_all(&d);
        d.join(name)
    }

    #[test]
    fn copy_and_resume_and_overwrite() {
        let src = tmp("src.bin");
        let dst = tmp("dst.bin");
        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);

        // 12 MiB 源文件
        let data: Vec<u8> = (0..12 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &data).unwrap();

        let cancel = AtomicBool::new(false);

        // 1) 完整拷贝
        let o = copy_file_simple(&src, &dst, CopyMode::Fresh, &cancel).unwrap();
        assert_eq!(o.written, data.len() as u64);
        assert_eq!(o.dst_len, data.len() as u64);

        // 2) 造一个「半截文件」模拟中断（截断到 5 MiB）
        let partial = &data[..5 * 1024 * 1024];
        fs::write(&dst, partial).unwrap();

        // 3) 续传
        let o = copy_file_simple(&src, &dst, CopyMode::Resume, &cancel).unwrap();
        assert_eq!(o.base, (5 * 1024 * 1024) as u64);
        assert_eq!(o.written, (7 * 1024 * 1024) as u64);
        assert_eq!(fs::read(&dst).unwrap(), data);

        // 4) 覆盖
        fs::write(&dst, b"garbage").unwrap();
        copy_file_simple(&src, &dst, CopyMode::Overwrite, &cancel).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), data);

        // 5) 目标比源大 → 续传退化为重写
        fs::write(&dst, vec![0u8; data.len() + 999]).unwrap();
        let o = copy_file_simple(&src, &dst, CopyMode::Resume, &cancel).unwrap();
        assert!(o.restarted);
        assert_eq!(fs::read(&dst).unwrap(), data);

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }
}
