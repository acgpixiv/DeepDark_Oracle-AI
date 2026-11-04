// 渲染崩溃记录（REQ-108 的"崩溃上报 + 本地缓存批量补发"）。
//
// 这是离线桌面程序，没有服务器可以上报，所以这里做的是"本地留证"：
// 把 WebGL 上下文丢失、着色器编译失败这类事件记下来，用户可以在
// 「设置 → 图形与 GPU」里看到并复制给开发者。隐私要求（REQ-108）：
// 只记录显卡/驱动/后端这类渲染相关的元信息，绝不记录文件路径或内容。

export interface CrashEntry {
  /** 毫秒时间戳。 */
  ts: number;
  /** context-lost | shader-fail | init-fail | slow-frame */
  kind: string;
  /** 当时用的后端：webgl2 / webgl1 / canvas2d */
  backend: string;
  /** 显卡渲染器字符串（不含任何用户数据）。 */
  renderer: string;
  /** 一句人话描述。 */
  detail: string;
}

const STORAGE_KEY = "ddo.render.crashes.v1";
const MAX_ENTRIES = 20;

function load(): CrashEntry[] {
  if (typeof localStorage === "undefined") return [];
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const arr = JSON.parse(raw) as CrashEntry[];
    return Array.isArray(arr) ? arr.slice(-MAX_ENTRIES) : [];
  } catch {
    return [];
  }
}

/** 当前的崩溃记录（响应式数组，设置页直接渲染）。 */
export const crashLog: CrashEntry[] = load();

function persist() {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(crashLog.slice(-MAX_ENTRIES)));
  } catch {
    /* 配额满就算了：内存里还有一份 */
  }
}

/** 记一条事件。相同的 detail 在 30 秒内只记一次，避免掉帧时刷屏。 */
export function recordCrash(entry: Omit<CrashEntry, "ts">) {
  const now = Date.now();
  const dup = crashLog.find((e) => e.kind === entry.kind && e.detail === entry.detail && now - e.ts < 30000);
  if (dup) {
    dup.ts = now;
    persist();
    return;
  }
  crashLog.push({ ...entry, ts: now });
  while (crashLog.length > MAX_ENTRIES) crashLog.shift();
  persist();
}

/** 清空记录。 */
export function clearCrashLog() {
  crashLog.splice(0, crashLog.length);
  persist();
}

/**
 * 监听某块画布的 WebGL 上下文丢失（REQ-108 的 webglcontextlost 捕获）。
 * 返回解绑函数；引擎销毁时调用。
 */
export function watchContextLoss(
  canvas: HTMLCanvasElement,
  info: () => { backend: string; renderer: string },
): () => void {
  const onLost = (ev: Event) => {
    // 阻止默认行为：浏览器默认认为上下文不可恢复，这里我们要保留恢复机会。
    ev.preventDefault();
    const meta = info();
    recordCrash({
      kind: "context-lost",
      backend: meta.backend,
      renderer: meta.renderer,
      detail: "WebGL 上下文丢失（驱动重置、休眠唤醒或显存不足）。程序已回退 Canvas 2D。",
    });
  };
  const onRestored = () => {
    const meta = info();
    recordCrash({
      kind: "context-restored",
      backend: meta.backend,
      renderer: meta.renderer,
      detail: "WebGL 上下文已恢复。",
    });
  };
  canvas.addEventListener("webglcontextlost", onLost, false);
  canvas.addEventListener("webglcontextrestored", onRestored, false);
  return () => {
    canvas.removeEventListener("webglcontextlost", onLost, false);
    canvas.removeEventListener("webglcontextrestored", onRestored, false);
  };
}

/** 崩溃记录导出为纯文本，方便用户复制。 */
export function crashLogText(): string {
  if (!crashLog.length) return "（无记录）";
  return crashLog
    .map((e) => {
      const d = new Date(e.ts);
      const p = (x: number) => String(x).padStart(2, "0");
      const ts = d.getFullYear() + "-" + p(d.getMonth() + 1) + "-" + p(d.getDate()) + " " + p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds());
      return "[" + ts + "] " + e.kind + " (" + e.backend + ") " + e.detail + "\n    渲染器：" + e.renderer;
    })
    .join("\n");
}
