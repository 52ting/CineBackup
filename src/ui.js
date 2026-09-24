/**
 * ui.js — 纯渲染层：把状态画到 DOM 上（不做任何业务决策）
 *
 * 对应三栏布局：左「源」列表 / 中「磁盘」或「传输」/ 右「目标」卡片。
 */
import { fmtBytes, fmtDuration, fmtSpeed } from "./backend.js";

const $ = (id) => document.getElementById(id);

/* ==================== 扁平线性图标（跟随 currentColor） ==================== */
export const ICO = {
  folder: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7.5A1.5 1.5 0 0 1 4.5 6h4l2 2h9A1.5 1.5 0 0 1 21 9.5v8A1.5 1.5 0 0 1 19.5 19h-15A1.5 1.5 0 0 1 3 17.5z"/></svg>`,
  file: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M14 3H7a1.5 1.5 0 0 0-1.5 1.5v15A1.5 1.5 0 0 0 7 21h10a1.5 1.5 0 0 0 1.5-1.5V7.5z"/><path d="M14 3v4.5h4.5"/></svg>`,
  missing: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M12 7.5v5.5M12 16.4v.1"/></svg>`,
  disk: `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="2.2"/><path d="M7 9.5h4"/><path d="M3 14.5h18"/></svg>`,
};

const iconOfKind = (k) => (k === "dir" ? ICO.folder : k === "file" ? ICO.file : ICO.missing);

/* ==================== 源列表（左栏） ==================== */
export function renderSources(sources) {
  const host = $("srcList");
  const empty = $("srcEmpty");
  if (!host) return;
  $("srcCount").textContent = String(sources.length);
  empty.classList.toggle("hidden", sources.length > 0);

  host.innerHTML = sources
    .map((s, i) => {
      const name = baseName(s.path) || s.path;
      const kindLabel = s.kind === "dir" ? "文件夹" : s.kind === "file" ? "文件" : "缺失";
      const meta = s.size > 0 ? `${kindLabel} · ${fmtBytes(s.size)}` : kindLabel;
      const fsCls = isFragileFs(s.fs) ? " warn-fs" : "";
      return `<li class="item" title="${esc(s.path)}">
        <span class="item-ico">${iconOfKind(s.kind)}</span>
        <div class="item-main">
          <div class="item-name">${esc(name)}</div>
          <div class="item-sub"><span class="mono">${esc(s.path)}</span></div>
          <div class="item-meta">
            <span class="fs-tag${fsCls}">${esc(s.fs || "—")}</span>
            <span>${esc(meta)}</span>
          </div>
        </div>
        <button class="icon-btn" data-del="${i}" title="移除">✕</button>
      </li>`;
    })
    .join("");
}

/* ==================== 目标卡片（右栏） ==================== */
export function renderTarget(target, free) {
  const card = $("dstCard");
  const empty = $("dstEmpty");
  if (!card) return;
  if (!target) {
    card.classList.add("hidden");
    empty.classList.remove("hidden");
    return;
  }
  card.classList.remove("hidden");
  empty.classList.add("hidden");

  $("dstIco").innerHTML = `<span class="disk-ico"></span>`;
  $("targetName").textContent = baseName(target.path) || target.path;
  const fsEl = $("targetFs");
  fsEl.textContent = target.fs || "—";
  fsEl.className = "fs-tag" + (isFragileFs(target.fs) ? " warn-fs" : "");
  $("targetPath").textContent = target.path;
  $("targetFree").textContent =
    free === null || free === undefined
      ? "剩余空间未知"
      : `剩余 ${fmtBytes(free)}${free === 0 ? "（可能未连接或已写满）" : ""}`;
}

/* ==================== 磁盘网格（中栏） ==================== */
const KIND_LABEL = {
  fixed: "本地磁盘",
  removable: "可移动",
  network: "网络盘",
  optical: "光驱",
  ramdisk: "内存盘",
  volume: "外置卷",
  system: "系统盘",
  unknown: "未知",
};

function fallbackLabel(d) {
  if (d.kind === "system") return "系统盘";
  if (d.kind === "optical") return "光驱";
  if (d.kind === "network") return "网络盘";
  return `磁盘 ${d.id}`;
}

/**
 * 渲染磁盘网格（中栏唯一一份，往左拖=源、往右拖=目标，点击则弹出操作菜单）。
 * @param {Array} disks 后端 list_disks 的返回
 * @param {{usedIds?:Set<string>, targetId?:string, busy?:boolean}} ctx
 */
export function renderDisks(disks, ctx = {}) {
  const host = $("diskGrid");
  if (!host) return;
  if (!disks || disks.length === 0) {
    host.innerHTML = `<div class="empty-note">未检测到可用磁盘，可用左栏「＋」手动选择。</div>`;
    return;
  }
  const used = ctx.usedIds || new Set();
  const busy = !!ctx.busy;

  host.innerHTML = disks
    .map((d) => {
      const ro = !d.writable;
      const noRoom = d.total > 0 && d.free === 0;
      const cls = ["disk-card"];
      if (used.has(d.id) || ctx.targetId === d.id) cls.push("is-used");
      if (ro) cls.push("is-ro");
      if (noRoom) cls.push("is-bad");
      if (busy) cls.push("is-busy");

      const name = d.label && d.label.trim() ? d.label : fallbackLabel(d);
      const cap =
        d.total > 0 ? (noRoom ? "剩余 0 B" : `剩余 ${fmtBytes(d.free)}`) : "容量未知";
      // 只给异常态打标签，卡片保持干净；磁盘类型 / 文件系统放进 title
      const tags = [];
      if (ro) tags.push('<span class="disk-tag ro">只读</span>');
      if (used.has(d.id)) tags.push('<span class="disk-tag ok">已作源</span>');
      if (ctx.targetId === d.id) tags.push('<span class="disk-tag ok">已作目标</span>');

      return `<div class="${cls.join(" ")}" data-id="${esc(d.id)}" data-mount="${esc(
        d.mount
      )}" data-name="${esc(name)}" data-fs="${esc(d.fs || "")}" data-writable="${
        ro ? 0 : 1
      }" data-freeroom="${noRoom ? 0 : 1}" title="${esc(name)} · ${esc(d.mount)} · ${esc(
        KIND_LABEL[d.kind] || d.kind
      )}${d.fs ? " · " + esc(d.fs) : ""}">
        <span class="disk-ico"></span>
        <span class="disk-name">${esc(name)}</span>
        <span class="disk-cap${noRoom ? " bad" : ""}">${cap}</span>
        ${tags.length ? `<span class="disk-tags">${tags.join("")}</span>` : ""}
      </div>`;
    })
    .join("");
}

/** 磁盘概览文字（顶部标题栏右侧） */
export function renderDiskSummary(disks) {
  const el = $("diskSummary");
  if (!el) return;
  if (!disks || disks.length === 0) {
    el.textContent = "未检测到磁盘";
    return;
  }
  const totalFree = disks.reduce((a, d) => a + (d.free || 0), 0);
  el.textContent = `${disks.length} 个卷 · 合计可用 ${fmtBytes(totalFree)}`;
}

/**
 * 顶部胶囊：源总量 vs 目标剩余空间，直接回答「装得下吗」。
 * @param {{srcBytes:number, needBytes:number, freeBytes:number|null, hasTarget:boolean}} s
 */
export function renderCapacity(s) {
  const el = $("capacityPill");
  if (!el) return;
  el.classList.remove("warn", "bad");
  // 「+」表示还有文件夹源的体积要等预扫描才算得出来
  const srcTxt = s.srcBytes > 0 ? `源共 ${fmtBytes(s.srcBytes)}${s.partial ? "+" : ""}` : "";
  if (!s.hasTarget) {
    el.textContent = srcTxt ? `${srcTxt} · 还没选目标盘` : "把素材拖进左栏开始";
    return;
  }
  const need = s.needBytes || 0;
  const free = s.freeBytes;
  if (free === null || free === undefined) {
    el.textContent = `${srcTxt || "源"} · 剩余空间未知`;
    return;
  }
  if (need <= 0) {
    // 还没预扫描 / 源体积未知 → 只报目标剩余空间，别说「需写入 0 B」
    el.textContent = srcTxt
      ? `${srcTxt} · 目标剩余 ${fmtBytes(free)}`
      : `目标剩余 ${fmtBytes(free)}`;
    return;
  }
  const short = need > free;
  el.classList.toggle("bad", short);
  el.textContent = short
    ? `空间不足：需 ${fmtBytes(need)} · 目标仅剩 ${fmtBytes(free)}`
    : `需写入 ${fmtBytes(need)} · 目标剩余 ${fmtBytes(free)}`;
  if (!short && free > 0 && need / free > 0.85) el.classList.add("warn");
}

/* ==================== 状态 ==================== */
const STATUS_LABEL = {
  idle: "空闲",
  scanning: "预扫描冲突中",
  copying: "拷贝中",
  verifying: "校验中",
  done: "完成",
};
export function renderStatus(status) {
  $("statusPill").dataset.s = status;
  $("statusText").textContent = STATUS_LABEL[status] || status;
  const running = status !== "idle" && status !== "done";
  // 「开始备份」运行中不禁用：点它是「追加一轮」（排队到本轮结束后），由 main.js 接管
  $("btnDry").disabled = running;
  $("btnCancel").classList.toggle("hidden", !running);
}

/**
 * 「开始备份」按钮的文案随状态变化：
 *   空闲 → 开始备份 ｜ 运行中 → 追加一轮 ｜ 已排队 → 取消排队
 */
export function renderStartButton({ running, queued }) {
  const b = $("btnStart");
  if (!b) return;
  b.textContent = !running ? "开始备份" : queued ? "取消排队 ⌛" : "追加一轮";
  b.title = !running
    ? "按当前源与目标开始备份"
    : queued
    ? "已排队：本轮任务结束后自动补跑新加的源；点击取消排队"
    : "不改动正在跑的任务，等它结束后自动补跑新加的源（已备完的源不再重扫）";
  b.classList.toggle("queued", !!queued);
}

/* ==================== 中栏：磁盘 ↔ 传输 双态切换 ==================== */
/**
 * @param {"disks"|"transfers"} mode
 * @param {{sub?:string, showTotal?:boolean}} ctx
 */
export function renderMidMode(mode, ctx = {}) {
  const grid = $("diskGrid");
  const list = $("transferList");
  const title = $("midTitle");
  const sub = $("midSub");
  if (!grid || !list) return;
  const showTransfers = mode === "transfers";
  grid.classList.toggle("hidden", showTransfers);
  list.classList.toggle("hidden", !showTransfers);
  title.textContent = showTransfers ? "传输" : "磁盘";
  if (sub) sub.textContent = ctx.sub || "";
  $("btnRefreshDisks").classList.toggle("hidden", showTransfers);
  // 总进度条：只要还有任务在跑 / 还有传输行，即使切到磁盘视图也留在下面
  const showTotal = ctx.showTotal === undefined ? showTransfers : !!ctx.showTotal;
  $("totalBar").classList.toggle("hidden", !showTotal);
}

/**
 * 折叠按钮（中栏右上角箭头）的外观：文案 + tooltip + 高亮态
 * @param {{icon?:string, title?:string, on?:boolean}} hint
 */
export function setFoldHint(hint = {}) {
  const b = $("btnFoldMid");
  if (!b) return;
  if (hint.icon) b.textContent = hint.icon;
  if (hint.title) b.title = hint.title;
  b.classList.toggle("on", !!hint.on);
}

/** 手动折叠：中栏内容（磁盘网格）收起；按钮外观统一由 setFoldHint 管 */
export function setMidFolded(folded) {
  const body = $("midBody");
  if (body) body.classList.toggle("hidden", folded);
  const b = $("btnFoldMid");
  if (b) b.classList.toggle("folded", folded);
}

/* ==================== 中栏：传输列表 ==================== */
const trMap = new Map(); // path -> 节点引用集合

function trRowText(r) {
  // 预扫描阶段后端只发 SCAN、不发 PROGRESS，此时每行的字节数还是 0。
  // 若仍显示「排队中」，在大目录 / 大量已备文件的场景下会长时间一片「排队中」，
  // 看着像卡死（0.4.3 之前用户就是这么误判的）。
  if (r.state === "scan") return "预扫描中…";
  if (r.state === "scanning") return "正在扫描…";
  if (r.state === "skipped") return "无需拷贝";
  if (r.state === "waiting") return "排队中";
  if (r.state === "failed") return "有文件失败";
  if (r.state === "done") return `已完成 · ${fmtBytes(r.bytesDone)}`;
  const pct = r.bytesTotal > 0 ? (r.bytesDone / r.bytesTotal) * 100 : 0;
  const parts = [`${pct.toFixed(1)}%`];
  if (r.speedBps > 0) {
    parts.push(fmtSpeed(r.speedBps));
    const remain = Math.max(0, r.bytesTotal - r.bytesDone);
    if (r.etaSecs > 0 || remain > 0) parts.push(`约 ${fmtDuration(r.etaSecs)}`);
  }
  return parts.join(" · ");
}

/**
 * 渲染传输列表（一行一个源）。按 path 复用 DOM，避免每 150ms 重建导致闪烁。
 * @param {Array<{path:string,kind:string,name:string,dstName:string,relPath:string,
 *   state:string,bytesTotal:number,bytesDone:number,currentFile:string,
 *   speedBps:number,etaSecs:number}>} rows
 */
export function renderTransfers(rows) {
  const host = $("transferList");
  if (!host) return;
  const seen = new Set();

  for (const r of rows) {
    seen.add(r.path);
    let n = trMap.get(r.path);
    if (!n) {
      const el = document.createElement("div");
      el.className = "transfer";
      el.innerHTML = `
        <div class="tr-ico">${r.kind === "dir" ? ICO.folder : ICO.disk}</div>
        <div class="tr-main">
          <div class="tr-title">
            <span class="tr-src"></span>
            <span class="tr-arrow">→</span>
            <span class="tr-dst"></span>
            <span class="tr-path mono"></span>
          </div>
          <div class="tr-bar"><i></i></div>
          <div class="tr-foot">
            <span class="tr-file"></span>
            <span class="tr-stat"></span>
          </div>
        </div>`;
      n = {
        el,
        src: el.querySelector(".tr-src"),
        arrow: el.querySelector(".tr-arrow"),
        dst: el.querySelector(".tr-dst"),
        path: el.querySelector(".tr-path"),
        bar: el.querySelector(".tr-bar"),
        fill: el.querySelector(".tr-bar > i"),
        file: el.querySelector(".tr-file"),
        stat: el.querySelector(".tr-stat"),
      };
      trMap.set(r.path, n);
      host.appendChild(el);
    }

    n.el.dataset.state = r.state;
    n.src.textContent = r.name;
    // 还没选目标时，别留一个孤零零的箭头
    const hasDst = !!r.dstName;
    n.dst.textContent = r.dstName || "";
    n.dst.classList.toggle("hidden", !hasDst);
    n.arrow.classList.toggle("hidden", !hasDst);
    n.path.textContent = r.relPath || "";
    n.file.textContent = r.currentFile || "";
    n.stat.textContent = trRowText(r);

    const pct =
      r.bytesTotal > 0
        ? Math.min(100, (r.bytesDone / r.bytesTotal) * 100)
        : r.state === "done"
        ? 100
        : 0;
    n.fill.style.width = `${pct.toFixed(2)}%`;
    n.bar.className =
      "tr-bar" +
      (r.state === "done"
        ? " done"
        : r.state === "failed"
        ? " failed"
        : r.state === "waiting" || r.state === "skipped"
        ? " idle"
        : r.state === "scan" || r.state === "scanning"
        ? " scan"
        : "");
  }

  for (const [k, n] of trMap) {
    if (!seen.has(k)) {
      n.el.remove();
      trMap.delete(k);
    }
  }
  // 按 rows 顺序重排（理论上本来就是顺序的，这里兜底）
  rows.forEach((r, i) => {
    const n = trMap.get(r.path);
    if (n && host.children[i] !== n.el) host.insertBefore(n.el, host.children[i] || null);
  });
}

export function clearTransfers() {
  const host = $("transferList");
  if (host) host.innerHTML = "";
  trMap.clear();
}

/* ==================== 进度 ==================== */
export function renderProgress(p) {
  const total = p.bytesTotal || 0;
  const done = p.bytesDone || 0;
  const pct = total > 0 ? Math.min(100, (done / total) * 100) : 0;
  const bar = $("progressBar");
  bar.style.width = `${pct.toFixed(2)}%`;
  bar.className = "progress-inner";

  $("phaseText").textContent =
    p.phase === "verify" ? "校验阶段" : p.phase === "scan" ? "预扫描" : "拷贝阶段";
  // 总进度可能是几百 GB：667 GB 上 1% 就是 6.7 GB，一位小数会几十秒才动一下。
  // 10% 以下多给一位，让「确实在动」看得出来。
  $("progressPct").textContent = pct < 10 ? `${pct.toFixed(2)}%` : `${pct.toFixed(1)}%`;
  $("progressBytes").textContent = `${fmtBytes(done)} / ${fmtBytes(total)}`;
  $("progressSpeed").textContent = fmtSpeed(p.speedBps);
  $("progressEta").textContent = `预计剩余：${fmtDuration(p.etaSecs)}`;

  // 当前文件的进度。总量几百 GB 时，总百分比动得太慢，这一项才是「活着」的读数
  // （后端一直在发 currentFileDone / currentFileTotal，之前前端没显示）。
  const ft = p.currentFileTotal || 0;
  const fd = p.currentFileDone || 0;
  const filePart =
    ft > 0
      ? ` · 当前文件 ${Math.min(100, (fd / ft) * 100).toFixed(0)}%（${fmtBytes(fd)} / ${fmtBytes(ft)}）`
      : "";
  if (p.currentFile) {
    $("curFile").textContent = `${p.filesDone}/${p.filesTotal}${filePart} · ${p.currentFile}`;
  } else if (p.filesTotal) {
    $("curFile").textContent = `${p.filesDone}/${p.filesTotal}`;
  } else {
    // 阶段收尾的事件不带文件名（拷贝阶段结束时就是这样），别显示成 0/0
    $("curFile").textContent = "—";
  }
}

export function resetProgress() {
  $("progressBar").style.width = "0%";
  $("progressBar").className = "progress-inner";
  $("progressPct").textContent = "0%";
  $("progressBytes").textContent = "0 B / 0 B";
  $("progressSpeed").textContent = "— /s";
  $("progressEta").textContent = "预计剩余：—";
  $("curFile").textContent = "—";
  $("phaseText").textContent = "—";
}

export function renderScanProgress(s) {
  $("phaseText").textContent = "预扫描";
  $("progressBytes").textContent = `${s.checked} / ${s.total} 项`;
  $("progressSpeed").textContent = `已枚举 ${s.filesSeen} 个文件`;
  $("progressEta").textContent = `已比对 ${fmtBytes(s.bytesHashed)}`;
  $("curFile").textContent = s.current || "扫描中…";
  if (s.total > 0) {
    const pct = Math.min(100, (s.checked / s.total) * 100);
    $("progressBar").className = "progress-inner";
    $("progressBar").style.width = `${pct.toFixed(2)}%`;
    $("progressPct").textContent = `${pct.toFixed(1)}%`;
  } else {
    // 枚举阶段还没有总数 → 不确定态动画
    $("progressBar").className = "progress-inner indeterminate";
    $("progressPct").textContent = "扫描中";
  }
}

export function markProgressDone(ok) {
  $("progressBar").style.width = "100%";
  $("progressBar").className = "progress-inner " + (ok ? "done" : "err");
}

/* ==================== 日志 ==================== */
let logLines = 0;
/**
 * 日志行的时间戳。
 * 后端 `now_iso8601()` 给的是 **UTC**（`2026-09-24T03:27:10Z`），
 * 早先直接 `ts.slice(11,19)` 取字符串 → 把 UTC 当本地时间显示，
 * 在 GMT+8 上日志里会同时出现 03:27 和 11:27 两种时刻，看着像两天的记录。
 * 这里统一转成浏览器本地时间再格式化。
 */
function logClock(ts) {
  if (ts) {
    const d = new Date(ts);
    if (!Number.isNaN(d.getTime())) return d.toTimeString().slice(0, 8);
  }
  return new Date().toTimeString().slice(0, 8);
}

export function pushLog(level, message, ts) {
  const box = $("logBox");
  const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 40;
  const time = logClock(ts);
  const div = document.createElement("div");
  div.className = `line lv-${level}`;
  div.innerHTML = `<span class="t">${time}</span>${esc(message)}`;
  box.appendChild(div);
  logLines++;
  $("logCount").textContent = `${logLines} 行`;
  if (box.childElementCount > 4000) box.removeChild(box.firstChild);
  if (nearBottom) box.scrollTop = box.scrollHeight;
}
export function clearLog() {
  $("logBox").innerHTML = "";
  logLines = 0;
  $("logCount").textContent = "0 行";
}

/* ==================== 结果列表 ==================== */
const ST_MAP = {
  pass: { icon: "✅", cls: "row-pass", label: "通过" },
  fail: { icon: "❌", cls: "row-fail", label: "失败" },
  error: { icon: "❌", cls: "row-fail", label: "错误" },
  skip: { icon: "⏭", cls: "row-skip", label: "跳过" },
  copy: { icon: "→", cls: "row-skip", label: "已拷贝" },
};
export function resetResults() {
  $("resultTbody").innerHTML = `<tr class="empty-row"><td colspan="5">校验完成后在此展示每个文件的结果。</td></tr>`;
  $("cntPass").textContent = "0";
  $("cntFail").textContent = "0";
  $("cntSkip").textContent = "0";
}
/** 校验值列的表头跟着所选算法走（SHA-256 = 64 字符 / xxHash64 = 16 字符） */
export function setHashHeader(label) {
  const th = $("thHash");
  if (!th) return;
  th.textContent = label || "校验值";
  th.title =
    label === "xxHash64"
      ? "内容哈希 · xxHash64（16 位十六进制）"
      : "内容哈希 · SHA-256（64 位十六进制，可与 shasum -a 256 对照）";
}
export function addResult(r) {
  const tb = $("resultTbody");
  const empty = tb.querySelector(".empty-row");
  if (empty) empty.remove();
  const m = ST_MAP[r.status] || ST_MAP.skip;
  const tr = document.createElement("tr");
  tr.className = m.cls;

  // 校验值：源/目标不一致时（fail）把两份都塞进 tooltip，方便比对
  const src = r.srcHash || "";
  const dst = r.dstHash || "";
  let hashCell;
  if (!src) {
    hashCell = `<td class="hash-cell none">—</td>`;
  } else {
    const tip = dst && dst !== src ? `源  ${src}\n目标 ${dst}` : src;
    hashCell = `<td class="hash-cell" title="${esc(tip)}">${esc(src)}</td>`;
  }

  tr.innerHTML = `<td><span class="${r.status === "fail" || r.status === "error" ? "st-fail" : r.status === "pass" ? "st-pass" : "st-skip"}">${m.icon} ${m.label}</span></td>
    <td class="path-cell mono" title="${esc(r.path)}">${esc(baseName(r.path))}</td>
    <td class="mono">${fmtBytes(r.size)}</td>
    ${hashCell}
    <td title="${esc(r.message || "")}">${esc(r.message || "")}</td>`;
  tb.appendChild(tr);
}
export function bumpCounter(kind) {
  const el = kind === "pass" ? $("cntPass") : kind === "fail" ? $("cntFail") : $("cntSkip");
  el.textContent = String(Number(el.textContent) + 1);
}

/* ==================== 模态框 ==================== */
export function showConflict(payload, onReply) {
  $("cFileName").textContent = baseName(payload.src);
  $("cDetail").textContent =
    `源文件：${payload.src}\n` +
    `目标文件：${payload.dst}\n` +
    `源大小：${fmtBytes(payload.srcSize)}   目标大小：${fmtBytes(payload.dstSize)}\n` +
    `判定：${payload.reason || "—"}\n` +
    `建议：${labelOf(payload.suggested)}`;
  openModal("conflictModal", onReply);
}
export function showError(payload, onReply) {
  $("eFile").textContent = `文件：${payload.src}`;
  $("eDetail").textContent = `目标：${payload.dst}\n错误：${payload.error}`;
  openModal("errorModal", onReply);
}
function openModal(id, onReply) {
  const mask = $(id);
  // 清掉上一次可能残留的监听，避免重复触发
  if (activeHandler) {
    activeHandler.mask.removeEventListener("click", activeHandler.fn);
    activeHandler = null;
  }
  mask.classList.remove("hidden");
  const fn = (ev) => {
    const btn = ev.target.closest("button[data-reply]");
    if (!btn) return;
    mask.classList.add("hidden");
    mask.removeEventListener("click", fn);
    activeHandler = null;
    onReply(btn.dataset.reply);
  };
  activeHandler = { mask, fn };
  mask.addEventListener("click", fn);
}

/** 任务结束时强制关掉所有弹窗（例如弹窗还开着用户点了「取消任务」） */
export function hideModals() {
  ["conflictModal", "errorModal"].forEach((id) => $(id).classList.add("hidden"));
  if (activeHandler) {
    activeHandler.mask.removeEventListener("click", activeHandler.fn);
    activeHandler = null;
  }
}
let activeHandler = null;

/* ==================== 工具 ==================== */
export function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}
export function baseName(p) {
  const s = String(p || "").replace(/[\\/]+$/, "");
  return s.split(/[\\/]/).pop() || s;
}
function labelOf(a) {
  return (
    { copy: "完整拷贝", resume: "断点续传（从末尾续写）", skip: "跳过（内容一致）", overwrite: "覆盖", conflict: "待询问" }[
      a
    ] || a || "—"
  );
}
/** macOS 原生 NTFS 只读 / 网络盘等需要提醒的文件系统 */
function isFragileFs(fs) {
  const f = String(fs || "").toLowerCase();
  return f.includes("ntfs");
}
export const _internal = { $ };
