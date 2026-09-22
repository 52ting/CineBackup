"""把 cinebackup 源码打成可直接搬到 Mac 的 zip（排除构建产物与日志）。

用法：
    python tools/make_src_zip.py            # 输出到项目同级目录
    python tools/make_src_zip.py D:\\out    # 指定输出目录
"""
import os
import sys
import json
import zipfile

PROJ = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def version() -> str:
    """从 package.json 读版本号。

    别在这里硬编码 —— 之前写死 0.4.0，结果 0.4.2 出包时文件名还是 0.4.0。
    package.json 是前端、Cargo.toml、tauri.conf.json 三处里最先改的那个，
    拿它当单一来源最省心。
    """
    with open(os.path.join(PROJ, "package.json"), encoding="utf-8") as f:
        return json.load(f)["version"]


VERSION = version()

# 目录名：整棵子树都不进包
SKIP_DIRS = {
    "node_modules",
    "target",
    ".preview",
    "dist",
    "gen",          # src-tauri/gen（tauri 生成的 schema）
    ".git",
    "__pycache__",
    # watch_ci.py 下载云端产物的地方。曾经漏在旁边，结果 0.4.2 的源码包
    # 被塞进 18 MB 的 dmg/zip（总共才 79 个文件、19.8 MB）—— 已经踩过一次。
    "cinebackup-builds",
}
# 文件后缀 / 文件名：不进包
SKIP_SUFFIX = (".log", ".tmp", ".dmg", ".msi")
SKIP_NAME_PREFIX = ("vite.config.js.timestamp-",)


def keep(fname: str) -> bool:
    if fname.endswith(SKIP_SUFFIX):
        return False
    if fname.startswith(SKIP_NAME_PREFIX):
        return False
    if fname.endswith(".cinebackup.json"):
        return False
    return True


def main() -> int:
    out_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(PROJ)
    os.makedirs(out_dir, exist_ok=True)
    name = f"cinebackup-{VERSION}-src.zip"
    out = os.path.join(out_dir, name)

    files = []
    for root, dirs, names in os.walk(PROJ):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for n in names:
            if keep(n):
                files.append(os.path.join(root, n))

    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for p in sorted(files):
            rel = os.path.relpath(p, PROJ).replace(os.sep, "/")
            info = zipfile.ZipInfo(rel, date_time=(2026, 9, 18, 12, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            # 让 .sh 在 macOS / Linux 上自带可执行位
            mode = 0o755 if rel.endswith(".sh") else 0o644
            info.external_attr = mode << 16
            with open(p, "rb") as f:
                z.writestr(info, f.read())

    size = os.path.getsize(out)
    print(f"已生成 {out}")
    print(f"  文件数 {len(files)}，压缩后 {size / 1024:.1f} KB")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
