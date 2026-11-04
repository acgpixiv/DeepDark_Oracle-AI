// 渲染设置（REQ-103 / REQ-108 / 星图需求 #24~#32）：画质档位、后端偏好、
// 3D 维度与各种"耗性能但好看"的开关，全局共享并持久化。
//
// 为什么单独放一个模块而不是塞进 store.ts：图谱画布可能同时存在多份
// （全屏视图 + 右上角面板小图），它们必须读到同一份设置；而且这两项属于
// "本机显示偏好"，跟后端 config 无关，存 localStorage 比存 SQLite 更合适
// （换机/重装时不该被同步，也不该受索引库影响）。
import { reactive, watch } from "vue";
import type { QualityPreset, RenderBackend } from "./capability";

export type BackendPreference = "auto" | "webgl" | "canvas";

export interface RenderSettings {
  quality: QualityPreset;
  backend: BackendPreference;
  /** 渲染分辨率倍数：0 表示跟随画质自动（REQ-106）。 */
  dprOverride: number;
  /** 2D/3D 维度（星图需求 #1~#3）。 */
  dim: "2d" | "3d";
  /** 深空场景：背景雾 + 星尘 + 缓慢自转（#8/#9）。 */
  deepSpace: boolean;
  /** Bloom 辉光（#10）。 */
  bloom: boolean;
  /** 铅绘滤镜（#24~#28）。 */
  sketch: boolean;
  /** 自动降级：帧率持续偏低时自动关特效（#31/#33）。 */
  autoDegrade: boolean;
}

const STORAGE_KEY = "ddo.render.settings.v1";

function defaults(): RenderSettings {
  return {
    quality: "medium",
    backend: "auto",
    dprOverride: 0,
    dim: "3d",
    deepSpace: true,
    bloom: true,
    sketch: false,
    autoDegrade: true,
  };
}

function readStorage(): RenderSettings {
  const base = defaults();
  if (typeof localStorage === "undefined") return base;
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return base;
    const obj = JSON.parse(raw) as Partial<RenderSettings>;
    if (obj.quality === "low" || obj.quality === "medium" || obj.quality === "high" || obj.quality === "ultra") {
      base.quality = obj.quality;
    }
    if (obj.backend === "auto" || obj.backend === "webgl" || obj.backend === "canvas") {
      base.backend = obj.backend;
    }
    if (typeof obj.dprOverride === "number" && obj.dprOverride >= 0 && obj.dprOverride <= 3) {
      base.dprOverride = obj.dprOverride;
    }
    if (obj.dim === "2d" || obj.dim === "3d") base.dim = obj.dim;
    if (typeof obj.deepSpace === "boolean") base.deepSpace = obj.deepSpace;
    if (typeof obj.bloom === "boolean") base.bloom = obj.bloom;
    if (typeof obj.sketch === "boolean") base.sketch = obj.sketch;
    if (typeof obj.autoDegrade === "boolean") base.autoDegrade = obj.autoDegrade;
  } catch {
    /* 存储损坏就用默认值，不要因为一个坏 JSON 让整个界面起不来 */
  }
  return base;
}

export const renderSettings = reactive<RenderSettings>(readStorage());

/** 写回存储（watch 自动触发，设置页只需改字段）。 */
watch(
  () => [
    renderSettings.quality,
    renderSettings.backend,
    renderSettings.dprOverride,
    renderSettings.dim,
    renderSettings.deepSpace,
    renderSettings.bloom,
    renderSettings.sketch,
    renderSettings.autoDegrade,
  ],
  () => {
    if (typeof localStorage === "undefined") return;
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(renderSettings));
    } catch {
      /* 隐私模式/配额满：内存里的设置仍然生效 */
    }
  }
);

/** 给界面显示的中文名。 */
export function backendLabel(backend: RenderBackend): string {
  if (backend === "webgl2") return "WebGL 2";
  if (backend === "webgl1") return "WebGL 1";
  return "Canvas 2D";
}

/** 用户偏好对应的中文名。 */
export function preferenceLabel(pref: BackendPreference): string {
  if (pref === "webgl") return "强制 WebGL";
  if (pref === "canvas") return "强制 Canvas 2D";
  return "自动检测";
}

/** 渲染后端变化事件：设置页改完后，已打开的画布需要重建引擎。 */
export const RENDER_SETTINGS_EVENT = "ddo:render-settings";

export function notifyRenderSettingsChanged() {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent(RENDER_SETTINGS_EVENT));
}
