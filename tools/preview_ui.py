#!/usr/bin/env python3
"""CineBackup UI 预览工具（无需 Rust 工具链）

把 dist/ 复制到 .preview/ 并注入一份「假后端」，用真实 Chrome 无头截图。
改完前端样式后，可以秒级核对布局，不用等 cargo build。

用法：
    python tools/preview_ui.py              # 构建预览目录
    python tools/preview_ui.py --serve      # 构建 + 起 http 服务（Ctrl-C 结束）

截图（全自动：构建 + 起服务 + Chrome 无头截图）：
    python tools/preview_ui.py --shot                          # 空闲态（跟随系统主题）
    python tools/preview_ui.py --shot --theme dark              # 强制深色
    python tools/preview_ui.py --shot --theme light             # 强制浅色
    python tools/preview_ui.py --shot --run --theme dark        # 任务运行中（传输列表）
    python tools/preview_ui.py --shot --dnd --dnd-x 1150 --dnd-y 300   # 拖拽悬停态

预览页 URL 参数（浏览器里也能手动看效果）：
    ?theme=dark|light   强制主题
    ?run=1              任务运行中态（中栏换成每个源一条的传输进度）
    ?dnd=1&x=&y=        拖拽悬停态
"""

import argparse
import http.server
import os
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIST = os.path.join(ROOT, "dist")
PREVIEW = os.path.join(ROOT, ".preview")
PORT = 8765

CHROME_CANDIDATES = [
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "google-chrome",
]

# 假磁盘：贴近真机（3 块内置 + 1 块映射网络盘 + 1 块剩余为 0 的掉线网络盘）
MOCK_DISKS = [
    ("C:", "C:\\", "", "NTFS", "fixed", 789767405568, 124665937920, True),
    ("D:", "D:\\", "", "NTFS", "fixed", 209714147328, 113871224832, True),
    ("E:", "E:\\", "新加卷", "NTFS", "fixed", 16000881782784, 10582813462528, True),
    ("Z:", "Z:\\", "视频素材盘", "NTFS", "network", 7676309151744, 1523075424256, True),
    ("Y:", "Y:\\", "超能造2", "NTFS", "network", 3835577597952, 0, True),
]

MOCK_JS = """// ==== 预览用假后端（只在 .preview/ 里存在，不进产物） ====
(function () {
  const DISKS = __DISKS__;

  // 每次打开"选择对话框"依次返回这些路径
  const DIRS = [
    "D:\\\\拍摄素材\\\\《山海》A001_20260901",
    "E:\\\\DIT\\\\R3D_RAW\\\\Day03",
    "Z:\\\\DCP\\\\shanhai_final_dcp.zip",
    "E:\\\\CineBackup\\\\2026-09-18",
  ];
  const FILES = ["Z:\\\\DCP\\\\shanhai_final_dcp.zip"];
  let dirIdx = 0;
  let fileIdx = 0;

  const snap = (p) => ({
    path: p,
    kind: /\\.[A-Za-z0-9]{1,8}$/.test(p) ? "file" : "dir",
    fs: "NTFS",
    exists: true,
    size: /\\.[A-Za-z0-9]{1,8}$/.test(p) ? 26843545600 : 0,
  });

  let cbid = 0;
  // 记录前端订阅过的事件，便于手动投递（模拟拖拽）
  const listeners = {};
  window.__TAURI_INTERNALS__ = {
    // getCurrentWindow / getCurrentWebview 会读这里，缺了会在监听前就抛错
    metadata: {
      currentWindow: { label: "main" },
      currentWebview: { label: "main" },
    },
    transformCallback(cb) {
      const id = ++cbid;
      window["_" + id] = cb;
      return id;
    },
    invoke(cmd, args) {
      args = args || {};
      if (cmd === "list_disks") return Promise.resolve(DISKS);
      if (cmd === "fs_type") return Promise.resolve("NTFS");
      if (cmd === "free_space") return Promise.resolve(10582813462528);
      if (cmd === "is_busy") return Promise.resolve(false);
      if (cmd === "probe_path") return Promise.resolve(snap(args.path));
      if (cmd === "plugin:event|listen") {
        listeners[args.event] = args.handler;
        return Promise.resolve(1);
      }
      if (cmd === "plugin:event|unlisten") return Promise.resolve(null);
      if (cmd === "plugin:dialog|open") {
        const o = args.options || {};
        if (o.directory) return Promise.resolve(DIRS[dirIdx++ % DIRS.length]);
        const one = FILES[fileIdx++ % FILES.length];
        return Promise.resolve(o.multiple ? [one] : one);
      }
      return Promise.resolve(null);
    },
  };

  // 手动投递一个 tauri 事件（模拟后端推送）
  window.__cbEmit = function (event, payload) {
    const id = listeners[event];
    const cb = id && window["_" + id];
    if (typeof cb === "function") cb({ event, id, payload });
  };
  // 模拟系统拖拽：type = enter / over / drop / leave
  window.__cbDnd = function (type, paths, x, y) {
    window.__cbEmit("tauri://drag-" + type, { paths: paths, position: { x: x, y: y } });
  };

  // 想看的拖拽路径（真机上任你拖，这里写死几条片子味儿的）
  const DROP_PATHS = [
    "E:\\\\拍摄素材\\\\《山海》A001_20260901",
    "E:\\\\拍摄素材\\\\《山海》A002_20260902",
    "Z:\\\\DCP\\\\shanhai_final_dcp.zip",
    "D:\\\\PROXY\\\\day03.mov",
    "D:\\\\PROXY\\\\day04.mov",
  ];

  const qs = new URLSearchParams(location.search);

  // ?theme=dark|light → 强制主题（不传则跟随系统）
  if (qs.has("theme")) document.documentElement.dataset.theme = qs.get("theme");

  // ?run=1 → 模拟「任务运行中」：中栏应折叠磁盘、换成每个源一条的传输列表
  if (qs.has("run")) {
    setTimeout(function () {
      window.__cbEmit("cb:status", { status: "copying" });
      window.__cbEmit("cb:plan", {
        copy: 128, resume: 3, skip: 6, overwrite: 1, conflict: 0,
        totalBytes: 707724077016, filtered: 2,
      });
      const SRC = [
        { index: 0, path: "D:\\\\拍摄素材\\\\《山海》A001_20260901",
          bytesTotal: 412316860416, bytesDone: 412316860416,
          filesTotal: 96, filesDone: 96, state: "done", currentFile: "" },
        { index: 1, path: "E:\\\\DIT\\\\R3D_RAW\\\\Day03",
          bytesTotal: 268435456000, bytesDone: 123456789000,
          filesTotal: 142, filesDone: 58, state: "active",
          currentFile: "E:\\\\DIT\\\\R3D_RAW\\\\Day03\\\\A003_C012_0712AB.R3D" },
        { index: 2, path: "Z:\\\\DCP\\\\shanhai_final_dcp.zip",
          bytesTotal: 26843545600, bytesDone: 0,
          filesTotal: 1, filesDone: 0, state: "waiting", currentFile: "" },
      ];
      const push = function () {
        window.__cbEmit("cb:progress", {
          phase: "copy", filesTotal: 239, filesDone: 154,
          bytesTotal: 707724077016, bytesDone: 535803646416,
          speedBps: 503316480, etaSecs: 341, currentFile: SRC[1].currentFile,
          currentFileDone: 0, currentFileTotal: 0, elapsedSecs: 184, sources: SRC,
        });
      };
      push();
      setInterval(push, 600);
    }, 2600);
  }

  // ?dnd=1&x=960&y=300 → 自动进入拖拽悬停态，好截拖拽遮罩
  if (qs.has("dnd")) {
    const x = Number(qs.get("x") || 320);
    const y = Number(qs.get("y") || 300);
    setTimeout(() => window.__cbDnd("enter", DROP_PATHS, x, y), 1500);
    // 真机上鼠标移动会持续派发 over，这里也持续发，否则会被遮罩的看门狗收掉
    setInterval(() => window.__cbDnd("over", DROP_PATHS, x, y), 250);
  }


  // 自动演一遍：挑三个源、选一个目标，好让截图是有内容的状态
  // （?run=1 时同样先铺数据，否则左栏空着、传输列表里的「目标」也没有名字）
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  window.addEventListener("DOMContentLoaded", async () => {
    await sleep(400);
    const cards = () => document.querySelectorAll("#diskGrid .disk-card");
    const menuBtn = (act) =>
      document.querySelector('.disk-menu button[data-act="' + act + '"]');
    for (const i of [0, 1, 3]) {
      cards()[i]?.click();
      await sleep(170);
      menuBtn("browse")?.click();
      await sleep(280);
    }
    // 第 3 块盘 → 设为目标
    cards()[2]?.click();
    await sleep(170);
    menuBtn("target")?.click();
    // ?run=1 → 再按下「开始备份」，界面就会切到传输视图
    if (qs.has("run")) {
      await sleep(220);
      document.getElementById("btnStart")?.click();
    }
  });
})();
"""


def build():
    if not os.path.isdir(DIST):
        sys.exit("找不到 dist/，先跑 `npm run build`")
    # 增量同步，不用 shutil.rmtree：本机宿主对「批量删除」有配额限制（一个回合 50 个文件），
    # 而 .preview 一次就有上百个文件，整目录重建会把配额用光导致后面的构建失败。
    os.makedirs(PREVIEW, exist_ok=True)
    for name in os.listdir(DIST):
        src = os.path.join(DIST, name)
        dst = os.path.join(PREVIEW, name)
        if os.path.isdir(src):
            shutil.copytree(src, dst, dirs_exist_ok=True)
            # 顺手清掉该目录下已经不存在于 dist 的旧文件（hash 文件名会变，一般就 2 个）
            for root, _dirs, files in os.walk(dst):
                rel = os.path.relpath(root, dst)
                for f in files:
                    if not os.path.exists(os.path.join(src, rel, f)):
                        os.remove(os.path.join(root, f))
        else:
            shutil.copy2(src, dst)

    idx = os.path.join(PREVIEW, "index.html")
    html = open(idx, encoding="utf-8").read()
    if "mock-backend.js" not in html:
        html = html.replace(
            "</head>", '  <script src="/mock-backend.js"></script>\n  </head>'
        )
        open(idx, "w", encoding="utf-8").write(html)

    import json

    disks = [
        {
            "id": d[0],
            "mount": d[1],
            "label": d[2],
            "fs": d[3],
            "kind": d[4],
            "total": d[5],
            "free": d[6],
            "writable": d[7],
        }
        for d in MOCK_DISKS
    ]
    js = MOCK_JS.replace("__DISKS__", json.dumps(disks, ensure_ascii=False))
    open(os.path.join(PREVIEW, "mock-backend.js"), "w", encoding="utf-8").write(js)
    print(f"[preview] 已生成 {PREVIEW}")


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=PREVIEW, **kw)

    def log_message(self, *a):
        pass


def serve_bg():
    socketserver.TCPServer.allow_reuse_address = True
    httpd = socketserver.TCPServer(("127.0.0.1", PORT), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    print(f"[preview] http://127.0.0.1:{PORT}/")
    return httpd


def find_chrome():
    for c in CHROME_CANDIDATES:
        if os.path.exists(c) or shutil.which(c):
            return c
    sys.exit("找不到 Chrome / Edge，请手动打开上面的地址截图")


def shot(out, width=1560, height=1000, wait_ms=6000, query=""):
    chrome = find_chrome()
    # Chrome 的 --screenshot 是相对它自己的 cwd 解析的，统一转成绝对路径
    out = os.path.abspath(out)
    os.makedirs(os.path.dirname(out), exist_ok=True)
    if os.path.exists(out):
        os.remove(out)
    profile = tempfile.mkdtemp(prefix="cb-preview-")
    cmd = [
        chrome,
        "--headless=new",
        "--disable-gpu",
        "--hide-scrollbars",
        "--force-device-scale-factor=1",
        f"--window-size={width},{height}",
        f"--virtual-time-budget={wait_ms}",
        f"--user-data-dir={profile}",
        f"--screenshot={out}",
        f"http://127.0.0.1:{PORT}/{query}",
    ]
    r = subprocess.run(cmd, capture_output=True)
    shutil.rmtree(profile, ignore_errors=True)
    if os.path.exists(out):
        print(f"[preview] 截图已保存 {out} ({os.path.getsize(out)} B)")
    else:
        print(r.stdout.decode("utf-8", "replace")[-800:])
        print(r.stderr.decode("utf-8", "replace")[-800:])
        sys.exit("截图失败")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--serve", action="store_true")
    ap.add_argument("--shot", action="store_true")
    ap.add_argument("--out", default="")
    ap.add_argument("--width", type=int, default=1560)
    ap.add_argument("--height", type=int, default=1000)
    ap.add_argument(
        "--dnd",
        action="store_true",
        help="截「拖拽悬停」态（显示投放遮罩），配合 --dnd-x / --dnd-y 指定悬停位置",
    )
    ap.add_argument("--dnd-x", type=int, default=320, help="拖拽悬停点 x（左栏约 300，右栏约 1150）")
    ap.add_argument("--dnd-y", type=int, default=300)
    ap.add_argument(
        "--theme",
        default="",
        choices=["", "dark", "light"],
        help="强制主题：深浅两套都做，默认跟随系统；截图核对时用它各截一张",
    )
    ap.add_argument(
        "--run",
        action="store_true",
        help="截「任务运行中」态：中栏折叠磁盘、换成每个源一条的传输列表",
    )
    args = ap.parse_args()

    if args.out:
        out = args.out
    else:
        name = "ui-preview"
        if args.dnd:
            name += "-dnd"
        if args.run:
            name += "-run"
        if args.theme:
            name += "-" + args.theme
        out = os.path.join(ROOT, "tools", name + ".png")

    parts = []
    if args.dnd:
        parts.append(f"dnd=1&x={args.dnd_x}&y={args.dnd_y}")
    if args.theme:
        parts.append("theme=" + args.theme)
    if args.run:
        parts.append("run=1")
    query = "?" + "&".join(parts) if parts else ""

    build()
    if args.shot:
        serve_bg()
        time.sleep(0.7)
        shot(out, args.width, args.height, query=query)
    elif args.serve:
        print(f"[preview] 拖拽态预览地址：http://127.0.0.1:{PORT}/?dnd=1&x=960&y=300")
        serve_bg()
        try:
            while True:
                time.sleep(1)
        except KeyboardInterrupt:
            print("\n[preview] 已停止")


if __name__ == "__main__":
    main()
