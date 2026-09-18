/**
 * dnd.js — 从系统（资源管理器 / Finder）拖入文件或文件夹
 *
 * Tauri 会拦截 webview 的原生拖放，改由 `tauri://drag-drop` 事件把「绝对路径」交给前端，
 * 所以这里拿到的是真实磁盘路径，可以直接喂给后端的 probe_path / addSource。
 *
 * 投放意图按鼠标位置判定（只认右栏，其余全部落到源）：
 *   - 落在右栏「目标文件夹」`area-dst` 内 → 设为目标文件夹
 *   - 落在左栏「备份源」`area-src` 或其它任何区域 → 加入备份源
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
let unlisten = null;
let lastEventAt = 0; // 最近一次收到拖拽事件的时间戳

const $ = (id) => document.getElementById(id);

/** Tauri 给的是物理像素，换算成 CSS 像素才能喂给 elementFromPoint */
function cssPoint(pos) {
  const dpr = window.devicePixelRatio || 1;
  return { x: (pos?.x ?? 0) / dpr, y: (pos?.y ?? 0) / dpr };
}

/** 鼠标底下是哪个投放区（遮罩是 pointer-events:none，不会挡住探测） */
function hitTest(x, y) {
  const el = document.elementFromPoint(x, y);
  if (el && el.closest && el.closest(DST_SEL)) return "target";
  return "source";
}

function shortName(p) {
  const s = String(p || "").replace(/[\\/]+$/, "");
  return s.split(/[\\/]/).pop() || s;
}

/** 重画遮罩文案 + 两侧卡片高亮 */
function paint() {
  const src = document.querySelector(SRC_SEL);
  const dst = document.querySelector(DST_SEL);
  const blocked = !canDrop();

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
    els.title.textContent = "任务运行中，暂不能修改源 / 目标";
    els.sub.textContent = "等当前任务结束，或先点「取消任务」再拖入";
  } else if (hit === "target") {
    els.icon.textContent = "🎯";
    els.title.textContent = "松开 → 设为目标文件夹";
    els.sub.textContent = "拖入文件夹则直接作为目标；拖入文件则取其所在文件夹";
  } else {
    els.icon.textContent = "📥";
    els.title.textContent = "松开 → 加入备份源";
    els.sub.textContent = "可一次拖入多个文件 / 文件夹，将按断点续传规则逐项比对";
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
 *   canDrop?: () => boolean,
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

  onDragDrop((payload) => {
    const t = payload?.type;
    lastEventAt = Date.now();

    if (t === "enter" || t === "over") {
      if (Array.isArray(payload.paths) && payload.paths.length) paths = payload.paths;
      const { x, y } = cssPoint(payload.position);
      hit = hitTest(x, y);
      show();
      paint();
      return;
    }

    if (t === "drop") {
      const dropped = Array.isArray(payload.paths) && payload.paths.length ? payload.paths : paths;
      const where = hit;
      hide();
      if (!dropped.length) return;
      if (!canDrop()) return; // 业务层无需再判，运行中直接丢弃
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
