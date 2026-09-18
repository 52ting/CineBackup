//! 分块流式内容哈希 —— 默认 **SHA-256**（可选 xxHash64 快速模式）
//!
//! 设计要点（对应需求「核心！」）：
//! 1. **绝不整文件读入内存**：固定 4 MiB 缓冲区循环读取，几十 GB 的
//!    ProRes / MXF / DCP 单文件内存占用恒定。
//! 2. **与文件系统无关**：只读原始二进制字节流。APFS / NTFS / exFAT /
//!    FAT32 上的同一份数据得到同一个哈希。
//! 3. 全过程可取消，并可上报已读字节数（供「校验阶段速度 + 剩余时间」使用）。
//!
//! ## 两种算法
//!
//! | 算法 | 摘要长度 | 速度（现代 CPU） | 用途 |
//! |---|---|---|---|
//! | **SHA-256**（默认） | 64 位十六进制 | 约 1~2 GB/s（x86 SHA-NI / ARMv8 加密扩展） | 结果可对外核对，与 `shasum -a 256` 一致 |
//! | xxHash64 | 16 位十六进制 | 约 5~10 GB/s | 追求吞吐、不要求密码学强度时用 |
//!
//! 同一次任务里**所有**比对（预扫描查重、续传前缀、最终校验）都用同一种算法，
//! 避免出现「两个地方用不同算法」的解释负担。

use std::fs::File;
use std::hash::Hasher;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use xxhash_rust::xxh64::Xxh64;

use crate::util::CHUNK_SIZE;

/// xxHash64 种子（固定为 0，源 / 目标必须一致才有可比性）
pub const SEED: u64 = 0;

// ================================================================ 算法

/// 校验算法
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgo {
    /// 默认：SHA-256，64 位十六进制，可与系统 `shasum -a 256` / `certutil -hashfile` 对齐
    #[default]
    Sha256,
    /// 非加密快速哈希，吞吐高但摘要只有 64 位
    Xxh64,
}

impl HashAlgo {
    /// 界面 / 日志里显示的名字
    pub fn label(self) -> &'static str {
        match self {
            HashAlgo::Sha256 => "SHA-256",
            HashAlgo::Xxh64 => "xxHash64",
        }
    }
    /// 十六进制摘要长度
    pub fn hex_len(self) -> usize {
        match self {
            HashAlgo::Sha256 => 64,
            HashAlgo::Xxh64 => 16,
        }
    }
    /// 任务文件 / 命令行里用的短标识
    pub fn slug(self) -> &'static str {
        match self {
            HashAlgo::Sha256 => "sha256",
            HashAlgo::Xxh64 => "xxh64",
        }
    }
    /// 宽松解析（兼容 "sha256" / "SHA-256" / "sha_256" 等写法）
    pub fn parse(s: &str) -> Option<Self> {
        let t: String = s
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        match t.as_str() {
            "sha256" => Some(HashAlgo::Sha256),
            "xxh64" | "xxhash64" => Some(HashAlgo::Xxh64),
            _ => None,
        }
    }
}

// ================================================================ 摘要

/// 一次哈希的结果（自带算法信息，避免长度不同的摘要被混着比较）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    algo: HashAlgo,
    hex: String,
}

impl Digest {
    pub fn algo(&self) -> HashAlgo {
        self.algo
    }
    /// 完整十六进制摘要（SHA-256 = 64 字符，xxHash64 = 16 字符）
    pub fn hex(&self) -> &str {
        &self.hex
    }
    /// 前 `n` 个字符，日志里只展示前缀即可
    pub fn short(&self, n: usize) -> &str {
        &self.hex[..n.min(self.hex.len())]
    }
}

/// 增量哈希器：把两种算法的差异关在这里
enum Inner {
    Sha(Sha256),
    Xxh(Xxh64),
}

impl Inner {
    fn new(algo: HashAlgo) -> Self {
        match algo {
            HashAlgo::Sha256 => Inner::Sha(Sha256::new()),
            HashAlgo::Xxh64 => Inner::Xxh(Xxh64::new(SEED)),
        }
    }
    fn write(&mut self, bytes: &[u8]) {
        match self {
            Inner::Sha(h) => h.update(bytes),
            Inner::Xxh(h) => h.write(bytes),
        }
    }
    fn finish(self, algo: HashAlgo) -> Digest {
        let hex = match self {
            Inner::Sha(h) => to_hex(&h.finalize()),
            Inner::Xxh(h) => format!("{:016x}", h.finish()),
        };
        Digest { algo, hex }
    }
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX_DIGITS[(b >> 4) as usize] as char);
        s.push(HEX_DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

fn cancelled_err() -> io::Error {
    io::Error::new(ErrorKind::Interrupted, "任务已取消")
}

// ================================================================ 流式计算

/// 计算整个文件的哈希。返回 `(摘要, 读取字节数)`。
///
/// `on_bytes(delta)` 每读完一块回调一次，用于进度 / 速度统计。
pub fn hash_file(
    path: &Path,
    algo: HashAlgo,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> io::Result<(Digest, u64)> {
    hash_range(path, 0, None, algo, cancel, &mut on_bytes)
}

/// 只计算文件**前 `len` 字节**的哈希（用于断点续传前的「已写入部分」校验）
pub fn hash_prefix(
    path: &Path,
    len: u64,
    algo: HashAlgo,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> io::Result<(Digest, u64)> {
    hash_range(path, 0, Some(len), algo, cancel, &mut on_bytes)
}

/// 通用：从 `start` 偏移开始，最多读 `len` 字节（None = 到文件末尾）计算哈希
pub fn hash_range(
    path: &Path,
    start: u64,
    len: Option<u64>,
    algo: HashAlgo,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> io::Result<(Digest, u64)> {
    let mut file = File::open(path)?;
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }

    let mut hasher = Inner::new(algo);
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

    Ok((hasher.finish(algo), total))
}

/// 判断两个文件内容是否一致（先比大小短路，再比哈希）
///
/// 用于预扫描：大小不同 → 直接 false，**不做无谓的哈希**。
pub fn files_identical(
    a: &Path,
    b: &Path,
    algo: HashAlgo,
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
    // 空文件：任何算法的空输入都恒等，直接判定一致
    if ma.len() == 0 {
        return Ok(true);
    }
    let (ha, _) = hash_file(a, algo, cancel, &mut on_bytes)?;
    let (hb, _) = hash_file(b, algo, cancel, &mut on_bytes)?;
    Ok(ha == hb)
}

/// 断点续传前的安全检查：
/// 校验「源文件前 dst_len 字节」与「目标文件已有内容」的哈希是否一致。
///
/// 一致 → 可以安全地从目标末尾继续追加；
/// 不一致 → 说明上次写入被中断在半块上（或目标文件被改过），必须从头覆盖重写。
pub fn resume_prefix_ok(
    src: &Path,
    dst: &Path,
    dst_len: u64,
    algo: HashAlgo,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> io::Result<bool> {
    if dst_len == 0 {
        return Ok(true);
    }
    let (h_src, _) = hash_prefix(src, dst_len, algo, cancel, &mut on_bytes)?;
    let (h_dst, _) = hash_file(dst, algo, cancel, &mut on_bytes)?;
    Ok(h_src == h_dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 不走文件系统的小工具：直接对内存字节算摘要
    fn digest_of(algo: HashAlgo, data: &[u8]) -> Digest {
        let mut h = Inner::new(algo);
        h.write(data);
        h.finish(algo)
    }

    /// SHA-256 官方向量（FIPS 180-4 / NIST 示例）
    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            digest_of(HashAlgo::Sha256, b"").hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest_of(HashAlgo::Sha256, b"abc").hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest_of(HashAlgo::Sha256, b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq").hex(),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 摘要是 64 位十六进制，不是 xxHash64 的 16 位
        assert_eq!(HashAlgo::Sha256.hex_len(), 64);
        assert_eq!(digest_of(HashAlgo::Sha256, b"abc").hex().len(), 64);
    }

    #[test]
    fn xxh64_matches_official_impl() {
        let data = b"cinebackup";
        assert_eq!(
            digest_of(HashAlgo::Xxh64, data).hex(),
            format!("{:016x}", xxhash_rust::xxh64::xxh64(data, SEED))
        );
        assert_eq!(digest_of(HashAlgo::Xxh64, b"").hex(), "ef46db3751d8e999");
    }

    #[test]
    fn algo_parsing_is_lenient() {
        assert_eq!(HashAlgo::parse("SHA-256"), Some(HashAlgo::Sha256));
        assert_eq!(HashAlgo::parse(" sha256 "), Some(HashAlgo::Sha256));
        assert_eq!(HashAlgo::parse("xxHash64"), Some(HashAlgo::Xxh64));
        assert_eq!(HashAlgo::parse("md5"), None);
        // serde 的短标识与 parse 能互相认
        assert_eq!(HashAlgo::parse(HashAlgo::Sha256.slug()), Some(HashAlgo::Sha256));
        assert_eq!(HashAlgo::parse(HashAlgo::Xxh64.slug()), Some(HashAlgo::Xxh64));
    }

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

        for algo in [HashAlgo::Sha256, HashAlgo::Xxh64] {
            let (h1, n1) = hash_file(&f, algo, &cancel, |_| {}).unwrap();
            let (h2, _) = hash_file(&f, algo, &cancel, |_| {}).unwrap();
            assert_eq!(h1, h2, "{} 两次结果应一致", algo.label());
            assert_eq!(n1, data.len() as u64);
            assert_eq!(h1.algo(), algo);
            assert_eq!(h1.hex().len(), algo.hex_len());
            // 与一次性计算的官方实现一致
            assert_eq!(h1, digest_of(algo, &data), "{} 分块与整块结果应一致", algo.label());

            // 前缀哈希 == 前 1024 字节的摘要
            let (hp, np) = hash_prefix(&f, 1024, algo, &cancel, |_| {}).unwrap();
            assert_eq!(np, 1024);
            assert_eq!(hp, digest_of(algo, &data[..1024]));
        }

        // 两种算法对同一数据必须给出不同摘要（防止实现被串了）
        assert_ne!(
            digest_of(HashAlgo::Sha256, &data).hex().len(),
            digest_of(HashAlgo::Xxh64, &data).hex().len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identical_and_resume_prefix() {
        let dir = std::env::temp_dir().join("cinebackup_hash_test2");
        let _ = std::fs::create_dir_all(&dir);
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        let cancel = AtomicBool::new(false);

        for algo in [HashAlgo::Sha256, HashAlgo::Xxh64] {
            // 每轮都从干净状态开始，免得上一轮的改动影响下一轮
            std::fs::write(&a, b"0123456789abcdef").unwrap();
            std::fs::write(&b, b"0123456789abcdef").unwrap();
            assert!(files_identical(&a, &b, algo, &cancel, |_| {}).unwrap());

            // 目标更短（半截文件）→ 只比对已有部分，仍然可以续传
            std::fs::write(&b, b"012345").unwrap();
            assert!(resume_prefix_ok(&a, &b, 6, algo, &cancel, |_| {}).unwrap());

            // 中间字节被改过 → 必须从头重写
            std::fs::write(&b, b"0123456X").unwrap();
            assert!(!resume_prefix_ok(&a, &b, 8, algo, &cancel, |_| {}).unwrap());

            // 大小相同但内容不同 → 不能算作「已备份」
            std::fs::write(&b, b"0123456789abcdeF").unwrap();
            assert!(!files_identical(&a, &b, algo, &cancel, |_| {}).unwrap());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
