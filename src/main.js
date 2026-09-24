/**
 * main.js — 应用装配层：把前端交互 + Tauri 命令 + 后端事件串起来
 *
 * 布局约定：左栏 = 备份源，右栏 = 目标文件夹。
 * 两栏顶部都会自动拉取本机磁盘，点一下就定位到该盘去挑文件。
 */
import { open, save } from "@tauri-apps/plugin-dialog";
import {
  EV, probePath, fsType, freeSpace, listDisks, startJob, replyDecision,
  cancelJob, saveTaskFile, loadTaskFile, on, fmtBytes,
} from "./backend.js";
import * as ui from "./ui.js";
import { initDragDrop } from "./dnd.js";

const $ = (id) => document.getElementById(id);

/* ==================== 应用状态 ==================== */
const state = {
  /** @type {{path:string, kind:string, fs:string, size:number, exists:boolean}[]} */
  sources: [],
  target: null, // { path, fs }
  /** 目标剩余空间（字节）；null = 未知 */
  targetFree: null,
  /** 预扫描算出的实际需写入字节（比源总量准）；0 = 还没扫过 */
  planBytes: 0,
  /** 中栏当前展示的是磁盘网格还是传输列表（运行中也能手动切回去看磁盘） */
  midView: "disks",
  /** 中栏磁盘网格是否被手动折叠（仅空闲态有意义） */
  midFolded: false,
  /** 运行中往源里加了新素材 → 排一轮追加备份，本轮结束后自动开跑 */
  pendingRun: false,
  /**
   * 上一轮实际发出去的源（归一化 key）。
   *
   * 「追加一轮」只补这里面**没有**的源，理由见 queueRun 的注释：
   * 已经备完的源再扫一遍，判定「跳过」要靠内容哈希，会把源和目标各完整读一遍。
   */
  roundKeys: new Set(),
  /**
   * 本轮任务的相位：`"scan"`（预扫描）| `"copy"` | `"verify"`。
   * 预扫描只发 `cb:scan`、不发 `cb:progress`，中栏靠这个字段决定
   * 显示「预扫描中…」还是「排队中」，免得大目录扫描期间整列假死。
   */
  phase: "idle",
  /** 运行中每个源的进度行（来自后端 progress 事件） */
  runRows: [],
  /** @type {Array} 后端自动拉取到的磁盘 / 卷 */
  disks: [],
  running: false,
};

/* ==================== 磁盘自动拉取 ==================== */

/** 路径归一化，仅用于「是否重复」判断 */
function normKey(p) {
  return String(p ?? "")
    .replace(/[\\/]+/g, "/")
    .replace(/\/+$/, "")
    .toLowerCase();
}

/** 这条路径落在哪个卷上（返回磁盘 id） */
function mountIdOf(p) {
  if (!p) return null;
  const s = String(p).replace(/\\/g, "/");
  // Windows 盘符优先
  const m = /^([A-Za-z]):/.exec(s);
  if (m) return `${m[1].toUpperCase()}:`;
  // unix / macOS：取最长前缀匹配的挂载点
  let best = null;
  for (const d of state.disks) {
    let mp = String(d.mount || "").replace(/\\/g, "/");
    if (mp !== "/") mp = mp.replace(/\/+$/, "");
    if (!mp) continue;
    const hit = mp === "/" ? s.startsWith("/") : s === mp || s.startsWith(mp + "/");
    if (hit && (!best || mp.length > best.mount.length)) best = { id: d.id, mount: mp };
  }
  return best ? best.id : null;
}

/** 当前源列表用到了哪些卷 */
function usedDiskIds() {
  const set = new Set();
  for (const s of state.sources) {
    const id = mountIdOf(s.path);
    if (id) set.add(id);
  }
  return set;
}

/** 重画中栏磁盘网格（含「已被用作源 / 已选为目标」高亮） */
function paintDisks() {
  if (!state.disks.length && !diskLoaded) return; // 还没拉到时不要覆盖「正在检测…」
  const used = usedDiskIds();
  const targetId = state.target ? mountIdOf(state.target.path) : null;
  ui.renderDisks(state.disks, { usedIds: used, targetId, busy: state.running });
  ui.renderDiskSummary(state.disks);
  paintCapacity();
}

/**
 * 顶部胶囊：源总量 / 需写入量 vs 目标剩余空间 —— 直接回答「这块盘装得下吗」。
 */
function paintCapacity() {
  const srcBytes = state.sources.reduce((a, s) => a + (s.size || 0), 0);
  // 文件夹源在预扫描前拿不到体积（probe_path 只对文件给 size），
  // 这时宁可标成「待扫描」也别报一个偏小的数字骗人
  const partial = state.sources.some((s) => !(s.size > 0));
  const need = state.planBytes > 0 ? state.planBytes : partial ? 0 : srcBytes;
  ui.renderCapacity({
    srcBytes,
    needBytes: need,
    partial,
    freeBytes: state.target ? state.targetFree : null,
    hasTarget: !!state.target,
  });
}

/** 签名变化才重建 DOM；只有容量变化时走轻量文本更新 */
function diskSignature(list) {
  return list
    .map((d) => `${d.id}|${d.fs}|${d.kind}|${d.writable ? 1 : 0}|${d.label}`)
    .join(";");
}

function refreshDiskFreeText(list) {
  const byId = new Map(list.map((d) => [d.id, d]));
  document.querySelectorAll(".disk-card").forEach((el) => {
    const d = byId.get(el.dataset.id);
    if (!d) return;
    // 卡片里有两个 .disk-cap（容量 / 标签），取第一个即容量
    const cap = el.querySelector(".disk-cap");
    if (!cap) return;
    if (d.total === 0) cap.textContent = "容量未知";
    else if (d.free === 0) cap.textContent = "剩余 0 B";
    else cap.textContent = `剩余 ${fmtBytes(d.free)}`;
  });
  ui.renderDiskSummary(list);
  paintCapacity();
}

let diskSig = "";
let diskLoaded = false;

/**
 * 拉取本机磁盘。
 * @param {"init"|"manual"|"auto"|"tick"} reason 决定是否写日志
 */
async function loadDisks(reason = "auto") {
  let list;
  try {
    list = await listDisks();
  } catch (e) {
    $("diskSummary").textContent = "磁盘检测失败";
    ui.renderDisks([], {});
    if (reason === "manual") ui.pushLog("error", `磁盘检测失败：${e}`);
    return;
  }
  const sig = diskSignature(list);
  const changed = sig !== diskSig || !diskLoaded;
  state.disks = list;
  diskSig = sig;
  diskLoaded = true;

  if (changed) {
    paintDisks();
    if (reason === "init") {
      ui.pushLog("info", `已自动检测到 ${list.length} 个磁盘 / 卷，点击磁盘即可从该盘挑选源或目标。`);
    } else if (reason === "manual") {
      ui.pushLog("ok", `磁盘已刷新：检测到 ${list.length} 个卷。`);
    }
  } else {
    // 集合没变（多半只是剩余空间变化）→ 不重建 DOM，避免闪烁
    refreshDiskFreeText(list);
    if (reason === "manual") ui.pushLog("info", `磁盘无变化：${list.length} 个卷。`);
  }
}

/* ==================== 磁盘点击（中栏唯一一份网格） ==================== */
let diskMenuEl = null;

function closeDiskMenu() {
  if (diskMenuEl) {
    diskMenuEl.remove();
    diskMenuEl = null;
  }
}

/**
 * 磁盘卡本身只是「选中了哪块盘」，具体要干什么在弹出菜单里选：
 * 整盘当源 / 到盘里挑源 / 设为写入目标。
 */
function openDiskMenu(card) {
  closeDiskMenu();
  const mount = card.dataset.mount;
  const name = card.dataset.name;
  const id = card.dataset.id;
  const ro = card.dataset.writable === "0";
  const freeroom = card.dataset.freeroom;

  const el = document.createElement("div");
  el.className = "disk-menu";
  el.innerHTML = `
    <div class="dm-head">
      <span class="dm-name">${ui.esc(name)}</span>
      <span class="dm-mount mono">${ui.esc(mount)}</span>
    </div>
    <button data-act="whole">整盘加入备份源</button>
    <button data-act="browse">在该盘挑选文件 / 文件夹…</button>
    <button data-act="target"${ro ? " disabled" : ""}>设为备份目标</button>
    ${ro ? '<div class="dm-note">只读卷（macOS 原生 NTFS）不能作为写入目标</div>' : ""}
  `;
  document.body.appendChild(el);

  // 贴着卡片右侧弹出；越界就翻到左侧 / 往上收
  const r = card.getBoundingClientRect();
  const m = el.getBoundingClientRect();
  let x = r.right + 8;
  if (x + m.width > window.innerWidth - 10) x = Math.max(10, r.left - m.width - 8);
  let y = r.top - 6;
  if (y + m.height > window.innerHeight - 10) y = Math.max(10, window.innerHeight - m.height - 10);
  el.style.left = `${x}px`;
  el.style.top = `${y}px`;
  diskMenuEl = el;

  el.addEventListener("click", async (ev) => {
    const btn = ev.target.closest("button[data-act]");
    if (!btn || btn.disabled) return;
    const act = btn.dataset.act;
    closeDiskMenu();

    if (act === "whole") {
      ui.pushLog("info", `整盘加入源：${mount}`);
      await addSourcePaths([mount]);
      return;
    }
    if (act === "browse") {
      await addSourcesFromDialog("folder", mount);
      return;
    }
    // ---- 设为备份目标 ----
    if (ro) {
      ui.pushLog(
        "warn",
        `${mount} 是只读卷（macOS 原生 NTFS 只读），不能作为写入目标。请改用 exFAT / APFS / NTFS 第三方驱动挂载的卷。`
      );
      return;
    }
    if (freeroom === "0") {
      const d = state.disks.find((x2) => x2.id === id);
      ui.pushLog(
        "warn",
        `${mount} 报告剩余 0 B（${d?.kind === "network" ? "映射的网络盘可能未连接" : "磁盘可能已写满"}），` +
          "选它作目标大概率会立刻写失败。请换一块盘。"
      );
      return;
    }
    await pickTarget(mount);
  });
}

function bindDiskGrid() {
  $("diskGrid").addEventListener("click", (ev) => {
    const card = ev.target.closest(".disk-card");
    if (!card) return;
    if (state.running) {
      ui.pushLog("warn", "任务运行中，磁盘面板暂时只读。加源仍可用：直接拖入左栏，或用左栏的「＋」。");
      return;
    }
    openDiskMenu(card);
  });
  // 点别处 / 滚动 / 缩放窗口都收起菜单
  document.addEventListener("click", (ev) => {
    if (!ev.target.closest(".disk-menu") && !ev.target.closest(".disk-card")) closeDiskMenu();
  });
  window.addEventListener("resize", closeDiskMenu);
  document.addEventListener("scroll", closeDiskMenu, true);
}

/* ==================== 源列表操作 ==================== */
async function addSourcePaths(paths) {
  let added = 0;
  for (const p of paths) {
    if (state.sources.some((s) => normKey(s.path) === normKey(p))) {
      ui.pushLog("warn", `已存在，忽略：${p}`);
      continue;
    }
    try {
      const info = await probePath(p);
      state.sources.push(info);
      added++;
      ui.pushLog(
        "info",
        `已添加源 [${info.kind === "dir" ? "文件夹" : info.kind === "file" ? "文件" : "缺失"}] ${info.path}  (${info.fs})`
      );
    } catch (e) {
      ui.pushLog("error", `探测路径失败 ${p}：${e}`);
    }
  }
  ui.renderSources(state.sources);
  paintDisks();
  return added;
}

async function addSourcesFromDialog(mode, defaultPath) {
  let picked;
  try {
    picked = await open({
      multiple: true,
      directory: mode === "folder",
      defaultPath: defaultPath || undefined,
      title:
        mode === "folder"
          ? "选择一个或多个文件夹作为备份源"
          : "选择一个或多个文件作为备份源",
    });
  } catch (e) {
    ui.pushLog("error", `打开选择对话框失败：${e}`);
    return;
  }
  if (!picked) return;
  const list = Array.isArray(picked) ? picked : [picked];
  const wasRunning = state.running;
  const added = await addSourcePaths(list);
  if (added) {
    ui.pushLog("ok", `本次新增 ${added} 个源，共 ${state.sources.length} 个。`);
    if (wasRunning) queueRun("运行中加入了新素材");
  }
}

function clearSources() {
  if (state.running) {
    ui.pushLog("warn", "任务运行中不能清空源列表（正在跑的这轮会失控）。等结束后再清。");
    return;
  }
  state.sources = [];
  ui.renderSources(state.sources);
  paintDisks();
  ui.pushLog("info", "已清空源列表。");
}

$("srcList").addEventListener("click", (ev) => {
  const btn = ev.target.closest("button[data-del]");
  if (!btn) return;
  if (state.running) {
    ui.pushLog("warn", "任务运行中不能移除源；等这轮结束后再改，或先点「取消任务」。");
    return;
  }
  const i = Number(btn.dataset.del);
  const removed = state.sources.splice(i, 1)[0];
  ui.renderSources(state.sources);
  paintDisks();
  if (removed) ui.pushLog("info", `已移除源：${removed.path}`);
});

/* ==================== 目标文件夹 ==================== */

/**
 * 统一的目标设置入口：对话框 / 拖拽 都走这里。
 * @param {string} path 目标文件夹的绝对路径
 * @param {"dialog"|"drop"} origin 来源，仅影响日志措辞
 */
async function setTargetPath(path, origin = "dialog") {
  state.target = { path, fs: await fsType(path) };
  const free = await freeSpace(path);
  state.targetFree = free === null || free === undefined ? null : free;
  state.planBytes = 0; // 换了目标，之前预扫描的结论作废
  ui.renderTarget(state.target, free);
  paintDisks();
  ui.pushLog(
    "info",
    `目标文件夹${origin === "drop" ? "（来自拖拽）" : ""}：${path}  (${state.target.fs})，剩余空间 ${fmtBytes(free)}`
  );
  if (free === 0) {
    ui.pushLog("warn", "该目标盘报告剩余 0 B（可能未连接或已写满），继续备份大概率会立刻写失败。");
  }
  if (String(state.target.fs).toLowerCase().includes("ntfs")) {
    ui.pushLog(
      "warn",
      "目标盘为 NTFS：Windows 下可正常读写；macOS 原生仅只读，写入需第三方驱动（Paragon NTFS / Tuxera / Mounty）。"
    );
  }
}

async function pickTarget(defaultPath) {
  if (state.running) return;
  let dir;
  try {
    dir = await open({
      directory: true,
      multiple: false,
      defaultPath: defaultPath || undefined,
      title: "选择目标文件夹（备份文件存放位置）",
    });
  } catch (e) {
    ui.pushLog("error", `打开选择对话框失败：${e}`);
    return;
  }
  if (!dir) return;
  const path = Array.isArray(dir) ? dir[0] : dir;
  await setTargetPath(path, "dialog");
}

/* ==================== 拖拽投放 ==================== */

/** 取父目录（跨平台，顺带处理 Windows 盘符根） */
function dirName(p) {
  const s = String(p || "").replace(/[\\/]+$/, "");
  const i = Math.max(s.lastIndexOf("/"), s.lastIndexOf("\\"));
  if (i < 0) return "";
  const head = s.slice(0, i);
  if (/^[A-Za-z]:$/.test(head)) return `${head}\\`; // D: -> D:\
  return head || "/";
}

/**
 * 拖到「备份源」区域 → 全部作为源加入。
 * 任务运行中**也允许**：源列表加完就排一轮追加备份（见 queueRun）。
 */
async function dropToSources(paths) {
  const wasRunning = state.running;
  ui.pushLog("info", `拖入 ${paths.length} 项，按备份源处理。`);
  const added = await addSourcePaths(paths);
  if (added) {
    ui.pushLog("ok", `拖拽新增 ${added} 个源，共 ${state.sources.length} 个。`);
    if (wasRunning) queueRun("拖入了新素材");
  } else {
    ui.pushLog("warn", "拖入的路径没有新增任何源（可能已存在或路径无效）。");
  }
}

/**
 * 拖到「目标文件夹」区域 → 推断该用哪个文件夹当目标。
 * 规则：优先取第一个存在的文件夹；若拖的全是文件，则取第一个文件所在目录。
 * 任务运行中改目标会把「已在写入的位置」换掉，所以直接拒绝。
 */
async function dropToTarget(paths) {
  if (state.running) {
    ui.pushLog("warn", "任务运行中不能更换目标文件夹；等这轮结束或先点「取消任务」。");
    return;
  }
  const infos = [];
  for (const p of paths) {
    try {
      infos.push(await probePath(p));
    } catch (e) {
      ui.pushLog("warn", `无法识别拖入的路径 ${p}：${e}`);
    }
  }
  if (!infos.length) {
    ui.pushLog("error", "拖入的内容无法识别，已忽略。");
    return;
  }

  const dirs = infos.filter((i) => i.exists && i.kind === "dir");
  let path = "";
  let note = "";
  if (dirs.length) {
    path = dirs[0].path;
    if (dirs.length > 1) note = `（拖入 ${dirs.length} 个文件夹，已使用第一个）`;
  } else {
    const file = infos.find((i) => i.exists && i.kind === "file");
    if (!file) {
      ui.pushLog("error", "拖入的内容既不是文件夹也不是文件，已忽略。");
      return;
    }
    path = dirName(file.path);
    note = "（拖入的是文件，已取其所在文件夹）";
  }
  if (!path) {
    ui.pushLog("error", "无法从拖入内容推断出目标文件夹，已忽略。");
    return;
  }
  await setTargetPath(path, "drop");
  if (note) ui.pushLog("info", `目标来源说明${note}`);
}

/* ==================== 中栏：传输列表（一行一个源） ==================== */

/** 源路径 → 该源类型（挑图标用） */
function srcKindOf(path) {
  const hit = state.sources.find((s) => normKey(s.path) === normKey(path));
  return hit ? hit.kind : "dir";
}

/** 当前文件相对源根的路径（照参考图那样显示 子目录/文件名） */
function relPathOf(file, root) {
  if (!file) return "";
  const f = String(file).replace(/\\/g, "/");
  const r = String(root || "").replace(/\\/g, "/").replace(/\/+$/, "");
  if (r && f.toLowerCase().startsWith(r.toLowerCase() + "/")) return f.slice(r.length + 1);
  return ui.baseName(f);
}

/**
 * 标题行右侧要显示的「目标侧位置」= 源名 + 文件在源里的相对**目录**。
 * 参考图那里放的是路径而不是文件名，文件名留给进度条下方那一行。
 */
function targetPathOf(file, root) {
  const base = ui.baseName(root) || "";
  if (!file) return base;
  const rel = relPathOf(file, root);
  const cut = rel.lastIndexOf("/");
  const dir = cut > 0 ? rel.slice(0, cut) : "";
  return dir ? `${base}/${dir}` : base;
}

/**
 * 用后端推来的「按源进度」刷新传输列表。
 * 校验阶段后端不带按源数据 → 沿用上一批行，只有总进度在动。
 */
function paintTransfers(p) {
  if (!state.runRows.length) return;
  const dstName = state.target ? ui.baseName(state.target.path) || state.target.path : "";
  const speed = p?.speedBps || 0;
  const rows = state.runRows.map((s) => {
    const total = s.bytesTotal || 0;
    const done = s.bytesDone || 0;
    const remain = Math.max(0, total - done);
    // 串行拷贝 ⇒ 同一时刻只有一个源在动，用全局速度给它估剩余时间
    const eta = s.state === "active" && speed > 1 ? remain / speed : 0;
    const name = ui.baseName(s.path) || s.path;
    const tp = targetPathOf(s.currentFile, s.path);
    return {
      path: s.path,
      kind: srcKindOf(s.path),
      name,
      dstName,
      // 「位置」跟源名一样时没信息量，留空更清爽
      relPath: tp === name ? "" : tp,
      state: s.state || "waiting",
      bytesTotal: total,
      bytesDone: done,
      currentFile: s.currentFile ? ui.baseName(s.currentFile) : "",
      speedBps: s.state === "active" ? speed : 0,
      etaSecs: eta,
    };
  });
  ui.renderTransfers(rows);
  paintMid();
}

/* ==================== 中栏视图：磁盘 ⇄ 传输 ==================== */

/** 按 state 重画中栏（模式 / 副标题 / 总进度条显隐） */
function paintMid() {
  const n = state.runRows.length;
  const isTr = state.midView === "transfers";
  ui.renderMidMode(state.midView, {
    sub: isTr ? `${n} 个源` : n ? `${n} 个源传输中` : "",
    showTotal: isTr || n > 0 || state.running,
  });
  syncFoldButton();
}

/** 折叠按钮的图标 / tooltip 随「当前看的是哪一面」变化 */
function syncFoldButton() {
  if (state.midView === "transfers") {
    ui.setFoldHint({ icon: "⌄", title: "折叠传输列表，改看本机磁盘", on: true });
  } else if (state.runRows.length || state.running) {
    ui.setFoldHint({ icon: "⌃", title: "回到传输列表看每源进度", on: true });
  } else {
    ui.setFoldHint({
      icon: state.midFolded ? "⌃" : "⌄",
      title: state.midFolded ? "展开本机磁盘" : "收起本机磁盘",
      on: false,
    });
  }
}

/**
 * 切换中栏展示。
 * 运行中也能切回磁盘视图（看剩余空间 / 插了哪块盘），任务照跑不误。
 * @param {"disks"|"transfers"} view
 */
function setMidView(view) {
  state.midView = view;
  if (view === "transfers" && state.midFolded) {
    state.midFolded = false;
    ui.setMidFolded(false);
  }
  paintMid();
}

/** 进入传输视图，先用已选源铺出占位行（预扫描阶段后端还没算出按源数据） */
function enterTransfers() {
  state.phase = "scan";
  state.runRows = state.sources.map((s) => ({
    index: 0,
    path: s.path,
    // 占位行标成 "scan" 而不是 "waiting"：现在确实在预扫描，不是干等
    state: "scan",
    currentFile: "",
    bytesTotal: s.size || 0,
    bytesDone: 0,
    filesTotal: 0,
    filesDone: 0,
  }));
  state.midView = "transfers";
  state.midFolded = false;
  ui.setMidFolded(false);
  paintTransfers({ speedBps: 0 });
}

/**
 * 预扫描阶段把「后端当前扫到的文件」映射到它归属的那一行，
 * 只有那一行显示流动条纹 + 当前文件名，其余显示「预扫描中…」。
 * @param {{current?:string}} p cb:scan 的载荷
 */
function paintScanRows(p) {
  if (state.phase !== "scan" || !state.runRows.length) return;
  const cur = p.current || "";
  const hit = cur ? state.runRows.findIndex((r) => normKey(cur).startsWith(normKey(r.path) + "/") || normKey(cur) === normKey(r.path)) : -1;
  let changed = false;
  state.runRows = state.runRows.map((s, i) => {
    if (i === hit) {
      if (s.state === "scanning" && s.currentFile === cur) return s;
      changed = true;
      return { ...s, state: "scanning", currentFile: cur };
    }
    if (s.state === "scanning") {
      changed = true;
      return { ...s, state: "scan", currentFile: "" };
    }
    return s;
  });
  if (changed) paintTransfers({ speedBps: 0 });
}

/* ==================== 追加一轮（运行中加源） ==================== */

/**
 * 排一轮追加备份。
 * 同一时间后端只允许一个任务（`AppState.busy`），所以运行中不硬闯，
 * 而是记一个标记：本轮 JOB_END 之后自动再跑一轮。
 *
 * ⚠️ 「重跑一轮」**并不便宜** —— 这是 0.4.3 修掉的一个真机 bug：
 * 判定「已备份 → 跳过」靠的是内容哈希，而 `hash::files_identical`
 * 要把**源和目标各完整读一遍**。所以带着全部源重跑，等于把上一轮刚写进目标的内容
 * 从头再读两份。素材盘 1 TB → 空转约 2 TB 的读取时间；这期间中栏是
 * `enterTransfers()` 铺的 `waiting` 占位行（预扫描只发 SCAN、不发 PROGRESS），
 * 所以每一行都显示「排队中」，看着就像「排队了但什么都没拷」。
 * 因此追加的那轮**只发新加进来的源**（见 `run(dryRun, "append")`）。
 * 回归测试：`src-tauri/tests/resume_rule.rs` 的 `t13_second_round_rereads_everything_already_copied`。
 *
 * 一句话记住：「备份幂等」在**结果**上成立，在**代价**上不成立，别再按后者写代码。
 */
function queueRun(reason = "") {
  if (!state.running) {
    run(false);
    return;
  }
  if (state.pendingRun) return;
  state.pendingRun = true;
  ui.pushLog(
    "info",
    `${reason ? reason + "：" : ""}已排队，本轮任务结束后自动补跑新加的源（已备完的源不再重扫）。不想跑就点「取消排队」。`
  );
  ui.renderStartButton({ running: true, queued: true });
}

function cancelPendingRun() {
  if (!state.pendingRun) return;
  state.pendingRun = false;
  ui.pushLog("info", "已取消排队的追加备份。");
  ui.renderStartButton({ running: state.running, queued: false });
}

function togglePendingRun() {
  if (state.pendingRun) cancelPendingRun();
  else queueRun("手动追加");
}

/* ==================== 选项 / 组装请求 ==================== */
function collectOptions() {
  return {
    askOnConflict: $("optAsk").checked,
    quickScan: $("optQuick").checked,
    resumePrefixCheck: $("optPrefix").checked,
    verifyAfterCopy: $("optVerify").checked,
    // "sha256"（默认）| "xxh64" —— 预扫描查重 / 续传前缀 / 最终校验都用它
    hashAlgo: $("optAlgo").value,
  };
}

/** 算法选择器改名时，结果表头与说明文案一起跟上 */
function refreshAlgoUi() {
  const algo = $("optAlgo").value;
  const label = algo === "xxh64" ? "xxHash64" : "SHA-256";
  ui.setHashHeader(label);
  const note = $("algoNote");
  if (note) {
    note.innerHTML =
      algo === "xxh64"
        ? "吞吐高（约 5~10 GB/s），但摘要只有 16 位十六进制，不适合对外核对"
        : "标准校验和，可与 <code>shasum -a 256</code> / <code>certutil -hashfile</code> 对照";
  }
}
function guard() {
  if (state.running) {
    ui.pushLog("warn", "已有任务正在运行，请等待完成或先取消。");
    return false;
  }
  if (state.sources.length === 0) {
    ui.pushLog("error", "请先添加至少一个源。");
    return false;
  }
  if (!state.target) {
    ui.pushLog("error", "请先选择目标文件夹。");
    return false;
  }
  return true;
}

/* ==================== 开始 / 试运行 ==================== */

/**
 * 开始一轮备份。
 * @param {boolean} dryRun
 * @param {"full"|"append"} [mode]
 *   - `"full"`（默认）：把左栏当前的源全发一遍 —— 用户主动点「开始备份」/「试运行」
 *   - `"append"`：运行中加源后自动追的那一轮 —— **只发新加进来的源**
 */
async function run(dryRun, mode = "full") {
  if (!guard()) return;
  const allPaths = state.sources.map((s) => s.path);
  let paths = allPaths;

  if (mode === "append") {
    paths = allPaths.filter((p) => !state.roundKeys.has(normKey(p)));
    if (paths.length === 0) {
      ui.pushLog("info", "追加一轮：没有新加的源，无需再跑。");
      state.pendingRun = false;
      ui.renderStartButton({ running: false, queued: false });
      return;
    }
    ui.pushLog(
      "info",
      `追加一轮：只补新加的 ${paths.length} 个源` +
        (allPaths.length > paths.length
          ? `（上一轮已完成的 ${allPaths.length - paths.length} 个源不再重扫）`
          : "") +
        "。要重跑全部源，直接点「开始备份」。"
    );
  }

  // 记下本轮发出的**全部**源（含被跳过的），下一轮的增量就相对它来算
  state.roundKeys = new Set(allPaths.map(normKey));

  const req = {
    sources: paths,
    target: state.target.path,
    options: collectOptions(),
  };
  state.running = true;
  state.planBytes = 0;
  ui.resetProgress();
  refreshAlgoUi(); // 结果表的「校验值」列头要跟本次任务实际用的算法一致
  ui.resetResults();
  ui.renderStatus("scanning");
  // 中栏由「磁盘」切到「传输」：收起磁盘网格，改看每个源自己的进度条
  enterTransfers();
  ui.renderStartButton({ running: true, queued: state.pendingRun });
  paintDisks(); // 磁盘卡转入「运行中」状态，不再接受点击
  ui.pushLog("info", dryRun ? "===== 开始试运行 Dry Run（不写入任何数据）=====" : "===== 开始备份任务 =====");
  try {
    await startJob(req, dryRun);
  } catch (e) {
    state.running = false;
    ui.renderStatus("idle");
    paintDisks();
    ui.pushLog("error", `启动任务失败：${e}`);
  }
}

async function cancel() {
  ui.pushLog("warn", "已请求取消任务，正在安全停止…");
  try {
    await cancelJob();
  } catch (e) {
    ui.pushLog("error", `取消失败：${e}`);
  }
}

/* ==================== 任务保存 / 加载 ==================== */
async function saveTask() {
  if (state.sources.length === 0) {
    ui.pushLog("error", "源列表为空，无需保存。");
    return;
  }
  let path;
  try {
    path = await save({
      title: "保存任务文件",
      defaultPath: "cinebackup-task.json",
      filters: [{ name: "CineBackup 任务", extensions: ["json"] }],
    });
  } catch (e) {
    ui.pushLog("error", `保存对话框失败：${e}`);
    return;
  }
  if (!path) return;
  const task = {
    version: 1,
    app: "CineBackup",
    createdAt: new Date().toISOString(),
    targetDir: state.target?.path || "",
    sources: state.sources.map((s) => s.path),
    options: collectOptions(),
  };
  try {
    await saveTaskFile(path, task);
    ui.pushLog("ok", `任务已保存：${path}`);
  } catch (e) {
    ui.pushLog("error", `任务保存失败：${e}`);
  }
}

async function loadTask() {
  let path;
  try {
    path = await open({
      multiple: false,
      title: "加载任务文件",
      filters: [{ name: "CineBackup 任务", extensions: ["json"] }],
    });
  } catch (e) {
    ui.pushLog("error", `打开对话框失败：${e}`);
    return;
  }
  if (!path) return;
  const file = Array.isArray(path) ? path[0] : path;
  let task;
  try {
    task = await loadTaskFile(file);
  } catch (e) {
    ui.pushLog("error", `任务加载失败：${e}`);
    return;
  }
  // 重新探测每个源（文件系统信息不入 JSON，每次加载重新读取）
  state.sources = [];
  for (const p of task.sources || []) {
    try {
      const info = await probePath(p);
      state.sources.push(info);
      if (!info.exists) ui.pushLog("warn", `源路径不存在（可能盘未挂载或路径来自另一平台）：${p}`);
    } catch (e) {
      ui.pushLog("warn", `跳过无效源 ${p}：${e}`);
    }
  }
  state.target = null;
  state.targetFree = null;
  state.planBytes = 0;
  if (task.targetDir) {
    state.target = { path: task.targetDir, fs: await fsType(task.targetDir) };
    const free = await freeSpace(task.targetDir);
    state.targetFree = free === null || free === undefined ? null : free;
    ui.renderTarget(state.target, free);
  } else {
    ui.renderTarget(null, null);
  }
  const o = task.options || {};
  if (typeof o.askOnConflict === "boolean") $("optAsk").checked = o.askOnConflict;
  if (typeof o.quickScan === "boolean") $("optQuick").checked = o.quickScan;
  if (typeof o.resumePrefixCheck === "boolean") $("optPrefix").checked = o.resumePrefixCheck;
  if (typeof o.verifyAfterCopy === "boolean") $("optVerify").checked = o.verifyAfterCopy;
  // 老任务文件没有这个字段 → 保持当前选择（默认就是 SHA-256）
  if (o.hashAlgo === "xxh64" || o.hashAlgo === "sha256") $("optAlgo").value = o.hashAlgo;
  refreshAlgoUi();

  ui.renderSources(state.sources);
  paintDisks();
  ui.pushLog("ok", `任务已加载：${file}，共 ${state.sources.length} 个源，目标 ${state.target?.path || "（缺失）"}`);
}

/* ==================== 后端事件订阅 ==================== */
function subscribe() {
  on(EV.LOG, (p) => ui.pushLog(p.level || "info", p.message, p.ts));
  on(EV.STATUS, (p) => ui.renderStatus(p.status));

  on(EV.SCAN, (p) => {
    ui.renderScanProgress(p);
    paintScanRows(p);
  });

  on(EV.PROGRESS, (p) => {
    state.phase = p.phase === "verify" ? "verify" : "copy";
    ui.renderProgress({ ...p, phase: p.phase === "verify" ? "verify" : "copy" });
    // 后端只在拷贝阶段带按源数据；校验阶段沿用上一批行，只让总进度继续动
    if (Array.isArray(p.sources) && p.sources.length) state.runRows = p.sources;
    paintTransfers(p);
  });

  on(EV.PLAN, (p) => {
    state.planBytes = p.totalBytes || 0;
    paintCapacity();
    // 预扫描收工 → 占位行从「预扫描中」变「排队中」（马上就开拷）
    state.phase = "planned";
    state.runRows = state.runRows.map((s) =>
      s.state === "scan" || s.state === "scanning" ? { ...s, state: "waiting", currentFile: "" } : s
    );
    paintTransfers({ speedBps: 0 });
    ui.pushLog(
      "info",
      `预扫描完成：完整拷贝 ${p.copy}，断点续传 ${p.resume}，跳过(内容一致) ${p.skip}，` +
        `覆盖 ${p.overwrite}，待询问 ${p.conflict}；需写入 ${fmtBytes(p.totalBytes)}` +
        (p.filtered ? `；已过滤系统元数据 ${p.filtered} 项` : "")
    );
  });

  on(EV.CONFLICT, async (p) => {
    ui.pushLog("warn", `冲突：${p.src}`);
    ui.showConflict(p, async (reply) => {
      await replyDecision(reply);
    });
  });

  on(EV.COPY_ERROR, async (p) => {
    ui.showError(p, async (reply) => {
      await replyDecision(reply);
    });
  });

  on(EV.FILE_RESULT, (p) => {
    ui.addResult(p);
    if (p.status === "pass") ui.bumpCounter("pass");
    else if (p.status === "fail" || p.status === "error") ui.bumpCounter("fail");
    else ui.bumpCounter("skip");
  });

  on(EV.JOB_END, (p) => {
    state.running = false;
    state.phase = "idle";
    // 收尾：本轮一个文件都没拷（全跳过 / 空目录 / 中途取消）时，占位行会停在
    // 「预扫描中」「排队中」这种非终态，任务都结束了还挂着流动条纹 → 收干净
    const leftover = state.runRows.some((s) => s.state === "scan" || s.state === "scanning" || s.state === "waiting");
    if (leftover) {
      state.runRows = state.runRows.map((s) =>
        s.state === "scan" || s.state === "scanning" || s.state === "waiting"
          ? { ...s, state: p.ok && !p.aborted ? "skipped" : "waiting", currentFile: "" }
          : s
      );
      paintTransfers({ speedBps: 0 });
    }
    ui.hideModals();
    ui.renderStatus(p.ok ? "done" : "idle");
    ui.markProgressDone(p.ok && !p.aborted);
    if (p.dryRun) {
      ui.pushLog("ok", "===== 试运行结束（未写入任何数据）=====");
    } else if (p.aborted) {
      ui.pushLog("warn", "===== 任务已中止 =====");
    } else {
      ui.pushLog("ok", "===== 任务结束 =====");
    }
    ui.pushLog(
      p.failed > 0 ? "warn" : "ok",
      `汇总：拷贝 ${p.copied} · 续传 ${p.resumed} · 覆盖 ${p.overwritten} · 跳过 ${p.skipped} · ` +
        `校验通过 ${p.pass} · 失败 ${p.failed}；共写入 ${fmtBytes(p.totalBytes)}，耗时 ${p.elapsedSecs.toFixed(1)}s`
    );
    if (p.message) ui.pushLog(p.ok ? "info" : "error", p.message);
    // 盘上数据变了 → 刷新剩余空间，并让磁盘卡脱离「运行中」状态
    loadDisks("auto").then(() => paintDisks());

    // 运行中加过源 → 自动再跑一轮（用户主动取消的那一轮不追）
    if (state.pendingRun) {
      if (p.aborted) {
        state.pendingRun = false;
        ui.pushLog("warn", "任务被中止 → 已取消排队的追加备份。");
      } else {
        state.pendingRun = false;
        if (p.failed > 0) {
          ui.pushLog(
            "warn",
            `上一轮有 ${p.failed} 个文件失败；追加一轮只补新加的源，不会自动重试它们 —— ` +
              "需要的话请点「开始备份」重跑全部源。"
          );
        }
        ui.pushLog("info", "排队的追加备份开始（只补运行中新加的源，已备完的源不重扫）…");
        setTimeout(() => {
          if (!state.running) run(false, "append");
        }, 500);
      }
    }
    ui.renderStartButton({ running: state.running, queued: state.pendingRun });
  });
}

/* ==================== 按钮绑定 ==================== */
$("btnAddSource").addEventListener("click", (ev) => {
  ev.stopPropagation();
  $("addSourceMenu").classList.toggle("hidden");
});
$("addSourceMenu").addEventListener("click", (ev) => {
  const b = ev.target.closest("button[data-mode]");
  if (!b) return;
  $("addSourceMenu").classList.add("hidden");
  addSourcesFromDialog(b.dataset.mode);
});
document.addEventListener("click", () => $("addSourceMenu").classList.add("hidden"));

$("btnClearSources").addEventListener("click", clearSources);
$("btnPickTarget").addEventListener("click", () => pickTarget());
$("btnRefreshDisks").addEventListener("click", () => loadDisks("manual"));
// 运行中点「开始备份」= 追加一轮（排队到本轮结束后），不打断正在跑的这轮
$("btnStart").addEventListener("click", () => {
  if (state.running) {
    togglePendingRun();
    return;
  }
  run(false);
});
$("btnDry").addEventListener("click", () => run(true));
$("btnCancel").addEventListener("click", cancel);
$("btnSaveTask").addEventListener("click", saveTask);
$("btnLoadTask").addEventListener("click", loadTask);
$("btnClearLog").addEventListener("click", () => ui.clearLog());

// 任务选项弹出面板
$("btnOptions").addEventListener("click", (ev) => {
  ev.stopPropagation();
  $("optMenu").classList.toggle("hidden");
});
document.addEventListener("click", (ev) => {
  if (!ev.target.closest("#optWrap")) $("optMenu").classList.add("hidden");
});
// 换算法 → 结果表头 / 说明文案立刻跟上（真正生效是下一次任务开始时）
$("optAlgo").addEventListener("change", () => {
  refreshAlgoUi();
  const algo = $("optAlgo").value === "xxh64" ? "xxHash64" : "SHA-256";
  ui.pushLog("info", `校验算法已切换为 ${algo}（对下一次开始的任务生效）。`);
});

// 中栏右上角箭头：
//   传输视图 → 折叠传输列表、切回磁盘网格
//   磁盘视图（还有传输行 / 任务在跑）→ 回到传输列表
//   空闲且没有传输行 → 收起 / 展开磁盘网格本身
$("btnFoldMid").addEventListener("click", () => {
  if (state.midView === "transfers") {
    setMidView("disks");
    if (!state.running) loadDisks("auto");
    return;
  }
  if (state.runRows.length || state.running) {
    setMidView("transfers");
    return;
  }
  state.midFolded = !state.midFolded;
  ui.setMidFolded(state.midFolded);
  syncFoldButton();
});

bindDiskGrid();

// 系统级拖放：拖到左边加入源、拖到右边设为目标，其它区域默认按「加入源」处理。
// 任务运行中只放行「加源」（加完自动排队再跑一轮），改目标会被拦下。
initDragDrop({
  canDrop: (zone) => zone !== "target" || !state.running,
  isRunning: () => state.running,
  onDropSources: dropToSources,
  onDropTarget: dropToTarget,
  onStatus: (ok) => {
    if (!ok) {
      ui.pushLog("warn", "拖放功能未启用（无法订阅系统拖放事件），请改用「添加源」按钮选择文件 / 文件夹。");
    }
  },
});

// 运行中锁定编辑类操作（加源不锁：运行中加源是支持的，加完会自动排队跑下一轮）
const lockables = ["btnPickTarget", "btnSaveTask", "btnLoadTask"];
function refreshLock() {
  const lock = state.running;
  ui.renderStatus($("statusPill").dataset.s || "idle");
  lockables.forEach((id) => {
    const el = $(id);
    if (el) el.disabled = lock;
  });
  // 清空源、逐项删除在运行中仍然禁用（会让正在跑的这轮失控）
  const cls = $("btnClearSources");
  if (cls) cls.disabled = lock || state.sources.length === 0;
  ui.renderStartButton({ running: lock, queued: state.pendingRun });
  // 磁盘卡：运行中标记 busy（点了只给提示，不弹操作菜单）
  document.querySelectorAll(".disk-card").forEach((c) => c.classList.toggle("is-busy", lock));
}

/* ==================== 启动 ==================== */
subscribe();
ui.resetProgress();
refreshAlgoUi();
ui.resetResults();
ui.renderSources(state.sources);
ui.renderTarget(null, null);
ui.setMidFolded(false);
paintMid();
refreshLock();
loadDisks("init");

// 窗口重新获得焦点（插拔硬盘后切回来）自动刷新
window.addEventListener("focus", () => {
  if (!state.running) loadDisks("auto");
});
// 轻量轮询：只在窗口可见且没有任务在跑时执行，插盘即出现
setInterval(() => {
  if (state.running) return;
  if (typeof document.hasFocus === "function" && !document.hasFocus()) return;
  loadDisks("tick");
}, 6000);

setInterval(refreshLock, 400);
ui.pushLog(
  "info",
  "CineBackup 就绪。中栏列的是本机磁盘：点磁盘选择「当源 / 当目标」，也可以直接把文件 / 文件夹拖进左右两栏。"
);
