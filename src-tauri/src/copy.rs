//! 文件拷贝 —— 数据面**全部**委托给 `posix` 模块。
//!
//! # 为什么这个文件现在这么薄
//!
//! 早期这里是「自己写一套 read/write 循环」，`hash.rs` 里另有一套。
//! 两套循环语义稍有出入（尤其「末块写多少」和「续传起点怎么定」），
//! 就会直接表现为**目标文件长度正确、SHA-256 却不一致** —— 这是最难查的一类 bug。
//!
//! 现在拷贝与哈希**共用** `posix::DataFile` 的同一个读循环，从结构上排除了分叉。
//! 本文件只负责：目录准备、把 `CopyMode` 翻译成 `posix::WriteMode`。
//!
//! ## 断点续传的安全前提
//!
//! 「目标已存在且大小不同 → 续传」这条规则**必须**先确认目标里那段内容
//! 真的是源的前缀。否则只要目标里是一份**内容不同但更短**的文件，
//! append 出去就会得到一个「长度恰好等于源、内容却是拼出来的」文件：
//! `stat` 大小一致，SHA-256 必然不一致。
//!
//! 所以 `posix::copy_data_fork` 里：`prefix_check = true` 时**逐块比对前缀**，
//! 对不上就从头重写；`prefix_check = false`（用户关掉了校验）时**故意不盲续传**，
//! 直接退化为从头重写。备份工具里「多花时间重写一遍」永远优于「静默写坏」。

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::posix::{self, CopyPolicy, WriteMode};

/// 拷贝模式（由调用方根据预扫描结论决定）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// 目标不存在：从头写入
    Fresh,
    /// 目标存在、大小不一致：断点续传（从目标末尾继续写）
    ///
    /// ⚠️ 只是「打算续传」；是否真的能续，由 `posix` 校验前缀后决定，
    /// 不可续时会自动改为从头重写（`CopyOutcome::restarted` 会为 true）。
    Resume,
    /// 目标存在且需替换：从头写入（截断）
    Overwrite,
}

/// 一次拷贝的结果
#[derive(Debug, Clone, Default)]
pub struct CopyOutcome {
    /// 续传起点（本次从目标第几字节开始写）；未续传为 0
    pub base: u64,
    /// 本次实际写入的字节数
    pub written: u64,
    /// 写完后目标的 Data Fork 长度
    pub dst_len: u64,
    /// 本想续传但实际改为从头重写了（前缀不一致 / 校验被关 / 目标不可续）
    pub restarted: bool,
    /// 本次是否真的做过续传前缀校验
    pub prefix_checked: bool,
}

impl From<posix::CopyStat> for CopyOutcome {
    fn from(s: posix::CopyStat) -> Self {
        Self {
            base: s.base,
            written: s.written,
            dst_len: s.dst_len,
            restarted: s.restarted,
            prefix_checked: s.prefix_checked,
        }
    }
}

/// 建好目标文件的父目录（不存在则递归创建）
pub fn ensure_parent(dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        ensure_dir(parent)?;
    }
    Ok(())
}

/// 建目录（已存在则什么都不做）
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// 把一个文件的 **Data Fork** 拷到目标（`base` 之后的部分）。
///
/// - `on_bytes(delta) -> bool`：每写完一块回调；返回 `false` 表示请求中止。
/// - 全过程检查 `cancel`，「取消」在块级（≤4 MiB）生效。
/// - 只走 `posix::copy_data_fork`：精确 n 字节写入、不补零、不 ftruncate、必 fsync。
pub fn copy_file(
    src: &Path,
    dst: &Path,
    mode: CopyMode,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64) -> bool,
) -> io::Result<CopyOutcome> {
    // 默认开启续传前缀校验（安全优先）。要按用户选项走，用 `copy_file_with`。
    copy_file_with(src, dst, mode, 0, true, cancel, on_bytes)
}

/// 带完整策略的拷贝（`engine` 用这个，把「续传前校验」选项透传进来）。
///
/// `base_hint`：调用方看到的目标长度（快照，可能过期）；实测不一致时会从头重写。
/// `prefix_check`：是否逐块比对「目标已有内容 == 源前缀」，关闭时退化为从头重写。
pub fn copy_file_with(
    src: &Path,
    dst: &Path,
    mode: CopyMode,
    base_hint: u64,
    prefix_check: bool,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64) -> bool,
) -> io::Result<CopyOutcome> {
    let wmode = match mode {
        CopyMode::Fresh | CopyMode::Overwrite => WriteMode::Truncate,
        CopyMode::Resume => WriteMode::Append,
    };
    // 先保证父目录存在。
    // 旧实现里有这一步，重写时漏掉会导致「新目录树还没建好就写文件」直接 ENOENT；
    // 上层虽然会预先建 plan.dirs，但拷贝模块自己保证这一点更稳（也少一类偶发失败）。
    ensure_parent(dst)?;

    let st = posix::copy_data_fork(
        src,
        dst,
        wmode,
        base_hint,
        CopyPolicy { prefix_check },
        cancel,
        on_bytes,
    )?;
    Ok(st.into())
}

/// 无进度回调的简易拷贝（测试 / 内部工具用）
pub fn copy_file_simple(
    src: &Path,
    dst: &Path,
    mode: CopyMode,
    cancel: &AtomicBool,
) -> io::Result<CopyOutcome> {
    copy_file(src, dst, mode, cancel, &mut |_| true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::posix;
    use std::sync::atomic::AtomicBool;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join("cinebackup_copy_test");
        let _ = fs::create_dir_all(&d);
        d.join(name)
    }

    fn data(len: usize, seed: u64) -> Vec<u8> {
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

    #[test]
    fn copy_and_resume_and_overwrite() {
        let src = tmp("src.bin");
        let dst = tmp("dst.bin");
        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);

        // 12 MiB（3 个 4 MiB 块整）—— 另外补一条非整块长度的用例见 t_last_block
        let d: Vec<u8> = data(12 * 1024 * 1024, 1);
        fs::write(&src, &d).unwrap();

        let cancel = AtomicBool::new(false);

        // 1) 完整拷贝
        let o = copy_file_simple(&src, &dst, CopyMode::Fresh, &cancel).unwrap();
        assert_eq!(o.written, d.len() as u64);
        assert_eq!(o.dst_len, d.len() as u64);

        // 2) 半截文件（真的前半段）→ 续传
        let half = &d[..5 * 1024 * 1024];
        fs::write(&dst, half).unwrap();
        let o = copy_file_simple(&src, &dst, CopyMode::Resume, &cancel).unwrap();
        assert!(o.prefix_checked, "续传必须校验过前缀");
        assert!(!o.restarted);
        assert_eq!(o.base, (5 * 1024 * 1024) as u64);
        assert_eq!(o.written, (7 * 1024 * 1024) as u64);
        assert_eq!(fs::read(&dst).unwrap(), d);

        // 3) 覆盖
        fs::write(&dst, b"garbage").unwrap();
        copy_file_simple(&src, &dst, CopyMode::Overwrite, &cancel).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), d);

        // 4) 目标比源大 → 续传退化为重写
        fs::write(&dst, vec![0u8; d.len() + 999]).unwrap();
        let o = copy_file_simple(&src, &dst, CopyMode::Resume, &cancel).unwrap();
        assert!(o.restarted);
        assert_eq!(fs::read(&dst).unwrap(), d);

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }

    /// 末块不是整块大小时，长度与内容都必须精确（回归「写满 buffer」的坑）
    #[test]
    fn t_last_block_exact() {
        let src = tmp("odd_src.bin");
        let dst = tmp("odd_dst.bin");
        let d = data(4 * 1024 * 1024 + 12345, 9);
        fs::write(&src, &d).unwrap();
        let cancel = AtomicBool::new(false);

        let o = copy_file_simple(&src, &dst, CopyMode::Fresh, &cancel).unwrap();
        assert_eq!(o.dst_len, d.len() as u64, "长度必须精确到字节");
        assert_eq!(fs::read(&dst).unwrap(), d, "内容必须逐字节一致");

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }

    /// ⚠️ 核心回归：目标里是「内容不同的更短文件」时，不得拼出「长度对、内容错」。
    #[test]
    fn t_stale_shorter_target_is_not_spliced() {
        let src = tmp("stale_src.bin");
        let dst = tmp("stale_dst.bin");
        let d = data(3 * 1024 * 1024, 77);
        fs::write(&src, &d).unwrap();
        fs::write(&dst, data(1024 * 1024, 123)).unwrap(); // 更短、内容完全不同

        let cancel = AtomicBool::new(false);
        let o = copy_file_simple(&src, &dst, CopyMode::Resume, &cancel).unwrap();

        assert!(o.restarted, "前缀不一致必须改为从头重写");
        assert_eq!(fs::read(&dst).unwrap(), d);

        // 哈希必须一致 —— 这正是用户报的「长度一样、SHA256 不一致」那条
        let (a, _) = posix::hash_data_fork(&src, crate::hash::HashAlgo::Sha256, &cancel, &mut |_| {})
            .unwrap();
        let (b, _) = posix::hash_data_fork(&dst, crate::hash::HashAlgo::Sha256, &cancel, &mut |_| {})
            .unwrap();
        assert_eq!(a, b);

        let _ = fs::remove_file(&src);
        let _ = fs::remove_file(&dst);
    }
}
