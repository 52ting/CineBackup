/**
 * dnd.js — 从系统（资源管理器 / Finder）拖入文件或文件夹
 *
 * Tauri 会拦截 webview 的原生拖放，改由 `tauri://drag-*` 事件把「绝对路径」交给前端，
 * 所以这里拿到的是真实磁盘路径，可以直接喂给后端的 probe_path / addSource。
 *
 * 投放意图按鼠标位置判定（只认右栏，其余全部落到源）：
 *   - 落在右栏「目标文件夹」`area-dst` 内 → 设为目标文件夹
 *   - 落在左栏「备份源」`area-src` 或其它任何区域 → 加入备份源
 *   - 任务运行中 → 只放行「加源」（业务层会在本轮结束后自动再跑一轮），改目标会被挡下
 */
import { onDragDrop } from "./backend.js";
import { esc } from "./ui.js";

const SRC_SEL = ".area-src";
const DST_SEL = ".area-dst";
const MAX_LIST = 3; // 遮罩里最多列几条路径
const IDLE_HIDE_MS = 2500; // 超过这个时间没有新事件就收起遮罩（防止个别平台漏发 leave 卡住）

let els = null;
let active = false; // 遮罩是否正在显示
let hit = "source"; // 当前命中区：source | target
let paths = []; // 本次拖拽携带的路径（over 事件不带 paths，用 enter 时缓存的）
let canDrop = () => true;
let isRunning = () => false;
let unlisten = null;
let lastEventAt = 0; // 最近一次收到拖拽事件的时间戳

const $ = (id) => document.getElementById(id);

/* ==================== 坐标换算（跨平台坑点） ==================== */

const UA = typeof navigator !== "undefined" ? navigator.userAgent || "" : "";
const IS_MAC = /Mac OS X|Macintosh/i.test(UA);

/**
 * Tauri 给的拖拽 coordinate 换算成 CSS 像素 —— 两个平台的单位**不一样**：
 *
 *   - macOS：wry 用 `NSPoint draggingLocation()`，是 AppKit 的**逻辑点**（Retina 上就等于 CSS 像素）
 *   - Windows：wry 用 `ScreenToClient()` 拿到的客户区坐标，是**物理像素**（高 DPI 要除以 dpr）
 *
 * 早先这里一律除以 `devicePixelRatio`，结果在 Retina Mac（dpr=2）上右栏 x≈950 被折半成
 * 475、落进中栏，命中判定兜底返回 "source" —— 「拖到目标栏」于是被当成「加到源」。
 * 现在按平台取尺度，并且换算后明显出界时再换另一个尺度兜一次。
 */
function scaleFactor() {
  const forced = Number(window.__CB_DND_SCALE__);
  if (forced > 0) return forced; // 预览工具投递的本来就是 CSS 像素
  const dpr = window.devicePixelRatio || 1;
  return IS_MAC ? 1 : dpr;
}

function inView(p) {
  const w = window.innerWidth || 0;
  const h = window.innerHeight || 0;
  return p.x >= 0 && p.y >= 0 && p.x <= w && p.y <= h;
}

/** 该点底下是哪个投放区；两者都不是返回 null */
function zoneAt(x, y) {
  const el = document.elementFromPoint(x, y);
  if (!el || typeof el.closest !== "function") return null;
  if (el.closest(DST_SEL)) return "target";
  if (el.closest(SRC_SEL)) return "source";
  return null;
}

/**
 * 几何兜底：按到两栏的水平距离取更近的那个。
 * 万一坐标尺度还是判错，也不至于把「投给目标」静默变成「加到源」。
 */
function nearestZone(x, y) {
  const measure = (sel) => {
    const el = document.querySelector(sel);
    if (!el) return null;
    const r = el.getBoundingClientRect();
    if (y < r.top || y > r.bottom) return null; // 纵向压根不在这一栏
    const dx = x < r.left ? r.left - x : x > r.right ? x - r.right : 0;
    return { dx, cx: (r.left + r.right) / 2 };
  };
  const a = measure(SRC_SEL);
  const b = measure(DST_SEL);
  if (!a) return b ? "target" : "source";
  if (!b) return "source";
  if (a.dx !== b.dx) return a.dx < b.dx ? "source" : "target";
  return Math.abs(x - a.cx) <= Math.abs(x - b.cx) ? "source" : "target";
}

/** 判定投放区 */
function hitTest(pos) {
  const dpr = window.devicePixelRatio || 1;
  const s = scaleFactor();
  let p = { x: (pos?.x ?? 0) / s, y: (pos?.y ?? 0) / s };
  if (!inView(p)) {
    const other = s === 1 ? dpr : 1;
    const q = { x: (pos?.x ?? 0) / other, y: (pos?.y ?? 0) / other };
    if (inView(q)) p = q;
  }
  return zoneAt(p.x, p.y) || nearestZone(p.x, p.y);
}

/* ==================== 遮罩 ==================== */

function shortName(p) {
  const s = String(p || "").replace(/[\\/]+$/, "");
  return s.split(/[\\/]/).pop() || s;
}

/** 重画遮罩文案 + 两侧卡片高亮 */
function paint() {
  const src = document.querySelector(SRC_SEL);
  const dst = document.querySelector(DST_SEL);
  const blocked = !canDrop(hit);
  const running = isRunning();

  if (src) {
    src.classList.toggle("drop-hot", !blocked && hit === "source");
    src.classList.toggle("drop-cold", !blocked && hit !== "source");
  }
  if (dst) {
    dst.classList.toggle("drop-hot", !blocked && hit === "target");
    dst.classList.toggle("drop-cold", !blocked && hit !== "target");
  }
  els.mask.classList.toggle("dnd-blocked", blocked);

  if (blocked) {
    els.icon.textContent = "⛔";
    els.title.textContent = "任务运行中，暂不能更换目标文件夹";
    els.sub.textContent = "可以拖到左栏继续加源；换目标请等任务结束或先点「取消任务」";
  } else if (hit === "target") {
    els.icon.textContent = "🎯";
    els.title.textContent = "松开 → 设为目标文件夹";
    els.sub.textContent = "拖入文件夹则直接作为目标；拖入文件则取其所在文件夹";
  } else {
    els.icon.textContent = "📥";
    els.title.textContent = "松开 → 加入备份源";
    els.sub.textContent = running
      ? "当前任务跑完后会自动补跑新加的源（已备完的源不再重扫）"
      : "可一次拖入多个文件 / 文件夹，将按断点续传规则逐项比对";
  }

  if (paths.length) {
    els.list.innerHTML =
      paths
        .slice(0, MAX_LIST)
        .map((p) => `<li title="${esc(p)}">${esc(p)}</li>`)
        .join("") +
      (paths.length > MAX_LIST
        ? `<li class="more">…以及另外 ${paths.length - MAX_LIST} 项</li>`
        : "");
    els.list.classList.remove("hidden");
  } else {
    els.list.innerHTML = "";
    els.list.classList.add("hidden");
  }
}

function show() {
  if (active) return;
  active = true;
  els.mask.classList.remove("hidden");
  document.body.classList.add("dnd-active");
  paint();
}

function hide() {
  if (!active) return;
  active = false;
  paths = [];
  els.mask.classList.add("hidden");
  els.mask.classList.remove("dnd-blocked");
  document.body.classList.remove("dnd-active");
  document
    .querySelectorAll(`${SRC_SEL},${DST_SEL}`)
    .forEach((el) => el.classList.remove("drop-hot", "drop-cold"));
}

/**
 * 挂载拖放。
 * @param {{
 *   canDrop?: (zone: "source"|"target") => boolean,
 *   isRunning?: () => boolean,
 *   onDropSources?: (paths: string[]) => void,
 *   onDropTarget?: (paths: string[]) => void,
 * }} opts
 * @returns {() => void} 卸载函数
 */
export function initDragDrop(opts = {}) {
  els = {
    mask: $("dndMask"),
    icon: $("dndIcon"),
    title: $("dndTitle"),
    sub: $("dndSub"),
    list: $("dndList"),
  };
  if (!els.mask) return () => {};
  if (typeof opts.canDrop === "function") canDrop = opts.canDrop;
  if (typeof opts.isRunning === "function") isRunning = opts.isRunning;

  onDragDrop((payload) => {
    const t = payload?.type;
    lastEventAt = Date.now();

    if (t === "enter" || t === "over") {
      if (Array.isArray(payload.paths) && payload.paths.length) paths = payload.paths;
      hit = hitTest(payload.position);
      show();
      paint();
      return;
    }

    if (t === "drop") {
      const dropped = Array.isArray(payload.paths) && payload.paths.length ? payload.paths : paths;
      // 落点以 drop 事件自带的坐标为准（over 事件可能稀疏 / 缺失）
      const where = payload.position ? hitTest(payload.position) : hit;
      hide();
      if (!dropped.length) return;
      if (!canDrop(where)) return; // 业务层无需再判
      if (where === "target") opts.onDropTarget?.(dropped);
      else opts.onDropSources?.(dropped);
      return;
    }

    // leave / 其它 → 收起遮罩
    hide();
  }).then((fn) => {
    unlisten = fn;
    opts.onStatus?.(onDragDrop.subscribed === true);
  });

  // 兜底：个别平台拖出窗口不一定派发 leave，窗口失焦或长时间无事件时清理
  window.addEventListener("blur", hide);
  const watchdog = setInterval(() => {
    if (active && Date.now() - lastEventAt > IDLE_HIDE_MS) hide();
  }, 800);

  return () => {
    window.removeEventListener("blur", hide);
    clearInterval(watchdog);
    hide();
    try {
      unlisten?.();
    } catch {
      /* 忽略卸载失败 */
    }
  };
}
