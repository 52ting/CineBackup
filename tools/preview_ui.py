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
    python tools/preview_ui.py --shot --run --fold              # 运行中点箭头 → 切回磁盘视图
    python tools/preview_ui.py --shot --dnd --dnd-x 1150 --dnd-y 300   # 拖拽悬停态
    python tools/preview_ui.py --shot --dnd --drop --dnd-x 1150        # 真松手：右栏应设为目标
    python tools/preview_ui.py --shot --run --dnd --drop --dnd-x 300   # 运行中拖入左栏 → 自动排队
    python tools/preview_ui.py --shot --opts                          # 展开「任务选项」（校验算法选择器）

预览页 URL 参数（浏览器里也能手动看效果）：
    ?theme=dark|light   强制主题
    ?run=1              任务运行中态（中栏换成每个源一条的传输进度，并投递几条校验结果）
    ?run=1&fold=1       运行中把中栏切回磁盘视图
    ?dnd=1&x=&y=        拖拽悬停态；再加 &drop=1 会真的松手投递一次
    ?opts=1             展开任务选项下拉
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
  // 预览页的拖拽坐标本来就按 CSS 像素投递，明确告诉 dnd.js 别再除 dpr
  window.__CB_DND_SCALE__ = 1;
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
      // 存下 id：queuetest 场景需要在「本轮结束」时把它停掉，
      // 否则这个只带本轮 3 个源的定时器会一直覆盖下一轮的进度数据
      window.__cbRunTimer = setInterval(push, 600);

      // 文件级结果 → 「校验结果」表里能看到 SHA-256 的 64 位校验值
      // （故意混一条 fail 和一条 skip，覆盖有值 / 无值两种单元格）
      const OK = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
      const H1 = "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4";
      const H2 = "9f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4";
      const RESULTS = [
        {
          path: "D:\\\\拍摄素材\\\\《山海》A001_20260901\\\\A001_C001_0701AB.R3D",
          target: "E:\\\\CineBackup\\\\2026-09-18\\\\《山海》A001_20260901\\\\A001_C001_0701AB.R3D",
          size: 12884901888, status: "pass",
          srcHash: OK, dstHash: OK, message: "SHA-256 校验一致",
        },
        {
          path: "D:\\\\拍摄素材\\\\《山海》A001_20260901\\\\A001_C002_0702AC.R3D",
          target: "E:\\\\CineBackup\\\\2026-09-18\\\\《山海》A001_20260901\\\\A001_C002_0702AC.R3D",
          size: 8589934592, status: "pass",
          srcHash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
          dstHash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
          message: "SHA-256 校验一致",
        },
        {
          path: "Z:\\\\DCP\\\\shanhai_final_dcp.zip",
          target: "E:\\\\CineBackup\\\\2026-09-18\\\\shanhai_final_dcp.zip",
          size: 26843545600, status: "fail",
          srcHash: H1, dstHash: H2,
          message: "SHA-256 不一致（源 " + H1.slice(0, 8) + " / 目标 " + H2.slice(0, 8) + "，字节 26843545600 vs 26843545600）",
        },
        {
          path: "D:\\\\PROXY\\\\day03.mov",
          target: "E:\\\\CineBackup\\\\2026-09-18\\\\PROXY\\\\day03.mov",
          size: 3221225472, status: "skip",
          srcHash: "", dstHash: "", message: "按选择跳过，未校验",
        },
      ];
      RESULTS.forEach((r, i) =>
        setTimeout(() => window.__cbEmit("cb:file-result", r), 1200 + i * 300)
      );
    }, 2600);
  }

  // ?verify=1 → 「校验阶段进行中」。重点核对底部那三行读数：
  // 大任务上总百分比动得极慢（667 GB 里 1% 就是 6.7 GB），所以
  //   ① 百分比在 10% 以下给两位小数；
  //   ② 补上「当前文件」自己的百分比与字节数（后端一直在发，之前前端没显示）。
  // 数字直接取自用户真机截图（macOS，需写入 334 GB → 校验要读 667 GB）。
  if (qs.has("verify")) {
    const VBIG = 716251971584; // 667 GB
    setTimeout(function () {
      window.__cbEmit("cb:status", { status: "verifying" });
      window.__cbEmit("cb:plan", {
        copy: 263, resume: 0, skip: 0, overwrite: 0, conflict: 0,
        totalBytes: 358587598080, filtered: 0,
      });
      // 先给一条拷贝阶段的「按源」进度：用自动演示里加进来的那 3 个源，
      // 否则它们会停在中栏显示成「排队中」（预览脚本的 mock 数据所致）。
      // 路径必须和 mock 磁盘发现出来的完全一致，才会就地更新而不是多出几行。
      window.__cbEmit("cb:progress", {
        phase: "copy", filesTotal: 263, filesDone: 263,
        bytesTotal: VBIG, bytesDone: VBIG, speedBps: 0, etaSecs: 0,
        currentFile: "", currentFileDone: 0, currentFileTotal: 0,
        elapsedSecs: 3720,
        sources: [
          { index: 0, path: "D:\\\\拍摄素材\\\\《山海》A001_20260901",
            bytesTotal: 322122547200, bytesDone: 322122547200,
            filesTotal: 131, filesDone: 131, state: "done", currentFile: "" },
          { index: 1, path: "E:\\\\DIT\\\\R3D_RAW\\\\Day03",
            bytesTotal: 34896609280, bytesDone: 34896609280,
            filesTotal: 132, filesDone: 132, state: "done", currentFile: "" },
          { index: 2, path: "Z:\\\\DCP\\\\shanhai_final_dcp.zip",
            bytesTotal: 26843545600, bytesDone: 26843545600,
            filesTotal: 1, filesDone: 1, state: "done", currentFile: "" },
        ],
      });
      const VCUR =
        "/Volumes/Tiger/2024_9_17 中秋节民宿施工/Cam A/A153C001_240917LR.MP4";
      const VFILE_TOTAL = 2480349248; // 2.31 GB
      let n = 0;
      const pushV = function () {
        window.__cbEmit("cb:progress", {
          phase: "verify", filesTotal: 263, filesDone: 1 + n,
          bytesTotal: VBIG, bytesDone: 4966055936 + n * 805306368,
          speedBps: 67829760, etaSecs: 10440 - n * 3,
          currentFile: VCUR,
          currentFileDone: 1073741824 + n * 134217728,
          currentFileTotal: VFILE_TOTAL, elapsedSecs: 76 + n * 2,
          sources: [],
        });
        n++;
      };
      pushV();
      setInterval(pushV, 900);
      // 校验结果表也跟着涨两条，和截图一致
      const VH = "a63c4888d15ecd31b1b6b1a7b6b1a7b6b1a7b6b1a7b6b1a7b6b1a7b6b1a7b6b1";
      setTimeout(function () {
        window.__cbEmit("cb:file-result", {
          path: VCUR, target: "/Volumes/2024 《老虎的斑纹》/" + VCUR.split("/").pop(),
          size: VFILE_TOTAL, status: "pass", srcHash: VH, dstHash: VH,
          message: "SHA-256 校验一致",
        });
      }, 1200);
      setTimeout(function () {
        window.__cbEmit("cb:file-result", {
          path: "/Volumes/Tiger/2024_9_17 中秋节民宿施工/Cam A/A153C001_240917LRM01.XML",
          target: "/Volumes/2024 《老虎的斑纹》/A153C001_240917LRM01.XML",
          size: 2201170739, status: "pass", srcHash: VH, dstHash: VH,
          message: "SHA-256 校验一致",
        });
      }, 1500);
    }, 2600);
  }

  // ?dnd=1&x=960&y=300 → 自动进入拖拽悬停态，好截拖拽遮罩
  // 再加 &drop=1 → 2.4s 后真的「松手」，验证落点判定（右栏应设为目标、左栏应加源）
  if (qs.has("dnd")) {
    const x = Number(qs.get("x") || 320);
    const y = Number(qs.get("y") || 300);
    setTimeout(() => window.__cbDnd("enter", DROP_PATHS, x, y), 1500);
    if (qs.has("drop")) {
      setTimeout(() => window.__cbDnd("drop", DROP_PATHS, x, y), 2400);
    } else {
      // 真机上鼠标移动会持续派发 over，这里也持续发，否则会被遮罩的看门狗收掉
      setInterval(() => window.__cbDnd("over", DROP_PATHS, x, y), 250);
    }
  }

  // ?queuetest=1 → 回归场景：运行中往左栏加源 → 本轮 job-end 后应自动再跑一轮。
  // 与 ?run=1&dnd=1&drop=1&x=300&y=300 组合使用；结果写进 <pre id="probeOut">，
  // 由 `preview_ui.py --probe`（chrome --dump-dom）取回，不截图。
  //
  // 记录每次 start_job 的 sources，就能回答关键问题：
  //   ① 第二轮到底有没有发出去？  ② 发出去的那轮带没带上新加的源？
  if (qs.has("queuetest")) {
    const calls = [];
    const rawInvoke = window.__TAURI_INTERNALS__.invoke;
    window.__TAURI_INTERNALS__.invoke = function (cmd, args) {
      if (cmd === "start_job") {
        calls.push({
          n: calls.length + 1,
          tMs: Math.round(performance.now()),
          sources: ((args || {}).req || {}).sources || [],
          dryRun: !!(args || {}).req && !!(args || {}).req.dryRun,
        });
      }
      return rawInvoke.apply(this, arguments);
    };

    const btns = () => document.getElementById("btnStart");
    const rowSnap = () =>
      Array.from(document.querySelectorAll("#transferList .transfer")).map((e) => ({
        name: (e.querySelector(".tr-src") || {}).textContent || "",
        state: e.dataset.state,
      }));
    const resultSnap = () =>
      Array.from(document.querySelectorAll("#resultTbody tr"))
        .filter((tr) => !tr.classList.contains("empty-row"))
        .map((tr) => ({
          file: ((tr.querySelector(".path-cell") || {}).textContent || "").trim(),
          status: ((tr.querySelector("td span") || {}).textContent || "").trim(),
          hash: ((tr.querySelector(".hash-cell") || {}).textContent || "").trim(),
        }));
    // 日志面板里的时间戳是真实时钟，跨运行时无意义，只留级别+正文
    const logLines = () =>
      Array.from(document.querySelectorAll("#logBox .line")).map((e) =>
        e.textContent.replace(/^[0-9][0-9]:[0-9][0-9]:[0-9][0-9]/, "").trim()
      );

    // 追加轮后端只会带这 4 个新源（= 拖进来减去已存在的那条）
    const APPEND_SRC = [
      { index: 0, path: "E:\\\\拍摄素材\\\\《山海》A001_20260901",
        bytesTotal: 21474836480, bytesDone: 21474836480,
        filesTotal: 5, filesDone: 5, state: "done", currentFile: "" },
      { index: 1, path: "E:\\\\拍摄素材\\\\《山海》A002_20260902",
        bytesTotal: 32212254720, bytesDone: 10737418240,
        filesTotal: 7, filesDone: 2, state: "active",
        currentFile: "E:\\\\拍摄素材\\\\《山海》A002_20260902\\\\A002_C003_0715AD.R3D" },
      { index: 2, path: "D:\\\\PROXY\\\\day03.mov",
        bytesTotal: 2147483648, bytesDone: 0,
        filesTotal: 1, filesDone: 0, state: "waiting", currentFile: "" },
      { index: 3, path: "D:\\\\PROXY\\\\day04.mov",
        bytesTotal: 2147483648, bytesDone: 0,
        filesTotal: 1, filesDone: 0, state: "waiting", currentFile: "" },
    ];

    const dump = () => {
      if (document.getElementById("probeOut")) return;
      // 只有 --probe 才把判定贴到页面上：截图模式下这会盖住整个界面
      if (!qs.has("probe")) return;
      // 断言一：运行中加源后排的那一轮，应「只带新加的源」。
      // 带上老源的话，预扫描要把上一轮刚写完的内容（源+目标）各再读一遍 —— 素材盘上就是几小时空转。
      const first = calls[0] ? calls[0].sources : [];
      const second = calls[1] ? calls[1].sources : [];
      const rows = rowSnap();
      const results = resultSnap();
      const pre = document.createElement("pre");
      pre.id = "probeOut";
      pre.textContent = JSON.stringify(
        {
          startCallCount: calls.length,
          startCalls: calls,
          verdict: {
            第二轮是否发出: calls.length >= 2,
            第二轮源个数: second.length,
            第二轮是否只含新源:
              second.length > 0 && second.every((p) => !first.includes(p)),
          },
          // 断言二：追加轮是「承接」而不是「重开」——
          // 上一轮已完成的传输行和校验结果都必须还在（0.4.3 曾把它们清掉，
          // 表现就是「第一个先拷的文件没有校验值」）。
          carryover: {
            中栏行数: rows.length,
            中栏是否保留上一轮的源: first.every((p) =>
              rows.some((r) => p.indexOf(r.name) >= 0 || r.name === p)
            ),
            结果表行数: results.length,
            结果表是否保留上一轮的校验值:
              results.some((r) => /A001_C001_0701AB/.test(r.file)) &&
              results.some((r) => r.hash && r.hash !== "—"),
            结果表文件: results.map((r) => r.file),
            通过计数: (document.getElementById("cntPass") || {}).textContent,
          },
          btnText: btns() ? btns().textContent : "(无按钮)",
          btnQueued: btns() ? btns().classList.contains("queued") : null,
          srcCount: (document.getElementById("srcCount") || {}).textContent,
          // 断言三：日志措辞别串台 —— 「本轮追加」这种标签只能出现在真正的追加轮
          firstSummary,
          第一轮是否被误标为追加: /本轮追加/.test(firstSummary),
          rows: rows,
          warnOrErrLogs: logLines().filter((l) => /失败|错误|取消|中止/.test(l)),
          tailLogs: logLines().slice(-14),
        },
        null,
        2
      );
      document.body.appendChild(pre);
      document.title = "PROBE_DONE";
    };

    // 3s：再补一条**本轮（上一轮）**的校验结果。
    // 目的是让「结果表是否保留上一轮」这条断言不依赖时序运气：
    // 任务肯定已经开始跑（开始备份的点击在 2.2s 左右），因此这条一定进的是
    // 第一轮的结果表 —— 追加轮开跑后它必须还在。
    setTimeout(() => {
      window.__cbEmit("cb:file-result", {
        path: "D:\\\\拍摄素材\\\\《山海》A001_20260901\\\\A001_C009_0710ZZ.R3D",
        target: "E:\\\\CineBackup\\\\2026-09-24\\\\《山海》A001_20260901\\\\A001_C009_0710ZZ.R3D",
        size: 4294967296, status: "pass",
        srcHash: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        dstHash: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        message: "SHA-256 校验一致",
      });
    }, 3000);

    // 6.1s：job-end 刚处理完、追加轮还没起跑 —— 抓这一瞬间的措辞，
    // 用来确认「（本轮追加）」这种标签只出现在真正的追加轮，不会跑到第一轮头上
    let firstSummary = "";
    setTimeout(() => {
      const all = logLines().filter((l) => l.indexOf("汇总") === 0);
      firstSummary = all.length ? all[all.length - 1] : "(没有汇总行)";
    }, 6100);

    // 6s：模拟本轮任务跑完（注意 elapsedSecs 必须有值，否则前端 toFixed 会抛错）
    setTimeout(() => {
      window.__cbEmit("cb:job-end", {
        ok: true, message: "", copied: 12, resumed: 0, overwritten: 0, skipped: 3,
        pass: 12, failed: 0, totalBytes: 123456789, elapsedSecs: 3.5,
        aborted: false, dryRun: false,
      });
      // 本轮结束 → 本轮那个「只带老源」的进度定时器必须停掉，
      // 真机上后端这时已经把进度切到追加轮了
      if (window.__cbRunTimer) clearInterval(window.__cbRunTimer);
    }, 6000);
    // 7.4s：追加轮已经开跑（job-end + 500ms 延时）→ 投一条**只含新源**的进度。
    //        此时上一轮那 3 行和校验结果表都必须还在。
    setTimeout(() => {
      window.__cbEmit("cb:progress", {
        phase: "copy", filesTotal: 14, filesDone: 7,
        bytesTotal: 77284382720, bytesDone: 32212254720,
        speedBps: 503316480, etaSecs: 90,
        currentFile: APPEND_SRC[1].currentFile,
        currentFileDone: 0, currentFileTotal: 0, elapsedSecs: 42, sources: APPEND_SRC,
      });
    }, 7400);
    // 9s：追加轮进度早已到达，抓状态
    setTimeout(dump, 9000);
  }


  // ?scan=1 → 预扫描进行中：中栏每行不该显示「排队中」，而是
  // 当前那行「正在扫描…」+ 流动条纹，其余「预扫描中…」。
  // （0.4.3 之前这段时间整列都是「排队中」，用户误以为卡死了。）
  if (qs.has("scan")) {
    const SCAN_CUR = [
      "E:\\\\DIT\\\\R3D_RAW\\\\Day03\\\\A003_C012_0712AB.R3D",
      "E:\\\\DIT\\\\R3D_RAW\\\\Day03\\\\A007_C021_0755EF.R3D",
      "Z:\\\\DCP\\\\shanhai_final_dcp.zip",
      "D:\\\\拍摄素材\\\\《山海》A001_20260901\\\\A001_C002_0702AC.R3D",
    ];
    setTimeout(function () {
      window.__cbEmit("cb:status", { status: "scanning" });
      let n = 0;
      const pushScan = function () {
        window.__cbEmit("cb:scan", {
          phase: "check", filesSeen: 1380 + n * 12, dirsSeen: 96,
          checked: 240 + n * 12, total: 1386,
          bytesHashed: 15032385536 + n * 1073741824,
          current: SCAN_CUR[n % SCAN_CUR.length],
        });
        n++;
      };
      pushScan();
      setInterval(pushScan, 700);
    }, 400);
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
    // ?run=1 / ?scan=1 / ?verify=1 → 再按下「开始备份」，界面就会切到传输视图
    if (qs.has("run") || qs.has("scan") || qs.has("verify")) {
      await sleep(220);
      document.getElementById("btnStart")?.click();
      // ?fold=1 → 运行中再点中栏箭头，应折回磁盘视图（验证箭头能来回切）
      if (qs.has("fold")) {
        await sleep(1200);
        document.getElementById("btnFoldMid")?.click();
      }
    }
    // ?opts=1 → 展开任务选项（看校验算法选择器）
    if (qs.has("opts")) {
      await sleep(280);
      document.getElementById("btnOptions")?.click();
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


def probe(query="", wait_ms=11000):
    """跑一个带断言的场景，把页面里 <pre id="probeOut"> 的内容打出来（不截图）。

    比截图更硬：截图只能看「长什么样」，probe 能回答「某个动作之后到底发生了什么」
    ——例如「运行中加源，本轮结束后有没有真的再发一个 start_job，带没带上新源」。
    """
    import html as _html
    import re

    chrome = find_chrome()
    profile = tempfile.mkdtemp(prefix="cb-preview-")
    cmd = [
        chrome,
        "--headless=new",
        "--disable-gpu",
        "--hide-scrollbars",
        "--force-device-scale-factor=1",
        "--window-size=1280,880",
        f"--virtual-time-budget={wait_ms}",
        f"--user-data-dir={profile}",
        "--dump-dom",
        f"http://127.0.0.1:{PORT}/{query}",
    ]
    r = subprocess.run(cmd, capture_output=True)
    shutil.rmtree(profile, ignore_errors=True)
    dom = r.stdout.decode("utf-8", "replace")
    m = re.search(r'<pre id="probeOut">(.*?)</pre>', dom, re.S)
    if not m:
        print(dom[-2500:])
        sys.exit("页面里没有 #probeOut —— 场景没跑起来（看上面 DOM 尾部）")
    print(_html.unescape(m.group(1)))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--serve", action="store_true")
    ap.add_argument("--shot", action="store_true")
    ap.add_argument(
        "--probe",
        action="store_true",
        help="跑 ?queuetest=1 回归场景并打印 #probeOut（自动带上 run/dnd/drop，不截图）",
    )
    ap.add_argument("--out", default="")
    ap.add_argument("--width", type=int, default=1560)
    ap.add_argument("--height", type=int, default=1000)
    ap.add_argument(
        "--wait",
        type=int,
        default=0,
        help="截图前的等待毫秒数（默认 6000；queuetest 场景自动用 10000）",
    )
    ap.add_argument(
        "--queuetest",
        action="store_true",
        help="配合 --shot：跑「运行中加源 → 追加轮」场景并截图（可看到上一轮的行与校验结果仍在）",
    )
    ap.add_argument(
        "--dnd",
        action="store_true",
        help="截「拖拽悬停」态（显示投放遮罩），配合 --dnd-x / --dnd-y 指定悬停位置",
    )
    ap.add_argument("--dnd-x", type=int, default=320, help="拖拽悬停点 x（左栏约 300，右栏约 1150）")
    ap.add_argument("--dnd-y", type=int, default=300)
    ap.add_argument(
        "--drop",
        action="store_true",
        help="配合 --dnd：2.4s 后真的松手投递一次，验证落点判定（右栏→设目标 / 左栏→加源）",
    )
    ap.add_argument(
        "--fold",
        action="store_true",
        help="配合 --run：运行中点一次中栏箭头，截「传输 ⇄ 磁盘」切换后的样子",
    )
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
    ap.add_argument(
        "--scan",
        action="store_true",
        help="截「预扫描进行中」态：中栏应显示「正在扫描…/预扫描中…」而不是整列「排队中」",
    )
    ap.add_argument(
        "--verify",
        action="store_true",
        help="截「校验阶段进行中」态：核对底部百分比精度与「当前文件」进度（大任务上总百分比动得很慢）",
    )
    ap.add_argument(
        "--opts",
        action="store_true",
        help="打开「任务选项」下拉（核对校验算法选择器）",
    )
    args = ap.parse_args()

    if args.probe or args.queuetest:
        # 回归场景固定需要：运行中 + 往左栏拖一次（新源）+ 排队探针
        args.run = args.dnd = args.drop = True

    if args.out:
        out = args.out
    else:
        name = "ui-preview"
        if args.dnd:
            name += "-dnd"
        if args.drop:
            name += "-drop"
        if args.run:
            name += "-run"
        if args.scan:
            name += "-scan"
        if args.verify:
            name += "-verify"
        if args.queuetest:
            name += "-queuetest"
        if args.fold:
            name += "-fold"
        if args.opts:
            name += "-opts"
        if args.theme:
            name += "-" + args.theme
        out = os.path.join(ROOT, "tools", name + ".png")

    parts = []
    if args.dnd:
        parts.append(f"dnd=1&x={args.dnd_x}&y={args.dnd_y}")
        if args.drop:
            parts.append("drop=1")
    if args.theme:
        parts.append("theme=" + args.theme)
    if args.run:
        parts.append("run=1")
        if args.fold:
            parts.append("fold=1")
    if args.scan:
        parts.append("scan=1")
    if args.verify:
        parts.append("verify=1")
        args.run = True  # 复用「运行中」的数据铺垫，界面才会切到传输视图
    if args.opts:
        parts.append("opts=1")
    if args.probe or args.queuetest:
        parts.append("queuetest=1")
    if args.probe:
        parts.append("probe=1")
    query = "?" + "&".join(parts) if parts else ""

    build()
    if args.probe:
        serve_bg()
        time.sleep(0.7)
        probe(query=query)
    elif args.shot:
        serve_bg()
        time.sleep(0.7)
        # queuetest 场景里「追加轮进度」在 7.4s 才投出来，默认 6s 的预算截不到
        wait = args.wait or (10000 if "queuetest" in query else 6000)
        shot(out, args.width, args.height, wait_ms=wait, query=query)
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
