# -*- coding: utf-8 -*-
"""重新生成 src-tauri/src/unassigned.rs 里的 UNASSIGNED_RANGES 区间表。

依据：Apple APFS FAQ + 实测 v0.5.2 漏掉的 Cf 字符（ZERO WIDTH SPACE 等）。
APFS 拒绝：未分配 (Cn) + 控制 (Cc) + 格式 (Cf) + 非字符（也在 Cn/Cc 里）。

策略：**只更新区间数组和顶部的统计注释**，不动 is_rejected / first_rejected_in
（这两个是手写的，下次跑脚本也不会丢）。
"""
import re
import sys
import unicodedata
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "src-tauri" / "src" / "unassigned.rs"


def build_ranges():
    REJECT = {"Cn", "Cc", "Cf"}
    ranges = []
    start = None
    for cp in range(0x110000):
        if 0xD800 <= cp <= 0xDFFF:  # Cs 代理
            if start is not None:
                ranges.append((start, cp - 1))
                start = None
            continue
        ch = chr(cp)
        if unicodedata.category(ch) in REJECT:
            if start is None:
                start = cp
        else:
            if start is not None:
                ranges.append((start, cp - 1))
                start = None
    if start is not None:
        ranges.append((start, 0x10FFFF))
    return ranges


def main():
    ranges = build_ranges()
    total = sum(hi - lo + 1 for lo, hi in ranges)
    ver = unicodedata.unidata_version
    text = OUT.read_text(encoding="utf-8")

    # 1) 替换顶部统计注释行（"//! 共 ... 个区间，覆盖 ... 个码位。"）
    text = re.sub(
        r"//! 共 \d+ 个区间，覆盖 \d+ 个码位。",
        "//! 共 {} 个区间，覆盖 {} 个码位。".format(len(ranges), total),
        text,
        count=1,
    )

    # 2) 替换 UNASSIGNED_RANGES 数组（保留 pub const + 类型注解）
    new_body = "pub const UNASSIGNED_RANGES: [(u32, u32); {}] = [\n".format(len(ranges))
    for lo, hi in ranges:
        new_body += "    (0x{:04X}, 0x{:04X}),\n".format(lo, hi)
    new_body += "];"
    text = re.sub(
        r"pub const UNASSIGNED_RANGES: \[\(u32, u32\); \d+\] = \[.*?\];",
        new_body,
        text,
        count=1,
        flags=re.DOTALL,
    )

    OUT.write_text(text, encoding="utf-8", newline="\n")
    print("已更新 {}: {} 个区间 / {} 个码位 (Unicode {}, Cn+Cc+Cf)".format(
        OUT, len(ranges), total, ver))


if __name__ == "__main__":
    sys.exit(main())