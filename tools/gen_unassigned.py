# -*- coding: utf-8 -*-
"""生成 APFS 拒绝的 Unicode 码位表（未分配码位 + 非字符）→ src-tauri/src/unassigned.rs

依据：Apple 官方 APFS FAQ ——
  "APFS doesn't allow files to be created with filenames that contain
   unassigned codepoints in the Unicode 9.0 standard"
即：文件名合法 UTF-8 也可能被 APFS 以 EILSEQ (errno 92) 拒绝，只要里面
含「未分配码位」或「非字符」（noncharacters，Unicode 规定永不用于交换）。

这类字符的特性（正是本工具要解决的怪象）：
  - 是合法 UTF-8，Rust from_utf8 通过 → 现有 warn_if_non_utf8 抓不到
  - 终端/界面渲染成空白或方框 → 日志、弹窗看起来完全正常
  - 用户手打命令用的是干净字节 → cp / touch 都成功
  - 应用带着原始字节 File::create → APFS 拒绝 → Illegal byte sequence

用 Python 的 unicodedata（比 Unicode 9.0 新）生成 Cn（未分配）区间表。
表比 APFS 实际用的略新（少数新字符 Python 认为已分配、旧 APFS 仍拒绝），
所以只能作为「预检警告」与「重试兜底」，真正的裁决者是目标盘本身。
"""
import unicodedata
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "src-tauri" / "src" / "unassigned.rs"


def main():
    ranges = []
    start = None
    for cp in range(0x110000):
        # 代理区（Cs）根本无法出现在 Rust char 里，跳过
        if 0xD800 <= cp <= 0xDFFF:
            if start is not None:
                ranges.append((start, cp - 1))
                start = None
            continue
        ch = chr(cp)
        if unicodedata.category(ch) == "Cn":
            if start is None:
                start = cp
        else:
            if start is not None:
                ranges.append((start, cp - 1))
                start = None
    if start is not None:
        ranges.append((start, 0x10FFFF))

    total = sum(hi - lo + 1 for lo, hi in ranges)
    ver = unicodedata.unidata_version
    lines = [
        "//! 未分配 / 非字符 Unicode 码位区间表（自动生成，勿手改）",
        "//!",
        "//! 生成：`python tools/gen_unassigned.py`（unicodedata {}）".format(ver),
        "//!",
        "//! 依据 Apple APFS FAQ：APFS 拒绝创建文件名含未分配码位的文件（EILSEQ / errno 92），",
        "//! 即使字节是合法 UTF-8。这类字符在界面上不可见，是「cp 能拷、应用报错」怪象的根因。",
        "//! 共 {} 个区间，覆盖 {} 个码位。".format(len(ranges), total),
        "",
        "/// `(起始码位, 结束码位)` 闭区间，升序、互不重叠。",
        "pub const UNASSIGNED_RANGES: [(u32, u32); {}] = [".format(len(ranges)),
    ]
    for lo, hi in ranges:
        lines.append("    (0x{:04X}, 0x{:04X}),".format(lo, hi))
    lines += [
        "];",
        "",
        "/// 该码位是否属于「未分配 / 非字符」（APFS 等文件系统会以 EILSEQ 拒绝）",
        "pub fn is_rejected(cp: u32) -> bool {",
        "    let idx = UNASSIGNED_RANGES.partition_point(|&(_, hi)| hi < cp);",
        "    if idx >= UNASSIGNED_RANGES.len() {",
        "        return false;",
        "    }",
        "    let (lo, hi) = UNASSIGNED_RANGES[idx];",
        "    lo <= cp && cp <= hi",
        "}",
        "",
        "#[cfg(test)]",
        "mod tests {",
        "    use super::is_rejected;",
        "",
        "    #[test]",
        "    fn rejects_unassigned_and_noncharacters() {",
        "        assert!(is_rejected(0xFFFE)); // 非字符",
        "        assert!(is_rejected(0xFFFF));",
        "        assert!(is_rejected(0xFDD0)); // 非字符区",
        "        assert!(is_rejected(0x0378)); // 未分配（自 Unicode 1.1 起一直是洞）",
        "    }",
        "",
        "    #[test]",
        "    fn accepts_normal_text() {",
        "        assert!(!is_rejected(b'A' as u32));",
        "        assert!(!is_rejected(0x4E2D)); // 中",
        "        assert!(!is_rejected(0x00B7)); // · 中点",
        "        assert!(!is_rejected(0x00A0)); // NBSP（已分配，APFS 接受）",
        "        assert!(!is_rejected(0x1F600)); // emoji（已分配）",
        "        assert!(!is_rejected(0xE000)); // 私用区（已分配）",
        "        assert!(!is_rejected(0x300A)); // 《",
        "    }",
        "}",
        "",
    ]
    OUT.write_text("\n".join(lines), encoding="utf-8", newline="\n")
    print("已生成 {}: {} 个区间 / {} 个码位 (Unicode {})".format(OUT, len(ranges), total, ver))


if __name__ == "__main__":
    sys.exit(main())
