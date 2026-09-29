#!/usr/bin/env python3
"""审计文件名 —— 找出会让拷贝在某个文件系统上失败的名字。

## 为什么需要它

拷贝时报 `Illegal byte sequence (os error 92)`（POSIX errno 92 = EILSEQ），
是**目标文件系统拒绝了这个路径名**，不是文件坏了、不是磁盘满、不是权限不足。
常见的三种名字级原因：

1. `win_illegal`  —— 名字里有 NTFS/exFAT 不允许的字符 `<>:"/\\|?*`。
   最坑的是冒号 `:`：HFS+/APFS 允许，Finder 还会把它显示成 `/`，
   在 Mac 上看着毫无问题，一拷到 NTFS/exFAT 就报 EILSEQ。
   （摄影师素材里 `2024_9_17 10:30 拍摄.psd` 这种时间戳文件名非常常见）
2. `not_utf8`     —— 名字不是合法 UTF-8（从 Windows/FAT 盘、SMB 共享、
   GBK 编码的旧盘搬过来的）。macOS 的 APFS/HFS+ 要求合法 UTF-8。
3. `trailing_dot` —— 名字以空格或点结尾（Windows 侧会被静默改写）。
   另有 `ctrl_char` / `too_long_bytes` / `win_reserved` 几种边角情况。

## 用法

    python3 audit_names.py "/Volumes/素材盘/某项目"      # 扫描目录
    python3 audit_names.py 某个文件.psd                  # 扫描单个文件
    python3 audit_names.py --json /Volumes/素材盘 > out.json
    python3 audit_names.py --self-test                   # 自检，验证判定逻辑

只读：**本脚本不重命名、不删除任何东西**，只打印报告和可复制的 `mv` 建议。
扫描结果里每一条都会给出行之有效的改名命令，确认无误后再手动执行。
退出码：0 = 干净，1 = 发现问题，2 = 参数/环境错误。

作者：CineBackup 工具链。目录名同样会被检查 —— 非法字节往往在父目录里。
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import unicodedata

# NTFS / exFAT / FAT32 都禁止这 9 个字符（`/` 在 POSIX 侧是分隔符，不会出现在名字里，
# 但 Windows 盘经由某些共享挂载过来时可能残留，所以一并检查）
WIN_ILLEGAL = '<>:"/\\|?*'

# Windows 保留设备名（不分扩展名比较）
WIN_RESERVED = {
    "CON", "PRN", "AUX", "NUL",
    *(f"COM{i}" for i in range(1, 10)),
    *(f"LPT{i}" for i in range(1, 10)),
}

# 各文件系统对单个名字的上限（字节数口径最严格的是 255）
NAME_BYTE_LIMIT = 255

ISSUE_LABEL = {
    "not_utf8": "名字不是合法 UTF-8（APFS/HFS+ 会直接拒绝）",
    "win_illegal": "含 NTFS/exFAT 不允许的字符",
    "ctrl_char": "含控制字符（不可见，最容易被忽略）",
    "trailing_dot": "以空格或点结尾",
    "too_long_bytes": "名字超过 255 字节",
    "win_reserved": "是 Windows 保留设备名",
}

ISSUE_FIX = {
    "not_utf8": "按显示出来的乱码部分重新输入，或先用 macOS 的 Finder 改名再拷",
    "win_illegal": "把 : 换成 ： 或 -，其余 < > \" | ? * \\ 直接删掉",
    "ctrl_char": "删掉不可见字符（用 mv 命令重建名字最稳）",
    "trailing_dot": "去掉结尾的空格/点",
    "too_long_bytes": "截短，中文一个字算 3 字节",
    "win_reserved": "改名，例如 CON → CON_1",
}


def visible(text: str) -> str:
    """把不可见/敏感字符转义出来，避免「看着正常其实有鬼」。"""
    out = []
    for ch in text:
        if ch in WIN_ILLEGAL or unicodedata.category(ch) in ("Cc", "Cf", "Co", "Cs"):
            out.append("\\u%04x" % ord(ch))
        else:
            out.append(ch)
    return "".join(out)


def classify(name: bytes) -> tuple[str, list[str]]:
    """判定一个名字（原始字节）。返回 (用于显示的名字, 问题列表)。"""
    issues: list[str] = []

    try:
        text = name.decode("utf-8")
    except UnicodeDecodeError:
        issues.append("not_utf8")
        text = name.decode("utf-8", "replace")

    if any(ch in WIN_ILLEGAL for ch in text):
        issues.append("win_illegal")
    if any(unicodedata.category(ch) in ("Cc", "Cf") for ch in text):
        issues.append("ctrl_char")
    if text and text[-1] in " .":
        issues.append("trailing_dot")
    if len(name) > NAME_BYTE_LIMIT:
        issues.append("too_long_bytes")
    stem = text.split(".")[0].strip().upper()
    if stem in WIN_RESERVED:
        issues.append("win_reserved")

    return text, issues


def safe_name(text: str) -> str:
    """给一个「各平台都安全」的建议名（只用于展示，不落盘）。"""
    out = []
    for ch in text:
        if ch in WIN_ILLEGAL:
            out.append("：" if ch == ":" else "_")
        elif unicodedata.category(ch) in ("Cc", "Cf"):
            continue
        else:
            out.append(ch)
    s = "".join(out).rstrip(" .")
    if not s:
        s = "unnamed"
    # 按字节截断，且不要截出半个 UTF-8 字符
    raw = s.encode("utf-8")
    if len(raw) > NAME_BYTE_LIMIT:
        cut = raw[:NAME_BYTE_LIMIT]
        while cut:
            try:
                s = cut.decode("utf-8")
                break
            except UnicodeDecodeError:
                cut = cut[:-1]
    stem = s.split(".")[0].strip().upper()
    if stem in WIN_RESERVED:
        s = "_" + s
    return s


def shell_quote(b: bytes) -> str:
    """把 bytes 路径渲染成可以直接粘进 bash 的字面量。"""
    return "'" + b.decode("utf-8", "surrogateescape").replace("'", "'\\''") + "'"


def scan(root: bytes) -> list[dict]:
    """遍历 root（字节路径），收集有问题的条目。只读。"""
    found: list[dict] = []

    def check(bpath: bytes, name: bytes, kind: str) -> None:
        text, issues = classify(name)
        if not issues:
            return
        full = os.path.join(bpath, name) if bpath else name
        found.append({
            "kind": kind,
            "path_bytes": repr(full),
            "path_display": visible(full.decode("utf-8", "replace")),
            "name_display": visible(text),
            "issues": issues,
            "suggest": safe_name(text),
            "rename_cmd": "mv %s %s" % (
                shell_quote(full),
                shell_quote(os.path.join(bpath, safe_name(text).encode("utf-8", "surrogateescape"))),
            ),
        })

    if os.path.isfile(root):
        check(os.path.dirname(root), os.path.basename(root), "file")
        return found

    for bdir, bdirs, bfiles in os.walk(root):
        # 目录名先查一遍（非法字节经常在父目录里，文件名看着是干净的）
        for d in list(bdirs):
            check(bdir, d, "dir")
        for f in bfiles:
            check(bdir, f, "file")
    return found


SELF_TEST_CASES = [
    # (原始字节, 期望命中的问题)
    (b"cup.psd", []),
    ("杯垫.psd".encode("utf-8"), []),
    (b"2024_9_17 10:30 shot.psd", ["win_illegal"]),
    (b"\xff\xfe bad.psd", ["not_utf8"]),
    (b"trailing_space.psd ", ["trailing_dot"]),
    (b"trailing_dot.psd.", ["trailing_dot"]),
    (b"line\nbreak.psd", ["ctrl_char"]),
    (b"CON.psd", ["win_reserved"]),
    (b"a" * 300 + b".bin", ["too_long_bytes"]),
]


def self_test() -> int:
    bad = 0
    for name, want in SELF_TEST_CASES:
        got = sorted(classify(name)[1])
        ok = got == sorted(want)
        bad += 0 if ok else 1
        print("%s  %-30r 期望=%s 实际=%s" % ("OK  " if ok else "FAIL", name, want or "-", got or "-"))
    # 建议名必须自身干净，否则改名命令会把问题带过去
    for name, _ in SELF_TEST_CASES:
        cleaned = safe_name(name.decode("utf-8", "replace"))
        left = classify(cleaned.encode("utf-8"))[1]
        if left:
            bad += 1
            print("FAIL  建议名仍不干净：%r -> %r %s" % (name, cleaned, left))
    print("self-test：%s" % ("全部通过" if bad == 0 else "有 %d 项失败" % bad))
    return 1 if bad else 0


def main() -> int:
    ap = argparse.ArgumentParser(description="审计文件名，找出会导致拷贝失败的名字（只读）")
    ap.add_argument("paths", nargs="*", help="要检查的文件或目录（可多个）")
    ap.add_argument("--json", action="store_true", help="以 JSON 输出（方便喂给别的脚本）")
    ap.add_argument("--self-test", action="store_true", help="自检判定逻辑")
    args = ap.parse_args()

    if args.self_test:
        return self_test()
    if not args.paths:
        ap.print_help()
        return 2

    all_found: list[dict] = []
    for p in args.paths:
        b = os.fsencode(p)
        if not os.path.exists(b):
            print("路径不存在：%s" % p, file=sys.stderr)
            continue
        all_found.extend(scan(b))

    if args.json:
        print(json.dumps(all_found, ensure_ascii=False, indent=2))
        return 1 if all_found else 0

    if not all_found:
        print("✓ 没有发现名字层面的问题 —— 如果仍然报 Illegal byte sequence，")
        print("  那问题不在名字上，请检查目标盘的文件系统驱动（Paragon/Mounty 等）。")
        return 0

    counts: dict[str, int] = {}
    for item in all_found:
        for issue in item["issues"]:
            counts[issue] = counts.get(issue, 0) + 1

    print("发现 %d 个有问题的名字（目录+文件）\n" % len(all_found))
    for item in all_found:
        tag = ",".join(item["issues"])
        print("[%s] %s  %s" % (tag, item["kind"], item["path_display"]))
        for issue in item["issues"]:
            print("      · %s：%s" % (issue, ISSUE_LABEL[issue]))
        print("      原始字节：%s" % item["path_bytes"])
        print("      建议改名：%s" % item["suggest"])
        print("      可用命令：%s\n" % item["rename_cmd"])

    print("—— 汇总 ——")
    for issue, n in sorted(counts.items(), key=lambda kv: -kv[1]):
        print("  %-16s %d 个   %s" % (issue, n, ISSUE_FIX[issue]))
    print("\n本脚本未改动任何文件；确认上面的 mv 命令无误后再手动执行。")
    return 1


if __name__ == "__main__":
    sys.exit(main())
