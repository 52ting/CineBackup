import os, shutil

root = r"C:\Users\Ronnie\WorkBuddy\2026-09-18-14-51-29\cinebackup"

print("=== 项目文件 ===")
n = 0
for dp, dn, fn in os.walk(root):
    n += len(fn)
print("文件总数:", n, "| 目录存在:", os.path.isdir(root))

print()
print("=== 工具链 ===")
for c in ["cargo", "rustc", "node", "npm", "pnpm", "yarn", "winget", "rustup", "cmake", "cl"]:
    print("%-8s -> %s" % (c, shutil.which(c)))

print()
print("=== 常见安装位置 ===")
for p in [
    r"C:\Users\Ronnie\.cargo\bin\cargo.exe",
    r"C:\Users\Ronnie\.rustup",
    r"C:\Program Files\nodejs\npm.cmd",
    r"C:\Program Files (x86)\Microsoft Visual Studio",
    r"C:\Program Files\Microsoft Visual Studio",
]:
    print(("存在  " if os.path.exists(p) else "不存在"), p)

print()
print("=== 磁盘可用空间 ===")
for d in ["C:\\", "D:\\", "E:\\"]:
    try:
        t, u, f = shutil.disk_usage(d)
        print(d, "free %.1f GB / total %.1f GB" % (f / 2**30, t / 2**30))
    except Exception:
        print(d, "N/A")
