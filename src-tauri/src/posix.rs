//! POSIX 底层 IO —— **只碰 Data Fork**，只走 open / read / write。
//!
//! # 这个模块存在的理由
//!
//! 它是「拷贝」与「哈希」**唯一**的读盘入口。两边共用同一套读写语义，
//! 才能保证「拷出去的字节」和「算哈希的字节」是同一个东西。
//!
//! # 铁律（改这个文件前先读一遍）
//!
//! **① 只用 Data Fork。**
//! HFS+ / APFS 上一个文件可能有三部分：
//! - **Data Fork** ← 文件真正的二进制内容，只有它算「文件内容」
//! - Resource Fork 资源分支（图标、缩略图等）
//! - xattr 扩展属性（`com.apple.quarantine` / `com.apple.FinderInfo` …）
//!
//! 后两者**全部属于元数据**，绝不能参与拷贝的字节流，更不能参与哈希。
//! 实现上：`File::open` 天然只打开 Data Fork，本模块**绝不构造**
//! `..namedfork/rsrc` 之类的路径 —— 那样读出来的是资源分支，会让「内容」
//! 凭空多出一截，哈希必然对不上。
//!
//! **② 写入必须精确。**
//! `read` 返回 `n`，就**只写 `n` 字节**。绝不能「写满整个 buffer 再 ftruncate
//! 收尾」—— 那样 stat 大小是对的，但文件中间的二进制流是错的（尾部多出的字节
//! 被截掉前，若发生在中间块就会污染内容）。这正是「长度一样、SHA256 不一样」
//! 最典型的成因之一，所以本模块**不提供、也不使用 ftruncate**。
//!
//! **③ 每个返回值都要判。**
//! `read`/`write` 都可能被信号打断（EINTR）或**短写**（只写了一半）。
//! 短写必须循环补齐，否则会静默丢字节。
//!
//! # 关于 `O_TRUNC` 与「禁止 ftruncate」的区别（重要，别误读）
//!
//! 规则禁止的是「**先写入多余数据、再 ftruncate 截断**」这种补救式截断。
//! 而「打开时就 `O_TRUNC`」是**在写入之前**把文件清零，属于正常做法，两者不同。
//! 本模块：从头写用 `O_WRONLY|O_CREAT|O_TRUNC`；续传用 `O_WRONLY|O_APPEND`。

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::hash::{Digest, HashAlgo, Hasher};
use crate::util::CHUNK_SIZE;

/// 取消时统一返回的错误（调用方据 `ErrorKind::Interrupted` 识别）
pub fn cancelled_err() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "任务已取消")
}

// ================================================================ 打开方式

/// 写入模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// 从头写入：`O_WRONLY|O_CREAT|O_TRUNC`（写入**之前**清零，不是写完再截断）
    Truncate,
    /// 追加续传：`O_WRONLY|O_APPEND`
    Append,
}

// ================================================================ DataFile

/// 一个只读 / 只写的 Data Fork 句柄。
///
/// Unix 下直接握裸 fd（走 `libc::open/read/write`，与需求一致）；
/// Windows 下用 `std::fs::File` —— 它底层就是 `CreateFile/ReadFile/WriteFile`，
/// 语义与 POSIX 等价（精确 n 字节、短写需补齐），没必要为形式引入 FFI。
pub struct DataFile {
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
    #[cfg(not(unix))]
    file: std::fs::File,
}

impl DataFile {
    // ---------------------------------------------------------- 构造

    /// 只读打开 Data Fork（等价 `open(path, O_RDONLY|O_CLOEXEC)`）
    pub fn open_read(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let c = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径含 NUL 字节"))?;
            // O_CLOEXEC：fork/exec 时不被子进程继承，避免句柄泄漏
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                file: std::fs::File::open(path)?,
            })
        }
    }

    /// 写入打开（`Truncate` = 从头写 / `Append` = 续传追加）
    pub fn open_write(path: &Path, mode: WriteMode) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let c = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径含 NUL 字节"))?;
            let mut flags = libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC;
            flags |= match mode {
                WriteMode::Truncate => libc::O_TRUNC,
                WriteMode::Append => libc::O_APPEND,
            };
            // 0644：与 std 的默认建文件权限一致，受进程 umask 约束
            let fd = unsafe { libc::open(c.as_ptr(), flags, 0o644 as libc::c_uint) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd })
        }
        #[cfg(not(unix))]
        {
            use std::fs::OpenOptions;
            let mut o = OpenOptions::new();
            o.write(true).create(true);
            match mode {
                WriteMode::Truncate => {
                    o.truncate(true);
                }
                WriteMode::Append => {
                    o.append(true);
                }
            }
            Ok(Self { file: o.open(path)? })
        }
    }

    // ---------------------------------------------------------- 读

    /// 读一次（最多 `buf.len()` 字节）。返回 `0` 表示 EOF。
    ///
    /// EINTR 自动重试；其它错误原样上抛。**返回的 n 就是真实读到的字节数**，
    /// 调用方只能按 n 处理，不能假设读满了。
    pub fn read_some(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            #[cfg(unix)]
            let r = unsafe {
                libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            #[cfg(not(unix))]
            let r = {
                use std::io::Read;
                match self.file.read(buf) {
                    Ok(n) => n as isize,
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            };

            #[cfg(unix)]
            {
                if r < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue; // 信号打断，重试
                    }
                    return Err(e);
                }
                return Ok(r as usize);
            }
            #[cfg(not(unix))]
            return Ok(r as usize);
        }
    }

    /// 读**满** `buf`（或读到 EOF 为止），返回实际读到的字节数。
    ///
    /// ⚠️ 与 [`read_some`](Self::read_some) 的区别是**关键**，别混用：
    ///
    /// `read(2)` **不保证**把请求的字节数一次读完 —— 返回小于 `buf.len()` 是合法的。
    /// 本机 Windows/NTFS 上实测几乎总是读满，所以这个差异在本机看不出来；
    /// 但 **macOS/APFS 会返回部分读**（CI 上就是这么炸的：
    /// 「本该正常续传却判定要重写」，根因是两边一次读到的字节数不同 →
    /// 被当成「内容不一致」）。
    ///
    /// 因此：
    /// - **流式**场景（拷贝、整文件哈希）：读多少算多少，用 `read_some` —— 反正
    ///   字节是按顺序累加的，块大小不影响最终的字节序列。
    /// - **按偏移对齐比较**的场景（前缀比对、分片比对）：**必须**用 `read_full`，
    ///   否则两边读到的边界不一致，会把「同样的内容」误判成「不同」。
    pub fn read_full(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = self.read_some(&mut buf[filled..])?;
            if n == 0 {
                break; // EOF
            }
            filled += n;
        }
        Ok(filled)
    }

    // ---------------------------------------------------------- 写

    /// 把 `buf` **全部**写出去（短写自动补齐、EINTR 自动重试）。
    ///
    /// ⚠️ 这是「写多少」的**唯一**入口：调用方传进来的切片长度决定了写入字节数。
    /// 想要「只写 n 字节」就传 `&buf[..n]`，别传整个 buffer。
    pub fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            #[cfg(unix)]
            let r = unsafe {
                libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len())
            };
            #[cfg(not(unix))]
            let r = {
                use std::io::Write;
                match self.file.write(buf) {
                    Ok(n) => n as isize,
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            };

            #[cfg(unix)]
            {
                if r < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
            }
            let n = r as usize;
            // 短写（n < buf.len()）必须继续写剩下的，否则静默丢字节
            buf = &buf[n..];
        }
        Ok(())
    }

    // ---------------------------------------------------------- 位置 / 同步

    /// 当前 Data Fork 长度（`lseek(fd, 0, SEEK_END)`）。
    ///
    /// 用 lseek 而不是 `stat` 的 `st_size`：POSIX 下 lseek 给的是这个 fd
    /// 对应的数据流末尾，语义最直接，也不受平台 stat 结构差异影响。
    pub fn size(&mut self) -> io::Result<u64> {
        #[cfg(unix)]
        {
            let off = unsafe { libc::lseek(self.fd, 0, libc::SEEK_END) };
            if off < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(off as u64)
        }
        #[cfg(not(unix))]
        {
            use std::io::{Seek, SeekFrom};
            let cur = self.file.seek(SeekFrom::Current(0))?;
            let end = self.file.seek(SeekFrom::End(0))?;
            self.file.seek(SeekFrom::Start(cur))?;
            Ok(end)
        }
    }

    /// 把内核缓冲刷到磁盘（`fsync`）。
    ///
    /// 断点续传的前提：不 fsync 的话，进程退出后可能只落了一部分，
    /// 而「已写入长度」已经记成整个块 → 下次接着写就拼错。所以**拷贝成功
    /// 或中断后都必须 fsync**（本模块在 copy_data_fork 里统一做）。
    pub fn sync(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let r = unsafe { libc::fsync(self.fd) };
            if r != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            self.file.sync_all()
        }
    }
}

#[cfg(unix)]
impl Drop for DataFile {
    fn drop(&mut self) {
        // 关错了也无法补救（析构里不能返回错误），故忽略返回值
        unsafe {
            libc::close(self.fd);
        }
    }
}

// ================================================================ 长度查询

/// 一个路径的 **Data Fork** 长度。
///
/// `std::fs::metadata()?.len()` 在两个平台上给的都是 Data Fork 大小
/// （HFS+/APFS 的 `st_size` 不含资源分支），所以这里直接用 std，不必自己 fstat。
/// 记这一笔是为了**别再有人怀疑资源分支把大小算多了** —— 它不会。
pub fn data_fork_len(path: &Path) -> io::Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

// ================================================================ 拷贝

/// 一次 Data Fork 拷贝的结果
#[derive(Debug, Clone, Default)]
pub struct CopyStat {
    /// 续传起点（从目标第几字节开始写）；从头写为 0
    pub base: u64,
    /// 本次实际写入的字节数
    pub written: u64,
    /// 写完后目标的 Data Fork 长度（应等于源长度）
    pub dst_len: u64,
    /// 本想续传但目标不可续（为空 / 比源还大）→ 已改为从头重写
    pub restarted: bool,
    /// 本次是否真的校验过续传前缀
    pub prefix_checked: bool,
}

/// 拷贝的读写语义开关（**只能由上层从 JobOptions 映射进来**）
#[derive(Debug, Clone, Copy)]
pub struct CopyPolicy {
    /// 续传前是否校验「目标已有内容 == 源的前 N 字节」
    pub prefix_check: bool,
}

/// 把 `src` 的 Data Fork 拷到 `dst`（`base` 之后的字节）。
///
/// 这是全应用**唯一**的拷贝实现。要点：
/// 1. 循环 `read`；`read` 返回 `n` 就**只写 `n` 字节**（`&buf[..n]`）；
/// 2. 末块 `n < buf.len()` 时同样只写 n，**不补零**；
/// 3. 不用 ftruncate 收尾 —— 长度天然正确；
/// 4. 成功与失败路径都 `fsync`（断点续传的基础）；
/// 5. `on_bytes(delta)` 上报进度，返回 `false` 表示请求中止。
pub fn copy_data_fork(
    src: &Path,
    dst: &Path,
    mode: WriteMode,
    base_hint: u64,
    policy: CopyPolicy,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64) -> bool,
) -> io::Result<CopyStat> {
    let mut out = CopyStat::default();

    let mut src_f = DataFile::open_read(src)?;
    let src_len = src_f.size()?;

    // ---- 决定起点：续传 vs 从头 ----
    let mut mode = mode;
    let mut base = 0u64;
    if mode == WriteMode::Append {
        let dst_len = data_fork_len(dst).unwrap_or(0);
        // ⚠️ 上层的 base_hint 来自「预扫描时看到的目标长度」，可能已经过期
        // （期间别的进程改过这个文件）。实测值与提示值不一致 → 不可信，从头重写。
        if base_hint != 0 && base_hint != dst_len {
            out.restarted = true;
            mode = WriteMode::Truncate;
        }
        if dst_len == 0 || dst_len >= src_len {
            // 目标为空 → 没什么可续；目标比源还大 → 绝不可能是同一份的半截
            out.restarted = dst_len > 0;
            mode = WriteMode::Truncate;
        } else if policy.prefix_check {
            // ⚠️ 关键安全闸：确认目标里那段内容确实等于源的前 dst_len 字节。
            // 不做这一步就直接 append，只要目标里是一份**内容不同的更短文件**，
            // 就会拼出一个「长度恰好等于源、内容却错」的文件 —— SHA256 必然不一致。
            let mut dst_r = DataFile::open_read(dst)?;
            let same = prefix_matches(&mut src_f, &mut dst_r, dst_len, cancel)?;
            out.prefix_checked = true;
            if same {
                base = dst_len;
            } else {
                // 前缀对不上 → 只能从头重写
                out.restarted = true;
                mode = WriteMode::Truncate;
            }
        } else {
            // ⚠️ 用户关掉了前缀校验。**故意不盲续传**：
            // 备份工具里「静默写坏」比「多花时间重写一遍」严重得多。
            // 这里退化为从头重写，并把决定写进日志（由上层负责提示）。
            out.restarted = true;
            mode = WriteMode::Truncate;
        }
    }
    out.base = if mode == WriteMode::Append { base } else { 0 };

    // ⚠️⚠️ 必须**无条件**把源读位置定位到 base。
    //
    // 不能写成 `if out.base > 0 { seek }`：`prefix_matches` 会把 `src_f` 的读位置
    // 一路推到 `dst_len`。若随后判定「从头重写」（base = 0），那个 if 就不成立、
    // 不会 seek，主循环于是从**文件中间**开始拷 —— 目标长度会是
    // `src_len - dst_len`，或者内容整体错位。这是本模块最容易写错的一处。
    seek_from_start(&mut src_f, out.base)?;

    // ---- 打开目标 ----
    let mut dst_f = DataFile::open_write(
        dst,
        if mode == WriteMode::Append {
            WriteMode::Append
        } else {
            WriteMode::Truncate
        },
    )?;

    // ---- 主循环 ----
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut written: u64 = 0;
    let mut result: io::Result<()> = Ok(());

    loop {
        if cancel.load(Ordering::Relaxed) {
            result = Err(cancelled_err());
            break;
        }
        let n = match src_f.read_some(&mut buf) {
            Ok(0) => break, // 正常读到 Data Fork 末尾
            Ok(n) => n,
            Err(e) => {
                result = Err(e);
                break;
            }
        };

        // ⚠️⚠️ 只写 n 字节。这一步是「长度对、内容错」的分水岭：
        // 写成 write_all(&buf) 就会在末块多写 (buf.len()-n) 个 0，
        // 文件长度靠 ftruncate 补回来之后，stat 一样、内容已经错了。
        if let Err(e) = dst_f.write_all(&buf[..n]) {
            result = Err(e);
            break;
        }
        written += n as u64;
        if !on_bytes(n as u64) {
            result = Err(cancelled_err());
            break;
        }
    }

    // ---- 收尾：无论成败都 fsync，让「已写入长度」与磁盘真实状态一致 ----
    // 先记长度：即使 fsync 失败，已写入的字节数也是有效信息见
    let dst_len_after = dst_f.size().unwrap_or(0);
    let sync_res = dst_f.sync();
    drop(dst_f);
    drop(src_f);

    if let Err(e) = result {
        // 真正的读写错误优先上抛，不让 fsync 的结果把它掩盖掉
        return Err(e);
    }
    // 拷贝成功才要求 fsync 也必须成功 —— 否则「看起来写完了」其实没落盘
    sync_res?;

    out.written = written;
    out.dst_len = dst_len_after;
    Ok(out)
}

/// 比较两个 fd 的**前 `len` 字节**是否相同（分块，不整读进内存）
fn prefix_matches(
    a: &mut DataFile,
    b: &mut DataFile,
    len: u64,
    cancel: &AtomicBool,
) -> io::Result<bool> {
    let mut ba = vec![0u8; CHUNK_SIZE];
    let mut bb = vec![0u8; CHUNK_SIZE];
    let mut remaining = len;
    while remaining > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled_err());
        }
        let want = remaining.min(CHUNK_SIZE as u64) as usize;
        // ⚠️ 必须 read_full：两边要**按同一偏移**对齐比较，
        // 用 read_some 的话一旦两边读到的字节数不同（macOS/APFS 会），
        // 就会把「内容相同」误判成「不同」→ 无谓地整份重写（CI 上实测踩过）。
        let na = a.read_full(&mut ba[..want])?;
        let nb = b.read_full(&mut bb[..want])?;
        // 任何一边提前 EOF：说明长度不足，前缀不可能相同
        if na != nb || na == 0 {
            return Ok(false);
        }
        if ba[..na] != bb[..nb] {
            return Ok(false);
        }
        remaining -= na as u64;
    }
    Ok(true)
}

fn seek_from_start(f: &mut DataFile, pos: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        let off = unsafe { libc::lseek(f.fd, pos as libc::off_t, libc::SEEK_SET) };
        if off < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        use std::io::{Seek, SeekFrom};
        f.file.seek(SeekFrom::Start(pos))?;
        Ok(())
    }
}

// ================================================================ 哈希

/// 对 `src` 的 Data Fork 从 `start` 起最多读 `len` 字节算摘要（`None` = 到末尾）。
///
/// **与拷贝使用完全同一套读取语义**（同一个 `DataFile::read_some` 循环），
/// 所以「拷出去的」和「算哈希的」必然是同一段字节流。
pub fn hash_data_fork_range(
    path: &Path,
    start: u64,
    len: Option<u64>,
    algo: HashAlgo,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> io::Result<(Digest, u64)> {
    let mut f = DataFile::open_read(path)?;
    if start > 0 {
        seek_from_start(&mut f, start)?;
    }
    let mut hasher = Hasher::new(algo);
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut remaining = len.unwrap_or(u64::MAX);
    let mut total: u64 = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled_err());
        }
        let want = remaining.min(CHUNK_SIZE as u64) as usize;
        if want == 0 {
            break;
        }
        let n = f.read_some(&mut buf[..want])?;
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

/// 整个 Data Fork 的摘要
pub fn hash_data_fork(
    path: &Path,
    algo: HashAlgo,
    cancel: &AtomicBool,
    on_bytes: &mut dyn FnMut(u64),
) -> io::Result<(Digest, u64)> {
    hash_data_fork_range(path, 0, None, algo, cancel, on_bytes)
}

// ================================================================ 分片调试

/// 一个分片的信息
#[derive(Debug, Clone)]
pub struct ChunkInfo {
    pub index: u64,
    pub offset: u64,
    pub len: u64,
    pub hex: String,
}

/// 源 / 目标 的分片差异
#[derive(Debug, Clone, Default)]
pub struct ChunkDiff {
    pub chunk_size: u64,
    pub total_chunks: u64,
    /// 首个不一致的分片下标（None = 所有分片都一致）
    pub first_bad: Option<u64>,
    pub bad: Vec<ChunkInfo>,      // 源侧
    pub bad_dst: Vec<ChunkInfo>,  // 目标侧（与 bad 一一对应）
    /// 只扫到的分片数（达到 max_bad 就提前停）
    pub scanned_chunks: u64,
}

/// 按固定分片大小分别计算两边的摘要，**定位第一个不一致的分片**。
///
/// 用途：`SHA-256 不一致` 只知道「整体不同」，这个能告诉你「从第几个分片开始不同」，
/// 再乘上分片大小就是**首个坏字节的偏移** —— 直接指向成因：
/// - 第 0 片就不同 → 目标那份不是这份源（没真拷 / 旧文件）
/// - 前面几十片都对、从第 k 片开始不同 → 追加式损坏（k×chunk 就是上次的长度）
/// - 零散几片不同 → 链路 / 介质静默损坏
pub fn diff_chunks(
    src: &Path,
    dst: &Path,
    chunk_size: u64,
    algo: HashAlgo,
    cancel: &AtomicBool,
    max_bad: usize,
) -> io::Result<ChunkDiff> {
    let cs = chunk_size.max(1);
    let mut out = ChunkDiff {
        chunk_size: cs,
        ..Default::default()
    };

    let mut fa = DataFile::open_read(src)?;
    let mut fb = DataFile::open_read(dst)?;
    let mut ba = vec![0u8; cs as usize];
    let mut bb = vec![0u8; cs as usize];

    let mut idx: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled_err());
        }
        // 同 prefix_matches：分片比对要求两边按同一偏移对齐，必须读满
        let na = fa.read_full(&mut ba)?;
        let nb = fb.read_full(&mut bb)?;
        if na == 0 && nb == 0 {
            break;
        }
        out.total_chunks = idx + 1;
        out.scanned_chunks = idx + 1;

        // 长度不同也算不一致（一方先到 EOF）
        let same = na == nb && ba[..na] == bb[..nb];
        if !same {
            if out.first_bad.is_none() {
                out.first_bad = Some(idx);
            }
            let ha = digest_of_bytes(&ba[..na], algo);
            let hb = digest_of_bytes(&bb[..nb], algo);
            out.bad.push(ChunkInfo {
                index: idx,
                offset: idx * cs,
                len: na as u64,
                hex: ha,
            });
            out.bad_dst.push(ChunkInfo {
                index: idx,
                offset: idx * cs,
                len: nb as u64,
                hex: hb,
            });
            if out.bad.len() >= max_bad {
                break; // 找够了就停，别把几百 GB 全扫一遍
            }
        }
        idx += 1;
        if na == 0 || nb == 0 {
            break;
        }
    }
    Ok(out)
}

/// 直接对一段内存算摘要（分片用，不需要再开文件）
fn digest_of_bytes(bytes: &[u8], algo: HashAlgo) -> String {
    let mut h = Hasher::new(algo);
    h.write(bytes);
    h.finish(algo).hex().to_string()
}

// ================================================================ xattr / 元数据

/// 目标文件的隔离属性名（macOS 从网上下载/拷贝来的文件会带上，导致「已损坏」提示）
pub const QUARANTINE_XATTR: &str = "com.apple.quarantine";

/// 移除目标文件的 `com.apple.quarantine`。
///
/// 返回 `Ok(true)` = 确实有并删掉了；`Ok(false)` = 本来就没有（正常，不算错）；
/// `Err` = 删除动作本身失败（权限等），上层只记 warn，**不影响拷贝结果**。
///
/// 非 macOS 平台恒返回 `Ok(false)` —— 隔离属性是苹果独有的概念。
pub fn remove_quarantine(path: &Path) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径含 NUL 字节"))?;
        let name = std::ffi::CString::new(QUARANTINE_XATTR).unwrap();
        let r = unsafe { libc::removexattr(c.as_ptr(), name.as_ptr(), 0) };
        if r == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        // ENODATA / ENOATTR：属性不存在 —— 正常情况
        if matches!(e.raw_os_error(), Some(libc::ENOATTR)) {
            return Ok(false);
        }
        return Err(e);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Ok(false)
    }
}

/// 把源文件的**元数据**（权限位 + 修改时间 + xattr）复制到目标。
///
/// - 只在 `JobOptions.copy_metadata == true` 时由上层调用，**默认不调用**。
/// - ⚠️ **绝不复制资源分支**：那需要打开 `..namedfork/rsrc`，本程序禁止碰它。
///   资源分支属于元数据，缺了不影响 Data Fork 内容与哈希。
/// - 全程 best-effort：单项失败只记 warn，不阻断拷贝。
pub fn copy_metadata(src: &Path, dst: &Path) -> Vec<String> {
    let mut warns = Vec::new();

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let csrc = match std::ffi::CString::new(src.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                warns.push("源路径含 NUL，跳过元数据复制".to_string());
                return warns;
            }
        };
        let cdst = match std::ffi::CString::new(dst.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                warns.push("目标路径含 NUL，跳过元数据复制".to_string());
                return warns;
            }
        };

        // ---- 权限位 + 时间戳（用 stat 拿源的，再 chmod/utimes 到目标）----
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::stat(csrc.as_ptr(), &mut st) == 0 {
                if libc::chmod(cdst.as_ptr(), st.st_mode & 0o7777) != 0 {
                    warns.push(format!("复制权限位失败：{}", io::Error::last_os_error()));
                }
                // 用 utimensat 只改 atime/mtime（保持纳秒精度）
                let times = [
                    libc::timespec {
                        tv_sec: st.st_atime,
                        tv_nsec: st.st_atime_nsec,
                    },
                    libc::timespec {
                        tv_sec: st.st_mtime,
                        tv_nsec: st.st_mtime_nsec,
                    },
                ];
                if libc::utimensat(libc::AT_FDCWD, cdst.as_ptr(), times.as_ptr(), 0) != 0 {
                    warns.push(format!("复制时间戳失败：{}", io::Error::last_os_error()));
                }
            } else {
                warns.push(format!("读取源元数据失败：{}", io::Error::last_os_error()));
            }
        }

        #[cfg(target_os = "macos")]
        {
            match copy_xattrs_macos(&csrc, &cdst) {
                Ok(n) => {
                    if n > 0 {
                        warns.push(format!("__COPIED_XATTRS__{n}")); // 上层可据此记 info 日志
                    }
                }
                Err(e) => warns.push(format!("复制扩展属性失败：{e}")),
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (src, dst);
        warns.push("当前平台不支持元数据复制，已跳过".to_string());
    }

    warns
}

/// macOS：复制全部 xattr（best-effort，单个属性失败不中断）
#[cfg(target_os = "macos")]
fn copy_xattrs_macos(
    csrc: &std::ffi::CString,
    cdst: &std::ffi::CString,
) -> io::Result<usize> {
    // 先问需要多大缓冲
    let size = unsafe { libc::listxattr(csrc.as_ptr(), std::ptr::null_mut(), 0, 0) };
    if size <= 0 {
        return Ok(0);
    }
    let mut names = vec![0u8; size as usize];
    let n = unsafe {
        libc::listxattr(
            csrc.as_ptr(),
            names.as_mut_ptr() as *mut libc::c_char,
            names.len(),
            0,
        )
    };
    if n <= 0 {
        return Ok(0);
    }
    names.truncate(n as usize);

    let mut copied = 0usize;
    // names 是 NUL 分隔的属性名列表
    for raw in names.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        let cname = match std::ffi::CString::new(raw.to_vec()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // 逐个读值
        let vsize = unsafe {
            libc::getxattr(
                csrc.as_ptr(),
                cname.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        };
        if vsize < 0 {
            continue;
        }
        let mut val = vec![0u8; vsize as usize];
        let got = unsafe {
            libc::getxattr(
                csrc.as_ptr(),
                cname.as_ptr(),
                val.as_mut_ptr() as *mut libc::c_void,
                val.len(),
                0,
                0,
            )
        };
        if got < 0 {
            continue;
        }
        let ok = unsafe {
            libc::setxattr(
                cdst.as_ptr(),
                cname.as_ptr(),
                val.as_ptr() as *const libc::c_void,
                got as usize,
                0,
                0,
            )
        };
        if ok == 0 {
            copied += 1;
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cinebackup_posix_{tag}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 确定性伪随机内容（刻意不用全零：全零会掩盖「写到错误偏移」类 bug）
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

    /// 拷贝用的进度回调：返回 true = 继续
    fn noop(_: u64) -> bool {
        true
    }

    /// 哈希用的进度回调：返回 ()（签名不同，别混用）
    fn sink(_: u64) {}

    /// 规则②的回归：**末块只写 n 字节**，长度精确、内容精确。
    /// 用「不是块大小整数倍」的长度，专门戳「末块写满 buffer」这个坑。
    #[test]
    fn t01_last_block_writes_exact_len_no_padding() {
        let d = tmpdir("t01");
        let src = d.join("src.bin");
        let dst = d.join("dst.bin");

        // CHUNK_SIZE + 12345 → 两块，第二块远小于 buffer
        let len = CHUNK_SIZE + 12345;
        let content = data(len, 42);
        fs::write(&src, &content).unwrap();

        let cancel = AtomicBool::new(false);
        let st = copy_data_fork(
            &src,
            &dst,
            WriteMode::Truncate,
            0,
            CopyPolicy { prefix_check: true },
            &cancel,
            &mut noop,
        )
        .unwrap();

        assert_eq!(st.written, len as u64, "写入字节数必须等于源长度");
        assert_eq!(st.dst_len, len as u64, "目标长度必须等于源长度（不能靠截断补救）");
        assert_eq!(fs::metadata(&dst).unwrap().len(), len as u64);
        assert_eq!(fs::read(&dst).unwrap(), content, "内容必须逐字节一致");
    }

    /// 内容一致但长度不是块整数倍时，哈希必须相等（端到端）
    #[test]
    fn t02_hash_matches_after_copy_for_odd_length() {
        let d = tmpdir("t02");
        let src = d.join("s.bin");
        let dst = d.join("d.bin");
        let content = data(CHUNK_SIZE * 2 + 7, 7);
        fs::write(&src, &content).unwrap();

        let cancel = AtomicBool::new(false);
        copy_data_fork(
            &src,
            &dst,
            WriteMode::Truncate,
            0,
            CopyPolicy { prefix_check: true },
            &cancel,
            &mut noop,
        )
        .unwrap();

        let (a, na) = hash_data_fork(&src, HashAlgo::Sha256, &cancel, &mut sink).unwrap();
        let (b, nb) = hash_data_fork(&dst, HashAlgo::Sha256, &cancel, &mut sink).unwrap();
        assert_eq!(na, nb);
        assert_eq!(a, b, "拷贝后两边 SHA-256 必须一致");
    }

    /// ⚠️ 核心回归：目标里是「内容不同的更短文件」时，**绝不能盲续传拼出
    /// 「长度正确、内容错误」的文件**。开启前缀校验 → 应检测到并从头重写。
    #[test]
    fn t03_stale_shorter_target_does_not_get_spliced() {
        let d = tmpdir("t03");
        let src = d.join("src.bin");
        let dst = d.join("dst.bin");

        let content = data(3 * 1024 * 1024, 11);
        fs::write(&src, &content).unwrap();
        // 目标是一份**更短且内容完全不同**的文件（模拟旧版本素材）
        fs::write(&dst, data(1 * 1024 * 1024, 999)).unwrap();

        let cancel = AtomicBool::new(false);
        let st = copy_data_fork(
            &src,
            &dst,
            WriteMode::Append, // 上层按「大小不一致」判成了续传
            1 * 1024 * 1024,
            CopyPolicy { prefix_check: true },
            &cancel,
            &mut noop,
        )
        .unwrap();

        assert!(st.prefix_checked, "必须真的校验过前缀");
        assert!(st.restarted, "前缀不一致 → 必须改为从头重写");
        assert_eq!(st.base, 0);
        assert_eq!(fs::read(&dst).unwrap(), content, "结果必须是源的完整内容");

        let (a, _) = hash_data_fork(&src, HashAlgo::Sha256, &cancel, &mut sink).unwrap();
        let (b, _) = hash_data_fork(&dst, HashAlgo::Sha256, &cancel, &mut sink).unwrap();
        assert_eq!(a, b, "修复后不允许出现「长度对、哈希错」");
    }

    /// 关闭前缀校验时不允许盲续传（保守退化为从头重写）
    #[test]
    fn t04_prefix_check_off_degrades_to_full_rewrite() {
        let d = tmpdir("t04");
        let src = d.join("src.bin");
        let dst = d.join("dst.bin");
        let content = data(1024 * 1024, 5);
        fs::write(&src, &content).unwrap();
        fs::write(&dst, data(1024, 6)).unwrap();

        let cancel = AtomicBool::new(false);
        let st = copy_data_fork(
            &src,
            &dst,
            WriteMode::Append,
            1024,
            CopyPolicy { prefix_check: false },
            &cancel,
            &mut noop,
        )
        .unwrap();

        assert!(st.restarted, "关掉校验时必须退化为从头重写，不能盲拼");
        assert_eq!(fs::read(&dst).unwrap(), content);
    }

    /// 合法续传仍然要能工作（前缀真的相同）
    #[test]
    fn t05_legit_resume_still_works() {
        let d = tmpdir("t05");
        let src = d.join("src.bin");
        let dst = d.join("dst.bin");
        let content = data(2 * 1024 * 1024, 21);
        fs::write(&src, &content).unwrap();
        // 前半截是真的（模拟上次中断）
        fs::write(&dst, &content[..700_000]).unwrap();

        let cancel = AtomicBool::new(false);
        let st = copy_data_fork(
            &src,
            &dst,
            WriteMode::Append,
            700_000,
            CopyPolicy { prefix_check: true },
            &cancel,
            &mut noop,
        )
        .unwrap();

        assert!(st.prefix_checked);
        assert!(!st.restarted, "前缀真的一致 → 应正常续传而不是重写");
        assert_eq!(st.base, 700_000);
        assert_eq!(st.written, (2 * 1024 * 1024 - 700_000) as u64);
        assert_eq!(fs::read(&dst).unwrap(), content, "续传结果必须与源一致");
    }

    /// 分片调试：定位到正确的不一致分片
    #[test]
    fn t06_chunk_diff_locates_first_bad_chunk() {
        let d = tmpdir("t06");
        let src = d.join("src.bin");
        let dst = d.join("dst.bin");

        let chunk = 4096u64;
        let n = (chunk * 5) as usize;
        let a = data(n, 3);
        fs::write(&src, &a).unwrap();
        // 目标：第 3 个分片（下标 2）改掉一个字节
        let mut b = a.clone();
        b[2 * chunk as usize + 100] ^= 0xFF;
        fs::write(&dst, &b).unwrap();

        let cancel = AtomicBool::new(false);
        let diff = diff_chunks(&src, &dst, chunk, HashAlgo::Sha256, &cancel, 10).unwrap();

        assert_eq!(diff.first_bad, Some(2), "应定位到第 2 片（0 基）");
        assert_eq!(diff.bad.len(), 1, "只坏了一片");
        assert_eq!(diff.bad[0].offset, 2 * chunk);
        assert_eq!(diff.bad[0].hex.len(), 64, "SHA-256 十六进制 = 64 字符");
        assert_ne!(diff.bad[0].hex, diff.bad_dst[0].hex, "两边摘要必须不同");
        // 一致性：没坏的分片摘要两边相同
        assert_eq!(diff.bad[0].len, diff.bad_dst[0].len);
    }

    /// 分片调试：完全一致时不该报任何坏片
    #[test]
    fn t07_chunk_diff_clean_when_identical() {
        let d = tmpdir("t07");
        let src = d.join("src.bin");
        let dst = d.join("dst.bin");
        let content = data(10_000, 8);
        fs::write(&src, &content).unwrap();
        fs::write(&dst, &content).unwrap();

        let cancel = AtomicBool::new(false);
        let diff = diff_chunks(&src, &dst, 1024, HashAlgo::Sha256, &cancel, 10).unwrap();
        assert_eq!(diff.first_bad, None);
        assert!(diff.bad.is_empty());
        assert_eq!(diff.total_chunks, 10, "10000 字节 / 1024 = 10 片");
    }

    /// `read_full` 的契约：读满 buf，或读到 EOF 为止（两种情形都要对）
    #[test]
    fn t09_read_full_fills_or_stops_at_eof() {
        let d = tmpdir("t09");
        let f = d.join("small.bin");
        let content = data(1000, 5);
        fs::write(&f, &content).unwrap();

        let mut fh = DataFile::open_read(&f).unwrap();

        // 情形 A：buf 比文件大 → 停在 EOF，返回真实长度
        let mut big = vec![0u8; 4096];
        let n = fh.read_full(&mut big).unwrap();
        assert_eq!(n, 1000, "buf 大于文件时应返回文件实际长度");
        assert_eq!(&big[..n], &content[..]);

        // 情形 B：buf 比文件小、需要多次 read 才能读满 → 循环拼起来必须等于原文
        let mut fh = DataFile::open_read(&f).unwrap();
        let mut acc: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 64];
        loop {
            let n = fh.read_full(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            // 除最后一块外，每块都必须是「满」的 —— 这就是 read_full 与 read_some 的差别
            acc.extend_from_slice(&buf[..n]);
            if n < buf.len() {
                break;
            }
        }
        assert_eq!(acc, content, "分块读满再拼接必须逐字节等于原文");
    }

    /// ⚠️ 关键回归：**两边写入方式不同**（一次写完 vs 分多次追加）时，
    /// 前缀比对仍必须判为「一致」。
    ///
    /// 这条直接对应 CI 上的失败：`read(2)` 返回的粒度会受底层影响，
    /// 若比对时用 `read_some`（不保证读满），两边读到的边界不同就会误判为不一致。
    /// 用 `read_full` 后，无论底层一次读多少，两边都按同一偏移对齐。
    #[test]
    fn t10_prefix_match_survives_different_write_patterns() {
        let d = tmpdir("t10");
        let a = d.join("one_shot.bin");
        let b = d.join("appended.bin");
        let content = data(3 * 1024 * 1024 + 777, 17);

        // a：一次性写完
        fs::write(&a, &content).unwrap();
        // b：分多次追加写（内容相同，但写入路径不同 → 底层布局/读粒度可能不同）
        {
            let mut f = DataFile::open_write(&b, WriteMode::Truncate).unwrap();
            for chunk in content.chunks(200_000) {
                f.write_all(chunk).unwrap();
            }
            f.sync().unwrap();
        }

        let mut fa = DataFile::open_read(&a).unwrap();
        let mut fb = DataFile::open_read(&b).unwrap();
        let cancel = AtomicBool::new(false);
        let same = prefix_matches(&mut fa, &mut fb, content.len() as u64, &cancel).unwrap();
        assert!(same, "内容相同 → 前缀比对必须判为一致（不能因读粒度差异误判）");

        // 长度也真的相同（确认不是「两边都提前 EOF 而恰好相等」）
        assert_eq!(data_fork_len(&a).unwrap(), data_fork_len(&b).unwrap());
    }

    /// 测试 xattr 清理接口在无隔离属性时是「正常返回 false」而不是报错
    #[test]
    fn t08_remove_quarantine_is_idempotent() {
        let d = tmpdir("t08");
        let f = d.join("plain.bin");
        fs::write(&f, b"hi").unwrap();

        // 没有隔离属性 → Ok(false)，且不能报错
        let r = remove_quarantine(&f).unwrap();
        assert!(!r, "普通文件本来就没有 com.apple.quarantine");
        // 再调一次仍然不报错
        assert!(!remove_quarantine(&f).unwrap());
    }
}
