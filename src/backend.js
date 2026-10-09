/**
 * backend.js — 前端与 Rust 后端之间的唯一桥梁
 * 所有 Tauri command 名称、事件名称集中在这里，方便和后端对齐。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";

/** 事件名（与 Rust 端 events.rs 一一对应） */
export const EV = {
  LOG: "cb:log",
  STATUS: "cb:status",
  SCAN: "cb:scan",
  PROGRESS: "cb:progress",
  PLAN: "cb:plan",
  CONFLICT: "cb:conflict",
  COPY_ERROR: "cb:copy-error",
  FILE_RESULT: "cb:file-result",
  JOB_END: "cb:job-end",
  COMPARE_PROGRESS: "cb:compare-progress",
  COMPARE_DONE: "cb:compare-done",
  CLOSE_REQUESTED: "cb:close-requested",
};

/** Tauri command 名称（与 Rust 端 commands.rs 一一对应） */
export const CMD = {
  APP_VERSION: "app_version",
  PROBE_PATH: "probe_path",
  FS_TYPE: "fs_type",
  FREE_SPACE: "free_space",
  LIST_DISKS: "list_disks",
  START_JOB: "start_job",
  REPLY: "reply_decision",
  CANCEL: "cancel_job",
  SAVE_TASK: "save_task_file",
  LOAD_TASK: "load_task_file",
  IS_BUSY: "is_busy",
  SKIP_CURRENT_VERIFY: "skip_current_verify",
  START_COMPARE: "start_compare",
  CANCEL_COMPARE: "cancel_compare",
  CLOSE_APP: "close_app",
};

/**
 * 自动拉取本机磁盘 / 卷列表。
 * @returns {Promise<{id:string,mount:string,label:string,fs:string,total:number,free:number,kind:string,writable:boolean}[]>}
 */
export function listDisks() {
  return invoke(CMD.LIST_DISKS);
}

/** 取得应用版本号（标题栏显示）。非 Tauri 环境（浏览器预览）会抛错，调用方自行兜底 */
export function appVersion() {
  return invoke(CMD.APP_VERSION);
}

/** 探测单个路径：类型 / 文件系统 / 大小 */
export function probePath(path) {
  return invoke(CMD.PROBE_PATH, { path });
}

/** 取得某路径所在卷的文件系统类型（NTFS / APFS / exFAT …） */
export function fsType(path) {
  return invoke(CMD.FS_TYPE, { path });
}

/** 目标盘剩余空间（字节），失败返回 null */
export async function freeSpace(path) {
  try {
    return await invoke(CMD.FREE_SPACE, { path });
  } catch {
    return null;
  }
}

/** 启动备份（dryRun=true 时为试运行，不写任何数据） */
export function startJob(req, dryRun = false) {
  return invoke(CMD.START_JOB, { req: { ...req, dryRun } });
}

/** 回答冲突 / 错误弹窗 */
export function replyDecision(reply) {
  return invoke(CMD.REPLY, { reply });
}

/** 取消当前任务 */
export function cancelJob() {
  return invoke(CMD.CANCEL);
}

/** 保存 / 加载任务 JSON */
export function saveTaskFile(path, task) {
  return invoke(CMD.SAVE_TASK, { path, task });
}
export function loadTaskFile(path) {
  return invoke(CMD.LOAD_TASK, { path });
}
export function isBusy() {
  return invoke(CMD.IS_BUSY);
}

/**
 * 跳过「当前正在校验的文件」，继续校验下一个。
 *
 * 与「取消任务」不是一回事：取消会中断整个任务，这个只放弃当前这一个文件
 * （大文件、尤其网络盘上几百 GB 的，读一半就够了；不必等它读完，也不必取消整轮）。
 * 被跳过的文件在结果表里记「跳过」，**不计入失败**。
 *
 * @returns {Promise<boolean>} false = 当前不在校验阶段，请求未被受理
 *   （后端靠 `in_verify` 判定：拷贝阶段点「跳过」没有意义，不该被悄悄记下来）
 */
export function skipCurrentVerify() {
  return invoke(CMD.SKIP_CURRENT_VERIFY);
}

/**
 * 启动对比校验（只读，独立于备份任务）。
 * 进度 / 结果通过 `cb:compare-progress` / `cb:compare-done` 事件推送。
 * @param {string} left 左侧路径（文件或文件夹）
 * @param {string} right 右侧路径（类型需与左侧一致）
 * @param {{hashAlgo?:string, quick?:boolean}} options
 */
export function startCompare(left, right, options = {}) {
  return invoke(CMD.START_COMPARE, { left, right, options });
}

/** 取消正在进行的对比 */
export function cancelCompare() {
  return invoke(CMD.CANCEL_COMPARE);
}

/**
 * 前端确认退出后调用：立即退出进程（后端 `app.exit(0)`）。
 *
 * 只能由「关闭确认弹窗」的用户确认动作触发；不要在其他地方误调，
 * 否则会绕过所有确认直接关掉应用。
 */
export function closeApp() {
  return invoke(CMD.CLOSE_APP);
}

/** 订阅某个后端事件，返回 unlisten 函数 */
export function on(event, handler) {
  return listen(event, (e) => handler(e.payload));
}

/**
 * 订阅系统级文件 / 文件夹拖放（从资源管理器、Finder 拖进窗口）。
 *
 * Tauri 默认接管 webview 的原生拖放，路径通过该事件传给前端，
 * 因此这里能拿到绝对路径（HTML5 的 drop 事件在 Tauri 下拿不到路径）。
 *
 * payload 形如：
 *   { type: "enter" | "over", paths?: string[], position: {x, y} }  // position 为物理像素
 *   { type: "drop",  paths: string[],      position: {x, y} }
 *   { type: "leave" }
 *
 * @param {(p:{type:string,paths?:string[],position?:{x:number,y:number}})=>void} handler
 * @returns {Promise<() => void>} unlisten 函数（环境不支持时返回空函数，并置 subscribed=false）
 */
export async function onDragDrop(handler) {
  try {
    const unlisten = await getCurrentWebview().onDragDropEvent((e) => handler(e.payload));
    onDragDrop.subscribed = true;
    return unlisten;
  } catch (err) {
    // 非 Tauri 环境（例如浏览器里跑 UI 预览）静默降级，只留一条诊断日志
    console.warn("[魔王拷贝] 拖放事件订阅不可用：", err);
    onDragDrop.subscribed = false;
    return () => {};
  }
}
/** 上一次 onDragDrop 是否订阅成功（供启动日志提示用） */
onDragDrop.subscribed = false;

/** 字节格式化 */
export function fmtBytes(n) {
  if (n === null || n === undefined || Number.isNaN(n)) return "—";
  const u = ["B", "KB", "MB", "GB", "TB", "PB"];
  let i = 0;
  let v = Number(n);
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 || i === 0 ? v.toFixed(0) : v.toFixed(2)} ${u[i]}`;
}

/** 时长格式化：秒 → 1h 23m / 4m 05s */
export function fmtDuration(sec) {
  if (!Number.isFinite(sec) || sec <= 0) return "—";
  const s = Math.round(sec);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  if (h > 0) return `${h}h ${String(m).padStart(2, "0")}m`;
  if (m > 0) return `${m}m ${String(ss).padStart(2, "0")}s`;
  return `${ss}s`;
}

/** 速度格式化 */
export function fmtSpeed(bps) {
  if (!Number.isFinite(bps) || bps <= 0) return "—";
  return `${fmtBytes(bps)}/s`;
}
