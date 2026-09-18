#!/usr/bin/env python3
"""前端排查工具：把 .preview 预览页的 DOM dump 出来，并抓取页面控制台输出。

用途：前端事件/交互在真机上不好调试时（例如拖放事件到底有没有订阅上），
先用无头浏览器跑一遍，直接看 DOM 状态和控制台报错。

用法：
    python tools/preview_ui.py                       # 先构建一次预览目录
    python tools/debug_dom.py                        # dump 首页
    python tools/debug_dom.py "?dnd=1&x=950&y=300"   # dump 拖拽悬停态

输出：
    .preview/dom.html   完整 DOM
    stdout              [console] 开头的控制台日志
"""
import os
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import preview_ui as P  # noqa: E402

query = sys.argv[1] if len(sys.argv) > 1 else ""
P.build()
P.serve_bg()
time.sleep(0.8)

profile = tempfile.mkdtemp(prefix="cb-dbg-")
cmd = [
    P.find_chrome(),
    "--headless=new",
    "--disable-gpu",
    f"--user-data-dir={profile}",
    "--enable-logging=stderr",
    "--virtual-time-budget=6000",
    "--dump-dom",
    f"http://127.0.0.1:{P.PORT}/{query}",
]
r = subprocess.run(cmd, capture_output=True)
out = os.path.join(P.ROOT, ".preview", "dom.html")
with open(out, "w", encoding="utf-8") as f:
    f.write(r.stdout.decode("utf-8", "replace"))
for line in r.stderr.decode("utf-8", "replace").splitlines():
    if "CONSOLE" in line or "CineBackup" in line:
        print("[console]", line.strip()[:300])
print(f"[dbg] {len(r.stdout)} bytes -> {out}")
