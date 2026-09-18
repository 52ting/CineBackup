"""一键准备 Tauri 的 Windows 打包工具链（WiX + NSIS），走镜像加速。

为什么需要这个脚本
------------------
tauri 生成 .msi / .exe 安装包前，会从 GitHub Release 下载两套外部工具：

  1. WiX 3.14            → 生成 .msi
  2. NSIS 3.11 + 插件 dll → 生成 NSIS setup.exe

本机直连 GitHub Release 资产吞吐仅约 176 KB/s，而 tauri 内置的 HTTP 客户端
约 1 分钟就放弃，报 `failed to bundle project: timeout: global`。
本脚本用国内可用的 GitHub 镜像（实测 8.5 MB/s）把工具链预先下好并解压到
tauri 期望的缓存目录，之后 `npm run tauri build` 即可直接复用、不再联网下载。

缓存位置（Windows）
------------------
  %LOCALAPPDATA%\\tauri\\WixTools314\\   —— 需直接包含 candle.exe / light.exe / wix.dll ...
  %LOCALAPPDATA%\\tauri\\NSIS\\nsis-3.11\\ —— 需包含 makensis.exe / Bin/ / Include/ / Stubs/ ...
"""
import hashlib
import os
import shutil
import ssl
import sys
import time
import urllib.request
import zipfile

# ----------------------------------------------------------------- 下载源定义

GH = "https://github.com"
MIRRORS = ["https://gh-proxy.com/", "https://ghfast.top/", "https://ghproxy.net/", ""]

WIX_ZIP = f"{GH}/wixtoolset/wix3/releases/download/wix3141rtm/wix314-binaries.zip"
WIX_SHA256 = "6ac824e1642d6f7277d0ed7ea09411a508f6116ba6fae0aa5f2c7daa2ff43d31"

NSIS_ZIP = f"{GH}/tauri-apps/binary-releases/releases/download/nsis-3.11/nsis-3.11.zip"
NSIS_SHA1 = "EF7FF767E5CBD9EDD22ADD3A32C9B8F4500BB10D"

NSIS_UTILS_DLL = (
    f"{GH}/tauri-apps/nsis-tauri-utils/releases/download/"
    "nsis_tauri_utils-v0.5.3/nsis_tauri_utils.dll"
)
NSIS_UTILS_SHA1 = "75197FEE3C6A814FE035788D1C34EAD39349B860"

CACHE = os.path.join(os.path.expandvars(r"%LOCALAPPDATA%"), "tauri")
VENDOR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "_vendor")

WIX_REQUIRED = [
    "candle.exe", "light.exe", "wix.dll",
    "WixUIExtension.dll", "WixUtilExtension.dll", "winterop.dll", "wconsole.dll",
]
NSIS_REQUIRED = [
    "makensis.exe",
    os.path.join("Bin", "makensis.exe"),
    os.path.join("Include", "MUI2.nsh"),
]


def human(n):
    for u in ("B", "KB", "MB", "GB"):
        if n < 1024:
            return f"{n:.1f} {u}"
        n /= 1024
    return f"{n:.1f} TB"


def digest(path, algo):
    h = hashlib.new(algo)
    with open(path, "rb") as f:
        for blk in iter(lambda: f.read(1 << 20), b""):
            h.update(blk)
    return h.hexdigest()


def fetch(url, dest, algo, expect, retries=2):
    """按镜像顺序尝试下载，校验通过才返回。已存在且校验通过则直接复用。"""
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    if os.path.exists(dest) and digest(dest, algo).lower() == expect.lower():
        print(f"    已缓存且校验通过 ({human(os.path.getsize(dest))})")
        return dest

    for m in MIRRORS:
        for attempt in range(1, retries + 1):
            full = m + url
            label = m or "直连 github.com"
            try:
                t0 = time.time()
                req = urllib.request.Request(full, headers={"User-Agent": "Mozilla/5.0"})
                ctx = ssl.create_default_context()
                tmp = dest + ".part"
                got = 0
                with urllib.request.urlopen(req, timeout=60, context=ctx) as r, open(tmp, "wb") as f:
                    while True:
                        chunk = r.read(1 << 18)
                        if not chunk:
                            break
                        f.write(chunk)
                        got += len(chunk)
                        if time.time() - t0 > 90:
                            raise TimeoutError("总时长超限")
                if got == 0:
                    raise IOError("收到 0 字节")
                os.replace(tmp, dest)
                sp = got / max(time.time() - t0, 1e-6)
                print(f"    {label}: {human(got)} / {time.time()-t0:.1f}s = {human(sp)}/s")
                if digest(dest, algo).lower() == expect.lower():
                    print("      ✓ 校验通过")
                    return dest
                print("      ✗ 校验失败，换镜像")
                os.remove(dest)
            except Exception as e:
                print(f"    {label} 第{attempt}次失败: {type(e).__name__}: {e}")
                p = dest + ".part"
                if os.path.exists(p):
                    try:
                        os.remove(p)
                    except OSError:
                        pass
                time.sleep(1)
    raise SystemExit(f"下载失败: {url}")


def extract_zip(zip_path, dest_dir):
    if os.path.isdir(dest_dir):
        shutil.rmtree(dest_dir)
    os.makedirs(dest_dir, exist_ok=True)
    with zipfile.ZipFile(zip_path) as z:
        z.extractall(dest_dir)
    return dest_dir


# --------------------------------------------------------------------- WiX

def setup_wix():
    print()
    print("=" * 66)
    print("WiX 3.14  → 生成 .msi")
    print("=" * 66)
    zip_path = os.path.join(VENDOR, "wix314-binaries.zip")
    print("  [下载]")
    fetch(WIX_ZIP, zip_path, "sha256", WIX_SHA256)

    print("  [解压]")
    stage = extract_zip(zip_path, os.path.join(VENDOR, "WixTools314"))
    missing = [f for f in WIX_REQUIRED if not os.path.exists(os.path.join(stage, f))]
    print(f"    {len(os.listdir(stage))} 项，必需文件{'齐全' if not missing else '缺失 ' + str(missing)}")

    print("  [部署]")
    target = os.path.join(CACHE, "WixTools314")
    if os.path.isdir(target):
        shutil.rmtree(target)
    shutil.copytree(stage, target)
    shutil.copy2(zip_path, os.path.join(CACHE, "wix314-binaries.zip"))
    n = sum(len(f) for _, _, f in os.walk(target))
    print(f"    → {target}  ({n} 文件)")


# -------------------------------------------------------------------- NSIS

def setup_nsis():
    print()
    print("=" * 66)
    print("NSIS 3.11  → 生成 setup.exe（可选，便携安装包）")
    print("=" * 66)
    zip_path = os.path.join(VENDOR, "nsis-3.11.zip")
    print("  [下载]")
    fetch(NSIS_ZIP, zip_path, "sha1", NSIS_SHA1)

    print("  [解压]")
    # nsis-3.11.zip 内部顶层就是 nsis-3.11/，直接解到 NSIS 目录下即可
    nsis_root = os.path.join(CACHE, "NSIS")
    extract_zip(zip_path, nsis_root)
    inner = os.path.join(nsis_root, "nsis-3.11")
    use = inner if os.path.isdir(inner) else nsis_root
    print(f"    实际根目录: {use}")
    for f in NSIS_REQUIRED:
        print(f"      {'✓' if os.path.exists(os.path.join(use, f)) else '✗'} {f}")

    print("  [下载插件 dll]")
    dll = os.path.join(VENDOR, "nsis_tauri_utils.dll")
    fetch(NSIS_UTILS_DLL, dll, "sha1", NSIS_UTILS_SHA1)
    plug = os.path.join(use, "Plugins", "x86-unicode", "additional")
    os.makedirs(plug, exist_ok=True)
    shutil.copy2(dll, os.path.join(plug, "nsis_tauri_utils.dll"))
    print(f"    → {os.path.join(plug, 'nsis_tauri_utils.dll')}")


def main():
    only = sys.argv[1] if len(sys.argv) > 1 else "all"
    print(f"缓存根目录: {CACHE}")
    if only in ("all", "wix"):
        setup_wix()
    if only in ("all", "nsis"):
        setup_nsis()
    print()
    print("=" * 66)
    print("就绪。现在可以执行： npm run tauri build")
    print("=" * 66)


if __name__ == "__main__":
    main()
