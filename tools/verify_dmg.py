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
# ⚠️ 必须和 watch_ci.py 的 OUTDIR 一致：产物落在**仓库同级**的 cinebackup-builds/。
# 早先这里多写了一层 os.pardir，指到了「工作区根」那个同名目录 ——
# 那里躺的是很早以前的一对陈货，于是 `verify_dmg.py` 不带参数时会一脸正经地
# 验出一个几个月前的 dmg 并报「全部通过」。
BUILDS = os.path.join(os.path.dirname(os.path.abspath(__file__)), os.pardir, "cinebackup-builds")
LEGACY_BUILDS = os.path.join(ROOT, os.pardir, "cinebackup-builds")

CPU_TYPES = {0x01000007: "x86_64 (Intel)", 0x0100000C: "arm64 (Apple Silicon)"}


def builds_dir():
    """优先项目下的 cinebackup-builds/（watch_ci 的落点），没有才退回工作区根那个。"""
    d = os.path.abspath(BUILDS)
    if os.path.isdir(d) and any(f.endswith((".zip", ".dmg")) for f in os.listdir(d)):
        return d
    legacy = os.path.abspath(LEGACY_BUILDS)
    if os.path.isdir(legacy):
        print("      ⚠ 项目下没有产物，改用 %s（可能是旧版本，注意版本号）" % legacy)
        return legacy
    sys.exit("找不到产物目录 %s，先跑 `python tools/watch_ci.py watch`" % d)


def newest_dmg():
    d = builds_dir()
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


def plist_fields(raw_text):
    """Info.plist（XML 或二进制 plist）里直接搜关键字段，不做完整解析。"""
    txt = raw_text.decode("utf-8", "replace") if isinstance(raw_text, bytes) else raw_text
    pats = {
        "标识符": (r"com\.[A-Za-z0-9._-]*cinebackup[A-Za-z0-9._-]*", 0),
        "版本": (r"CFBundleShortVersionString</key>\s*<string>([^<]+)", 1),
        "最低系统": (r"LSMinimumSystemVersion</key>\s*<string>([^<]+)", 1),
    }
    out = {}
    for label, (pat, grp) in pats.items():
        m = re.search(pat, txt)
        out[label] = (m.group(grp) if grp else m.group(0)) if m else "(未找到)"
    return out


def check_plist(app_dir):
    pl = os.path.join(app_dir, "Contents", "Info.plist")
    if not os.path.isfile(pl):
        print("      ⚠ 找不到 Info.plist：%s" % pl)
        return False
    print("[plist] %s" % pl)
    for k, v in plist_fields(open(pl, "rb").read()).items():
        print("      %-8s: %s" % (k, v))
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
    """watch_ci.py 下载的是产物 zip（里面才是 dmg + app），顺手解开验一遍。

    返回整体是否通过 —— 早先这个返回值是丢掉的，于是「是不是 universal」
    哪怕验失败，main() 也照样打印「✅ 全部通过」，等于白验。
    """
    ok = True
    with zipfile.ZipFile(zip_path) as z:
        names = z.namelist()
        print("[zip] %s  条目 %d" % (os.path.basename(zip_path), len(names)))
        dmgs = [n for n in names if n.endswith(".dmg")]
        exes = [n for n in names if n.endswith("/MacOS/cinebackup")]
        if dmgs:
            out = os.path.join(os.path.dirname(zip_path), os.path.basename(dmgs[0]))
            # 旁边已经有同名 dmg（watch_ci 已解过）就别重复写一遍
            if not (os.path.isfile(out) and os.path.getsize(out) == z.getinfo(dmgs[0]).file_size):
                with open(out, "wb") as f:
                    f.write(z.read(dmgs[0]))
                print("      → 已解出 %s" % out)
            ok = check_dmg(out) and ok
        else:
            print("      ✗ zip 里没有 .dmg")
            ok = False
        if exes:
            data = z.read(exes[0])
            tmp = os.path.join(os.path.dirname(zip_path), "_macho_probe.bin")
            with open(tmp, "wb") as f:
                f.write(data)
            try:
                ok = check_macho(tmp, "可执行文件") and ok
            finally:
                os.remove(tmp)
        else:
            print("      ✗ zip 里找不到 Contents/MacOS/cinebackup")
            ok = False
        plists = [n for n in names if n.endswith("Contents/Info.plist")]
        if plists:
            print("[plist] zip 内 %s" % plists[0])
            for k, v in plist_fields(z.read(plists[0])).items():
                print("      %-8s: %s" % (k, v))
        else:
            print("      ✗ zip 里找不到 Info.plist")
            ok = False
    return ok


def sibling_zip(path):
    """产物 zip 和 dmg 是同一个 workflow artifact 里的，找一个同目录的 zip。"""
    d = os.path.dirname(os.path.abspath(path))
    for f in sorted(os.listdir(d)):
        if f.endswith(".zip"):
            return os.path.join(d, f)
    return None


def main():
    args = sys.argv[1:]
    if args:
        target = args[0]
    else:
        # 优先最新 zip（里面 .app / dmg 都在，能验全），否则最新 dmg
        d = builds_dir()
        cands = [os.path.join(d, f) for f in os.listdir(d) if f.endswith((".zip", ".dmg"))]
        if not cands:
            target = newest_dmg()
        else:
            target = max(cands, key=os.path.getmtime)
    print("== 校验 %s ==" % target)
    if target.endswith(".dmg"):
        ok = check_dmg(target)
        # dmg 容器本身验不出 .app —— 够不到里面的 FAT 与 Info.plist。
        # 同一个 artifact 里的 zip 才是能读进 .app 的那个，顺手一起验。
        z = sibling_zip(target)
        if z:
            print()
            ok = verify_zip(z) and ok
        else:
            print("      ⚠ 同目录没有产物 zip  →  只能验 dmg 容器，验不到里面的 .app"
                  "（想验 FAT / Info.plist 就把 artifact zip 一起拉下来）")
    elif target.endswith(".zip"):
        ok = verify_zip(target)
    elif target.endswith(".app") or os.path.isdir(target):
        ok = verify_app_dir(target)
    else:
        sys.exit("不认识的目标：%s（支持 .dmg / .zip / .app 目录）" % target)
    print("\n%s" % ("✅ 全部通过" if ok else "❌ 有检查项未通过"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
