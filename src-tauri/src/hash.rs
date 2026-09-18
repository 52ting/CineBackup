//! 分块流式 xxHash64 计算
//!
//! 设计要点（对应需求「核心！」）：
//! 1. **绝不整文件读入内存**：固定 4 MiB 缓冲区循环读取，几十 GB 的
//!    ProRes / MXF / DCP 单文件内存占用恒定。
//! 2. **与文件系统无关**：只读原始二进制字节流。APFS / NTFS / exFAT /
//!    FAT32 上的同一份数据得到同一个哈希。
//! 3. 全过程可取消，并可上报已读字节数（供「校验阶段速度 + 剩余时间」使用）。

use std::fs::File;
use std::hash::Hasher;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use xxhash_rust::xxh64::Xxh64;

use crate::util::CHUNK_SIZE;

/// xxHash64 种子（固定为 0，源 / 目标必须一致才有可比性）
pub const SEED: u64 = 0;

/// 哈希结果 → 16 位小写十六进制（便于日志/UI 展示）
pub fn hash_hex(h: u64) -> String {
    format!("{h:016x}")
}

fn cancelled_err() -> io::Error {
    io::Error::new(ErrorKind::Interrupted, "任务已取消")
}

/// 计算整个文件的 xxHash64。
///
/// 返回 `(hash, 读取字节数)`。
/// `on_bytes(delta)` 每读完一块回调一次，用于进度 / 速度统计。
pub fn hash_file(
    path: &Path,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> io::Result<(u64, u64)> {
    hash_range(path, 0, None, cancel, &mut on_bytes)
}

/// 只计算文件**前 `len` 字节**的 xxHash64（用于断点续传前的「已写入部分」校验）
pub fn hash_prefix(
    path: &Path,
    len: u64,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> io::Result<(u64, u64)> {
    hash_range(path, 0, Some(len), cancel, &mut on_bytes)
}

/// 通用：从 `start` 偏移开始，最多读 `len` 字节（None = 到文件末尾）计算 xxHash64
pub fn hash_range(
    path: &Path,
    start: u64,
    len: Option<u64>,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> io::Result<(u64, u64)> {
    let mut file = File::open(path)?;
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }

    let mut hasher = Xxh64::new(SEED);
    let mut buf = vec![0u8; CHUNK_SIZE]; // 4 MiB 常驻，恒定内存
    let mut remaining = len.unwrap_or(u64::MAX);
    let mut total: u64 = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled_err());
        }
        let want = remaining.min(buf.len() as u64) as usize;
        if want == 0 {
            break;
        }
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            break; // EOF
        }
        hasher.write(&buf[..n]);
        total += n as u64;
        remaining = remaining.saturating_sub(n as u64);
        on_bytes(n as u64);
    }

    Ok((hasher.finish(), total))
}

/// 判断两个文件内容是否一致（先比大小短路，再比 xxHash64）
///
/// 用于预扫描：大小不同 → 直接 false，**不做无谓的哈希**。
pub fn files_identical(
    a: &Path,
    b: &Path,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> io::Result<bool> {
    let ma = std::fs::metadata(a)?;
    let mb = match std::fs::metadata(b) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    if ma.len() != mb.len() {
        return Ok(false);
    }
    // 空文件：xxHash64 空输入恒等，直接判定一致
    if ma.len() == 0 {
        return Ok(true);
    }
    let (ha, _) = hash_file(a, cancel, &mut on_bytes)?;
    let (hb, _) = hash_file(b, cancel, &mut on_bytes)?;
    Ok(ha == hb)
}

/// 断点续传前的安全检查：
/// 校验「源文件前 dst_len 字节」与「目标文件已有内容」的 xxHash64 是否一致。
///
/// 一致 → 可以安全地从目标末尾继续追加；
/// 不一致 → 说明上次写入被中断在半块上（或目标文件被改过），必须从头覆盖重写。
pub fn resume_prefix_ok(
    src: &Path,
    dst: &Path,
    dst_len: u64,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> io::Result<bool> {
    if dst_len == 0 {
        return Ok(true);
    }
    let (h_src, _) = hash_prefix(src, dst_len, cancel, &mut on_bytes)?;
    let (h_dst, _) = hash_file(dst, cancel, &mut on_bytes)?;
    Ok(h_src == h_dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hash_matches_across_chunked_read() {
        let dir = std::env::temp_dir().join("cinebackup_hash_test");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("a.bin");
        let mut fh = File::create(&f).unwrap();
        // 10 MiB：跨 3 个 4 MiB 块
        let data = vec![0xABu8; 10 * 1024 * 1024];
        fh.write_all(&data).unwrap();
        drop(fh);

        let cancel = AtomicBool::new(false);
        let (h1, n1) = hash_file(&f, &cancel, |_| {}).unwrap();
        let (h2, _) = hash_file(&f, &cancel, |_| {}).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(n1, data.len() as u64);
        // 与一次性计算的官方实现一致
        assert_eq!(h1, xxhash_rust::xxh64::xxh64(&data, SEED));

        let (hp, np) = hash_prefix(&f, 1024, &cancel, |_| {}).unwrap();
        assert_eq!(np, 1024);
        assert_eq!(hp, xxhash_rust::xxh64::xxh64(&data[..1024], SEED));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
