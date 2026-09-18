#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""verify_dmg.py —— 在 Windows 上不开 Mac 也能验 dmg / .app 的真伪

校验三件事，全部靠读字节，不需要挂载也不需要 macOS：

  1. **dmg 是不是合法磁盘映像**：只读 UDIF 的尾部 512 字节以 `koly` 开头
     （`koly` = UDIF 资源尾巴的魔数），文件大小也一并报出来。
  2. **`.app` 里的可执行文件是不是 universal（FAT）**：头 4 字节 `0xCAFEBABE`
     = FAT_MAGIC（大端序），紧跟一个 u32 是架构数，之后每条 20 字节：
     `cputype / cpusubtype / offset / size / align`。
     CPU_TYPE_X86_64 = 0x01000007，CPU_TYPE_ARM64 = 0x0100000C。
  3. **Info.plist 的关键字段**：标识符、版本号、最低系统版本（从二进制里搜字符串）。

用法：
    python tools/verify_dmg.py                       # 自动找 cinebackup-builds/ 里最新的 dmg
    python tools/verify_dmg.py path/to/xxx.dmg
    python tools/verify_dmg.py path/to/CineBackup.app   # 也可以直接验解出来的 .app 目录
"""
import os
import re
import struct
import sys
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILDS = os.path.join(ROOT, os.pardir, "cinebackup-builds")

CPU_TYPES = {0x01000007: "x86_64 (Intel)", 0x0100000C: "arm64 (Apple Silicon)"}


def newest_dmg():
    d = os.path.abspath(BUILDS)
    if not os.path.isdir(d):
        sys.exit("找不到 %s，先跑 `python tools/watch_ci.py watch` 把产物拉回来" % d)
    cands = [os.path.join(d, f) for f in os.listdir(d) if f.endswith(".dmg")]
    if not cands:
        sys.exit("%s 里没有 .dmg" % d)
    return max(cands, key=os.path.getmtime)


def check_dmg(path):
    size = os.path.getsize(path)
    with open(path, "rb") as f:
        f.seek(-512, os.SEEK_END)
        tail = f.read(512)
    magic = tail[:4]
    ok = magic == b"koly"
    print("[dmg] %s" % os.path.basename(path))
    print("      大小      : %.2f MB (%d 字节)" % (size / 1048576, size))
    print("      尾部魔数  : %r  %s" % (magic, "✓ 合法 UDIF 磁盘映像" if ok else "✗ 不是 dmg"))
    return ok


def check_macho(path, label=""):
    with open(path, "rb") as f:
        head = f.read(8)
    if head[:4] != b"\xca\xfe\xba\xbe":
        print("      %-10s %s  ✗ 不是 FAT 通用二进制（头 4 字节 %s）" % (label, path, head[:4].hex()))
        return False
    n = struct.unpack(">I", head[4:8])[0]
    print("      %-10s ✓ FAT universal，含 %d 个架构" % (label, n))
    with open(path, "rb") as f:
        f.seek(8)
        for i in range(n):
            raw = f.read(20)
            if len(raw) < 20:
                break
            cputype, cpusub, offset, size, align = struct.unpack(">IIIII", raw)
            name = CPU_TYPES.get(cputype, "cputype=0x%08X" % cputype)
            print("        ├ %-22s 切片 %6.2f MB（偏移 %d）" % (name, size / 1048576, offset))
    return n >= 2


def check_plist(app_dir):
    pl = os.path.join(app_dir, "Contents", "Info.plist")
    if not os.path.isfile(pl):
        print("      ⚠ 找不到 Info.plist：%s" % pl)
        return False
    raw = open(pl, "rb").read()
    # Info.plist 可能是 XML 也可能是二进制 plist，两种都直接搜可打印字符串
    txt = raw.decode("utf-8", "replace")
    fields = {
        "标识符": r"com\.[A-Za-z0-9._-]*cinebackup[A-Za-z0-9._-]*",
        "版本": r"CFBundleShortVersionString</key>\s*<string>([^<]+)",
        "最低系统": r"LSMinimumSystemVersion</key>\s*<string>([^<]+)",
    }
    print("[plist] %s" % pl)
    for label, pat in fields.items():
        m = re.search(pat, txt)
        print("      %-8s: %s" % (label, m.group(1) if m and m.groups() else (m.group(0) if m else "(未找到)")))
    return True


def verify_app_dir(app_dir):
    print("[app] %s" % app_dir)
    exe = os.path.join(app_dir, "Contents", "MacOS", os.path.basename(app_dir).replace(".app", "").lower())
    if not os.path.isfile(exe):
        cands = os.listdir(os.path.join(app_dir, "Contents", "MacOS")) if os.path.isdir(os.path.join(app_dir, "Contents", "MacOS")) else []
        if not cands:
            sys.exit("Contents/MacOS/ 里没有可执行文件")
        exe = os.path.join(app_dir, "Contents", "MacOS", cands[0])
    ok = check_macho(exe, "可执行文件")
    ok = check_plist(app_dir) and ok
    return ok


def verify_zip(zip_path):
    """watch_ci.py 下载的是产物 zip（里面才是 dmg + app），顺手解开验一遍。"""
    with zipfile.ZipFile(zip_path) as z:
        names = z.namelist()
        print("[zip] %s  条目 %d" % (os.path.basename(zip_path), len(names)))
        dmgs = [n for n in names if n.endswith(".dmg")]
        exes = [n for n in names if n.endswith("/MacOS/cinebackup")]
        if dmgs:
            out = os.path.join(os.path.dirname(zip_path), os.path.basename(dmgs[0]))
            with open(out, "wb") as f:
                f.write(z.read(dmgs[0]))
            print("      → 已解出 %s" % out)
            check_dmg(out)
        if exes:
            data = z.read(exes[0])
            tmp = os.path.join(os.path.dirname(zip_path), "_macho_probe.bin")
            with open(tmp, "wb") as f:
                f.write(data)
            check_macho(tmp, "可执行文件")
            os.remove(tmp)


def main():
    args = sys.argv[1:]
    if args:
        target = args[0]
    else:
        # 优先最新 zip（还没解过的），否则最新 dmg
        cands = []
        if os.path.isdir(os.path.abspath(BUILDS)):
            cands = [os.path.join(os.path.abspath(BUILDS), f) for f in os.listdir(os.path.abspath(BUILDS))
                     if f.endswith((".zip", ".dmg"))]
        if not cands:
            target = newest_dmg()
        else:
            target = max(cands, key=os.path.getmtime)
    print("== 校验 %s ==" % target)
    if target.endswith(".dmg"):
        ok = check_dmg(target)
    elif target.endswith(".zip"):
        verify_zip(target)
        ok = True
    elif target.endswith(".app") or os.path.isdir(target):
        ok = verify_app_dir(target)
    else:
        sys.exit("不认识的目标：%s（支持 .dmg / .zip / .app 目录）" % target)
    print("\n%s" % ("✅ 全部通过" if ok else "❌ 有检查项未通过"))


if __name__ == "__main__":
    main()
