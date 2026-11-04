// 渲染能力探测与兼容性适配（需求 REQ-103 ~ REQ-106、REQ-108）。
//
// 为什么不用 PixiJS：这是离线桌面程序，装依赖要能离线完成；而且图谱用的是
// "几万个圆 + 几万条线"这种极简单的几何，自绘 WebGL2 点精灵比引入 400 KB 的
// 通用渲染框架更可控（也更容易在驱动异常时精确降级）。

/** 实际在用的渲染后端。 */
export type RenderBackend = "webgl2" | "webgl1" | "canvas2d";

/** 画质预设（REQ-108 的四档）。 */
export type QualityPreset = "low" | "medium" | "high" | "ultra";

export interface WebGLSupport {
  /** webgl2 | webgl1 | none */
  level: "webgl2" | "webgl1" | "none";
  /** 显卡厂商，例如 NVIDIA / AMD / Intel / Apple / unknown */
  vendor: string;
  /** 渲染器字符串，例如 ANGLE (NVIDIA, NVIDIA GeForce RTX 4060 Laptop GPU ...) */
  renderer: string;
  /** UNMASKED_DEVICE_ID_WEBGL（可能为空）。 */
  deviceId: string;
  maxTextureSize: number;
  maxVertexAttribs: number;
  /** 是否支持浮点纹理混合（粒子光晕用得上）。 */
  floatBlend: boolean;
  /** 是否支持 OES_standard_derivatives（抗锯齿边缘）。 */
  derivatives: boolean;
  /** 着色器精度：highp / mediump / lowp。 */
  precision: string;
  /** 命中的已知驱动问题（REQ-105）。 */
  patches: DriverPatch[];
}

/** 一条已知驱动兼容问题。 */
export interface DriverPatch {
  id: string;
  /** 命中的厂商/型号描述。 */
  matched: string;
  /** 给用户看的一句话说明。 */
  detail: string;
  /** 建议的动作。 */
  action: "force-canvas" | "compat-shader" | "no-postprocess" | "limit-dpr";
}

/*****
 * 已知驱动问题库（REQ-105）。
 *
 * 判断条件刻意写成"保守匹配"：宁可少命中（用户手动切画质即可），也不要误判
 * 把正常显卡强制降级成 Canvas 2D —— 降级后帧率会掉得很明显，用户只会觉得
 * "这软件越更新越卡"。
 */
export const KNOWN_DRIVER_BUGS: Array<{
  id: string;
  test: (info: { vendor: string; renderer: string }) => boolean;
  detail: string;
  action: DriverPatch["action"];
}> = [
  {
    id: "intel-hd-old",
    // Intel HD 4000/5000 时代的集成显卡跑 WebGL2 点精灵很慢，
    // 而且驱动会在长时间绘制后丢掉上下文。
    test: ({ vendor, renderer }) =>
      vendor === "intel" && /(HD Graphics (4|5)\d{3}|HD Graphics [2-5]\d{2})/i.test(renderer),
    detail: "检测到老款 Intel 核显：WebGL 绘制大图时容易丢上下文，已建议改用 Canvas 2D。",
    action: "force-canvas",
  },
  {
    id: "amd-radeon-vega-glitch",
    test: ({ vendor, renderer }) => vendor === "amd" && /(Vega|R7|R9)/i.test(renderer),
    detail: "检测到 AMD Vega / R 系列显卡：部分驱动在高负载下会出现着色器编译失败，已启用兼容着色器。",
    action: "compat-shader",
  },
  {
    id: "swiftshader",
    // SwiftShader = 没有真显卡时浏览器的软件渲染器。此时 WebGL 能用但极慢。
    test: ({ renderer }) => /SwiftShader|Software Adapter|Microsoft Basic/i.test(renderer),
    detail: "当前使用软件渲染（没有可用显卡），WebGL 会明显慢于 Canvas 2D。",
    action: "force-canvas",
  },
  {
    id: "nvidia-old-postprocess",
    test: ({ vendor }) => vendor === "nvidia",
    detail: "NVIDIA 显卡：如遇到画面撕裂或闪烁，可在画质里切到「低」。",
    action: "no-postprocess",
  },
];

/** 从 renderer 字符串里认厂商。 */
function guessVendor(raw: string): string {
  const s = raw.toLowerCase();
  if (s.includes("nvidia") || s.includes("geforce") || s.includes("rtx")) return "nvidia";
  if (s.includes("amd") || s.includes("radeon") || s.includes("ati")) return "amd";
  if (s.includes("intel")) return "intel";
  if (s.includes("apple") || s.includes("metal")) return "apple";
  if (s.includes("mali")) return "arm";
  if (s.includes("adreno")) return "qualcomm";
  return "unknown";
}

/**
 * 探测 WebGL 支持（REQ-103 的 detect_webgl_support）。
 *
 * 这一函数有副作用（创建并立即释放一两个 canvas 上下文），所以每次会话只该
 * 调一次，结果缓存起来。浏览器/WebView2 对同时存活的 WebGL 上下文数量有限制
 * （Chromium 约 16 个），探测完必须立刻 `loseContext` 归还。
 */
let cachedSupport: WebGLSupport | null = null;

/**
 * 带缓存的探测：一次会话只真探一次。
 *
 * 图谱页、索引面板小图、设置页都会问"支持什么"，每次都新建上下文既慢又
 * 逼近 Chromium 的上下文上限。只有设置页点「重新检测」时才 force 重探。
 */
export function webglSupport(force = false): WebGLSupport {
  if (!force && cachedSupport) return cachedSupport;
  cachedSupport = detectWebGL();
  return cachedSupport;
}

export function detectWebGL(): WebGLSupport {
  const base: WebGLSupport = {
    level: "none",
    vendor: "unknown",
    renderer: "",
    deviceId: "",
    maxTextureSize: 0,
    maxVertexAttribs: 0,
    floatBlend: false,
    derivatives: false,
    precision: "mediump",
    patches: [],
  };
  if (typeof document === "undefined") return base;

  let gl: WebGL2RenderingContext | WebGLRenderingContext | null = null;
  const probe = document.createElement("canvas");
  try {
    gl = probe.getContext("webgl2", { failIfMajorPerformanceCaveat: false });
    if (gl) base.level = "webgl2";
  } catch {
    gl = null;
  }
  if (!gl) {
    try {
      gl = probe.getContext("webgl");
      if (gl) base.level = "webgl1";
    } catch {
      gl = null;
    }
  }
  if (!gl) return base;

  const read = (name: number): string => {
    try {
      const v = gl!.getParameter(name);
      return typeof v === "string" ? v : "";
    } catch {
      return "";
    }
  };

  const vendorRaw = read(0x1f00); // GL_VENDOR
  const rendererRaw = read(0x1f01); // GL_RENDERER

  // 优先用 UNMASKED_* （需 WEBGL_debug_renderer_info）；拿不到就退回原始串。
  const dbg = gl.getExtension("WEBGL_debug_renderer_info");
  const unmaskedRenderer = dbg ? read(dbg.UNMASKED_RENDERER_WEBGL) : "";
  const unmaskedVendor = dbg ? read(dbg.UNMASKED_VENDOR_WEBGL) : "";
  const deviceId = dbg ? read(dbg.UNMASKED_DEVICE_ID_WEBGL) : "";
  const renderer = (unmaskedRenderer || rendererRaw || vendorRaw).trim();
  base.renderer = renderer;
  base.deviceId = deviceId;
  base.vendor = guessVendor([unmaskedVendor, unmaskedRenderer, vendorRaw, rendererRaw].join(" "));

  base.maxTextureSize = Number(gl.getParameter(0x0d33) || 0); // MAX_TEXTURE_SIZE
  base.maxVertexAttribs = Number(gl.getParameter(0x8869) || 0); // MAX_VERTEX_ATTRIBS
  base.floatBlend = !!gl.getExtension("EXT_float_blend");
  base.derivatives = !!gl.getExtension("OES_standard_derivatives");

  const fmt = gl.getShaderPrecisionFormat
    ? gl.getShaderPrecisionFormat(gl.VERTEX_SHADER, gl.HIGH_FLOAT)
    : null;
  base.precision = fmt && fmt.precision > 0 ? "highp" : "mediump";

  base.patches = KNOWN_DRIVER_BUGS
    .filter((bug) => {
      try {
        return bug.test({ vendor: base.vendor, renderer: base.renderer });
      } catch {
        return false;
      }
    })
    .map((bug) => ({ id: bug.id, matched: base.renderer, detail: bug.detail, action: bug.action }));

  // 探测完立刻释放：Chromium 系对同时存活的 WebGL 上下文有硬上限。
  try {
    const lose = gl.getExtension("WEBGL_lose_context");
    lose?.loseContext();
  } catch {
    /* 忽略：拿不到扩展就让它随 canvas 被 GC */
  }
  return base;
}

/** 画质预设的具体参数（REQ-108）。 */
export interface QualityProfile {
  key: QualityPreset;
  label: string;
  /** 节点光晕/粒子开关。 */
  glow: boolean;
  /** 连线是否用贝塞尔（关掉后画直线，快很多）。 */
  curvedEdges: boolean;
  /** 背景星点。 */
  starfield: boolean;
  /** devicePixelRatio 上限。 */
  maxDpr: number;
  /** Canvas 2D 模式下的节点上限；超过后自动按 LOD 收紧。 */
  maxNodes: number;
}

export const QUALITY_PROFILES: Record<QualityPreset, QualityProfile> = {
  low: {
    key: "low",
    label: "低（省电 / 老机器）",
    glow: false,
    curvedEdges: false,
    starfield: false,
    maxDpr: 1,
    maxNodes: 5000,
  },
  medium: {
    key: "medium",
    label: "中（默认）",
    glow: true,
    curvedEdges: true,
    starfield: true,
    maxDpr: 2,
    maxNodes: 12000,
  },
  high: {
    key: "high",
    label: "高",
    glow: true,
    curvedEdges: true,
    starfield: true,
    maxDpr: 2,
    maxNodes: 40000,
  },
  ultra: {
    key: "ultra",
    label: "极高（高分屏）",
    glow: true,
    curvedEdges: true,
    starfield: true,
    maxDpr: 3,
    maxNodes: 120000,
  },
};

/**
 * 计算实际要用的 devicePixelRatio（REQ-106）。
 *
 * 高分屏上按原生 dpr 渲染意味着 3 倍像素量；对 Canvas 2D 的逐节点路径填充
 * 来说这是纯粹的浪费（圆本来就看不出锯齿）。所以高端 GPU 才给原生 dpr，
 * 低端/Canvas 路径一律压到上限。
 */
export function effectiveDpr(
  profile: QualityProfile,
  backend: RenderBackend,
  deviceRatio = typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1,
  /** 用户在设置里指定的倍数；0 或省略表示跟随自动策略。 */
  override = 0,
): number {
  const ratio = Math.max(1, Math.min(3, deviceRatio));
  const cap = backend === "canvas2d" ? Math.min(2, profile.maxDpr) : profile.maxDpr;
  // 手动档位也要受画质上限约束：低画质下选 3x 会把帧率直接砍半，
  // 那不是用户想要的"更清楚"，而是"更卡"。
  if (override > 0) return Math.max(1, Math.min(cap, Math.min(3, override)));
  return Math.max(1, Math.min(cap, ratio));
}

/** 按探测结果与用户偏好决定最终后端。 */
export function pickBackend(
  support: WebGLSupport,
  preference: "auto" | "webgl" | "canvas" = "auto",
): RenderBackend {
  if (preference === "canvas") return "canvas2d";
  if (support.level === "none") return "canvas2d";
  // 已知会让 WebGL 直接崩/极慢的驱动，即使手动选了 webgl 也尊重一次探测结果。
  if (support.patches.some((p) => p.action === "force-canvas") && preference === "auto") {
    return "canvas2d";
  }
  return support.level === "webgl2" ? "webgl2" : "webgl1";
}