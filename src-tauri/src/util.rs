//! 通用工具：时间格式化、路径处理、速率/剩余时间估算

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 统一分块大小：4 MiB。
/// 几十 GB 的 ProRes / MXF 单文件也只需要 4 MiB 常驻内存，不会 OOM。
pub const CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// 拷贝/哈希的读缓冲（比 CHUNK_SIZE 小的顺序读用）
pub const IO_BUFFER: usize = 1024 * 1024;

// ---------------------------------------------------------------- 时间

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 无需第三方依赖的 UTC ISO-8601（`2026-09-18T06:51:31Z`）
pub fn now_iso8601() -> String {
    let secs = now_unix() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant 的 civil_from_days 算法（公历，1970-01-01 起算）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------- 路径

/// 路径 → UTF-8 字符串（非法字符用 replacement，不 panic）。
/// 中文、空格、emoji 文件名都能原样保留。
pub fn path_to_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// 按当前平台规范化路径分隔符：
/// - Windows：`/` → `\`
/// - macOS/Linux：`\` → `/`
///
/// 仅用于跨平台加载任务 JSON 时的路径修补，不改变盘符 / 卷名。
pub fn normalize_separators(s: &str) -> String {
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s.replace('\\', "/")
    }
}

/// 取路径的最后一段（文件名 / 目录名）
pub fn file_name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path_to_string(p))
}

/// 判断是否像 macOS 的绝对路径（用于跨平台加载任务时提示）
pub fn looks_like_macos_path(s: &str) -> bool {
    s.starts_with("/Volumes/") || s.starts_with("/Users/") || s.starts_with('/')
}

/// 判断是否像 Windows 的绝对路径（`C:\...` 或 UNC `\\server\share`）
pub fn looks_like_windows_path(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) || s.starts_with("\\\\")
}

/// 把「相对根目录的一段相对路径」拼到目标根下，始终保持正确的父子关系
pub fn join_relative(root: &Path, rel: &Path) -> PathBuf {
    let mut out = root.to_path_buf();
    for seg in rel.components() {
        use std::path::Component;
        match seg {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            // 理论上不会出现，防御性忽略
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------- 展示

pub fn human_bytes(n: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 || v >= 100.0 {
        format!("{:.0} {}", v, U[i])
    } else {
        format!("{:.2} {}", v, U[i])
    }
}

// ---------------------------------------------------------------- 速率 / ETA

/// 滑动窗口速率计：用于「实时拷贝速度 + 预估剩余时间」
///
/// - 只统计**新写入**的字节（续传时已存在的部分不计入速度，否则速度会虚高）
/// - 使用 EWMA 平滑，避免瞬时抖动导致 ETA 跳动
#[derive(Debug, Clone)]
pub struct RateMeter {
    start: Instant,
    last_t: Instant,
    last_bytes: u64,
    ema: f64,
    written: u64,
}

impl Default for RateMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateMeter {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last_t: now,
            last_bytes: 0,
            ema: 0.0,
            written: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// 累计新写入字节
    pub fn add(&mut self, bytes: u64) {
        self.written = self.written.saturating_add(bytes);
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// 当前速度（字节/秒）。内部自带 250ms 采样节流。
    pub fn speed(&mut self) -> f64 {
        let now = Instant::now();
        let dt = now.duration_since(self.last_t).as_secs_f64();
        if dt >= 0.25 {
            let inst = (self.written.saturating_sub(self.last_bytes)) as f64 / dt;
            self.ema = if self.ema <= 0.0 {
                inst
            } else {
                0.7 * self.ema + 0.3 * inst
            };
            self.last_t = now;
            self.last_bytes = self.written;
        }
        if self.ema > 0.0 {
            self.ema
        } else {
            let e = self.elapsed();
            if e > 0.1 {
                self.written as f64 / e
            } else {
                0.0
            }
        }
    }

    /// 预估剩余秒数（remaining = 还需写入的字节数）
    pub fn eta(&mut self, remaining: u64) -> f64 {
        let s = self.speed();
        if s <= 1.0 || remaining == 0 {
            return 0.0;
        }
        remaining as f64 / s
    }
}

/// 事件节流：避免每 4 MiB 都往前端推一次进度
#[derive(Debug)]
pub struct Throttle {
    last: Instant,
    interval_ms: u64,
}

impl Throttle {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            last: Instant::now() - std::time::Duration::from_millis(interval_ms),
            interval_ms,
        }
    }
    /// 到点返回 true，并重置计时
    pub fn ready(&mut self) -> bool {
        if self.last.elapsed().as_millis() as u64 >= self.interval_ms {
            self.last = Instant::now();
            true
        } else {
            false
        }
    }
    /// 强制放行（用于文件开始 / 结束等关键节点）
    pub fn force(&mut self) -> bool {
        self.last = Instant::now();
        true
    }
}
