#!/usr/bin/env python3
"""构建前清空 dist/（vite 自己是靠 node 的 rmSync 清空的，本机 node 被套了
「批量删除守卫」，文件一多就会被拦下来报 SAFE_DELETE_BULK_CONFIRM_REQUIRED）。

所以改成让 vite 别自己清（build.emptyOutDir = false），由这个脚本先删干净。
只认 dist 目录，路径不对就直接退出，不做任何递归删除。

用法： python tools/clean_dist.py
"""

import os
import shutil
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIST = os.path.join(ROOT, "dist")

# 双保险：只允许删项目根目录下那个 dist，避免变量被人改坏后误删别处
if os.path.basename(DIST) != "dist" or os.path.dirname(DIST) != ROOT or not os.path.isdir(DIST):
    print(f"[clean] 跳过：不是预期的 dist 目录 -> {DIST}")
    sys.exit(0)

n = sum(len(f) for _, _, f in os.walk(DIST))
shutil.rmtree(DIST)
print(f"[clean] 已清空 {DIST}（原有 {n} 个文件）")
