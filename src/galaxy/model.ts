// 星系图谱的数据模型与视觉常量。
// 对应需求 REQ-001~017 / REQ-042~052：节点半径、颜色映射、LOD 阈值都集中在这里，
// 面板小图（IndexTreePanel）与全屏视图（GalaxyView）共用，保证两处观感一致。

/** 后端 graph_data / tree_browse 返回的目录或文件。 */
export interface RawNode {
  id: string;
  node_type: string;
  name: string;
  path: string;
  kind: string;
  size: number;
  count: number;
  depth: number;
  mtime: number;
}

export interface RawEdge {
  source: string;
  target: string;
  edge_type: string;
  strength: number;
}

export interface RawGraph {
  root: string;
  name: string;
  nodes: RawNode[];
  edges: RawEdge[];
  total_files: number;
  total_bytes: number;
  truncated: number;
}

/** 画布上真正参与力导向的节点。 */
export interface GNode {
  id: string;
  name: string;
  type: "folder" | "file";
  kind: string;
  path: string;
  size: number;
  count: number;
  depth: number;
  mtime: number;
  /** 类型强调色：蓝图风里只用于胶囊左侧色条与图例，不再当填充色用。 */
  color: string;
  /** 半径（世界坐标，会随缩放一起变化）。 */
  r: number;
  /** 该节点在完整图里的子节点数量，用于判断能否展开。 */
  childCount: number;
  x: number;
  y: number;
  vx: number;
  vy: number;
  /** 被折叠隐藏时不出现在画布上。 */
  hidden: boolean;
  /** 由「展开」按钮临时拉取的层级（非首屏数据）。 */
  lazy?: boolean;
  /**
   * 父节点 id（可选）。
   *
   * 2D 图谱后端不给 parent，父子关系靠 `path` 推断（见 layouts2d.buildForest）；
   * 星图后端给了显式 parent，走这里。两种数据源共用同一套布局代码。
   */
  parentId?: string | null;
  /** 语义簇 id（仅语义星图有）：簇团布局据此分组。 */
  clusterId?: number;
}

export interface GEdge {
  source: GNode;
  target: GNode;
  type: string;
  strength: number;
}

// ---------------------------------------------------------------- 蓝图风格

/**
 * 蓝图（blueprint）风格：2D 星系图从「深空星云」改成「方格纸 + 胶囊卡片」。
 *
 * 为什么集中成一份常量：画布、GL 兜底路径、图例、面板边框都要用同一套色。
 * 各自写死的结果是改了一处、另一处还留着深空暗色，画面就"漏底"了。
 */
export const BLUEPRINT_CONFIG = {
  /** 页面底色（浅蓝灰）。 */
  bgColor: "#dcebf5",
  /** 网格线。 */
  gridColor: "#cce0f0",
  /** 节点底色：统一白，不再按类型上色。 */
  nodeFill: "#ffffff",
  /** 节点描边 / 主文字色（深蓝黑）。 */
  nodeBorder: "#2c3e50",
  /** 连线。 */
  lineColor: "#5d768f",
  /** 文字。 */
  textColor: "#2c3e50",
  /** 悬停 / 高亮。 */
  activeColor: "#ffeb3b",
  /** 悬停底色：纯黄在浅蓝底上对比不足，用它的柔和版当填充。 */
  activeFill: "#fffbe6",
  /** 网格间距（屏幕像素）。 */
  gridSize: 40,
  /** 节点描边宽度（CSS 像素）。 */
  nodeBorderWidth: 3,
  /** 连线宽度（CSS 像素）。 */
  lineWidth: 2,
  /** 「层级 = 文件」的边用虚线。 */
  dashPattern: [5, 5] as number[],
  /** 胶囊左侧类型色条宽度（CSS 像素）。 */
  accentWidth: 4,
};

/**
 * 强制 2D 星系图走 Canvas 2D。
 *
 * 理由：GL 管线是「点精灵 + 边缘柔光」，画出来必然是深色夜空里的发光圆点；
 * 蓝图要的是白卡片加深描边，属于另一套图形语言 —— 在 GL 里补出圆角描边
 * 等于重写一遍渲染器，改动量与风险都远大于收益。所以蓝图模式下把后端钉死，
 * 原有 GL 代码路径与探测结果都保留（诊断面板照常显示显卡信息），只是不选中。
 */
export const BLUEPRINT_FORCE_CANVAS2D = true;

// ---------------------------------------------------------------- 布局模式

/**
 * 2D 图谱的三种排布方式（需求「一键整理」#7/#9）。
 *
 * - `force`：原来的力导向自由布局，适合看"谁和谁扎堆"，但节点一多就成一坨。
 * - `tree`：蓝图树状布局（盘符 → 文件夹 → 文件按层铺开），一眼看清结构。
 * - `radial`：径向树，盘符居中、每层一圈，兼顾紧凑与层级 —— 节点多时首选。
 *
 * 三者共用同一份数据和同一套视觉语言（白卡片 + 深描边），
 * 所以切换只换坐标与连线形态，不重建画布。
 */
export type LayoutMode = "force" | "cluster" | "tree" | "radial";

export const LAYOUT_LABEL: Record<LayoutMode, string> = {
  force: "力导向",
  cluster: "簇团",
  tree: "层级树",
  radial: "径向树",
};

/** 布局过渡时长（毫秒）。UI 动效一律 <300ms 的规则不适用于"整图重排"， */
/** 但也不需要 1.2 秒那种拖沓感；800ms 配 ease-out 刚好"看得清过程、不催人"。 */
export const LAYOUT_TRANSITION_MS = 800;

// ---------------------------------------------------------------- 视觉常量

/** 节点半径按层级递进：根/类型节点 > 目录 > 文件（胶囊卡片要放得下名字）。 */
export const ROOT_RADIUS = 24;
export const BASE_RADIUS = 15;
export const RADIUS_SCALE = 3;
/** 目录半径上限：再大的目录也不该比根还大，否则层级关系看不出来。 */
const FOLDER_RADIUS_MAX = 10;
/** 文件是所有节点的最小一级。 */
export const FILE_RADIUS = 11;

/**
 * 胶囊卡片最大半宽（屏幕像素）。
 * 超长文件名必须截断 —— 一个名字横穿半个画布，比截断更糟。
 */
export const CAPSULE_MAX_HALF_W = 105;

/** REQ-008：类型配色。蓝图风下不再用于填充，而是胶囊左侧的色条与图例。 */
export const COLOR_FOLDER = "#4E79A7";
export const COLOR_DOC = "#59A14F";
export const COLOR_IMAGE = "#B07AA1";
export const COLOR_CODE = "#F28E2B";
export const COLOR_VIDEO = "#E15759";
export const COLOR_OTHER = "#9C755F";

/** 判定「代码」用的扩展名（命中即用代码色，否则按文档色）。 */
const CODE_EXTS = new Set([
  "rs", "js", "mjs", "cjs", "ts", "tsx", "jsx", "vue", "py", "c", "h", "cc", "cpp", "hpp",
  "cs", "java", "go", "rb", "php", "sh", "ps1", "bat", "cmd", "json", "toml", "yaml", "yml",
  "xml", "html", "css", "scss", "less", "sql", "lua", "kt", "swift", "gradle", "cmake",
  "dockerfile", "makefile", "ini", "conf", "lock",
]);

export function extOf(name: string): string {
  const i = name.lastIndexOf(".");
  return i > 0 ? name.slice(i + 1).toLowerCase() : "";
}

/**
 * 按后端 kind + 扩展名给出「类型强调色」。
 *
 * 注意语义：蓝图风里它不再是节点填充色，调用方只该拿它画左侧色条 / 图例，
 * 直接当 fillStyle 会让整张图重新变回彩色糖果盘。
 */
export function nodeColor(kind: string, name: string, isFolder: boolean): string {
  if (isFolder) return COLOR_FOLDER;
  switch (kind) {
    case "image":
      return COLOR_IMAGE;
    case "video":
      return COLOR_VIDEO;
    case "document":
    case "text":
      return CODE_EXTS.has(extOf(name)) ? COLOR_CODE : COLOR_DOC;
    default:
      return CODE_EXTS.has(extOf(name)) ? COLOR_CODE : COLOR_OTHER;
  }
}

/** 图例（面板与全屏视图共用）。 */
export const LEGEND: { label: string; color: string }[] = [
  { label: "文件夹", color: COLOR_FOLDER },
  { label: "文档", color: COLOR_DOC },
  { label: "图片", color: COLOR_IMAGE },
  { label: "代码", color: COLOR_CODE },
  { label: "视频", color: COLOR_VIDEO },
  { label: "其他", color: COLOR_OTHER },
];

/**
 * REQ-007：半径计算。
 *
 * 蓝图卡片比圆点占地方，半径整体放大到能放下 14px 文字的高度；
 * 层级差异靠 depth 体现（根 > 目录 > 文件），目录之间仍按文件数微调。
 */
export function radiusOf(count: number, isFolder: boolean, depth = 1): number {
  if (!isFolder) return FILE_RADIUS;
  if (depth <= 0) return ROOT_RADIUS;
  return BASE_RADIUS + Math.min(FOLDER_RADIUS_MAX, RADIUS_SCALE * Math.log(count + 1));
}

/** REQ-011：标签在屏幕上达到这个字号（px）才显示 —— 对应三级 LOD 的「>1x」。 */
export const LOD_LABEL_PX = 15;

/** 缩放范围 REQ-020。 */
export const MIN_ZOOM = 0.1;
export const MAX_ZOOM = 5;

/**
 * 力导向参数 REQ-012 / REQ-013。
 *
 * linkDistance 比原先（44）大：节点从圆点变成卡片后横向宽了一倍多，
 * 沿用旧距离会让父子卡片直接压在一起，连线也被遮住看不见。
 */
export const FORCE = {
  alpha: 1.0,
  alphaDecay: 0.0228,
  velocityDecay: 0.4,
  linkDistance: 76,
  linkStrength: 0.08,
  charge: -170,
  collide: 1.55,
  /** 拖拽阻尼 REQ-018。 */
  dragDamping: 0.85,
};

/**
 * 等宽字体下的文字宽度估算（ASCII 约 0.6em，CJK 约 1em）。
 *
 * 为什么估算而不是 measureText：拾取命中区与绘制必须用同一份宽度，
 * 否则会出现「看得见却点不中」；估算值偏大一点正好给命中区留余量。
 */
export function textWidth(name: string, fontPx: number): number {
  let em = 0;
  for (const ch of name) em += ch.charCodeAt(0) >= 0x2e80 ? 1 : 0.6;
  return em * fontPx;
}

/** 胶囊卡片在屏幕上的度量（CSS 像素）。 */
export interface CapsuleMetrics {
  halfW: number;
  halfH: number;
  /** 圆角半径（胶囊取半高）。 */
  radius: number;
  fontPx: number;
  /** 太小的时候只留色块，不画名字。 */
  showLabel: boolean;
  /** 实际绘制的文字（过长会截断）。 */
  text: string;
}

/**
 * 计算节点的胶囊卡片尺寸。
 *
 * 为什么宽度跟着文字走：卡片一旦装不下名字就退化成一堆看不懂的白点；
 * 宽度由文字撑开、高度只跟半径相关，既保留层级差异，又保证名字读得全。
 */
export function capsuleOf(n: GNode, k: number): CapsuleMetrics {
  return capsuleFor(n.name || "", Math.max(1.2, n.r * k));
}

/**
 * 按「屏幕半径 + 名字」算胶囊尺寸。
 *
 * 为什么单独拆出来：绘制时半径会被鱼眼透镜二次放大，拾取时用的是原始半径，
 * 两边必须能各自传入实际值，否则放大区域点不中。
 */
export function capsuleFor(name: string, radiusPx: number): CapsuleMetrics {
  // 字号跟着卡片长，但设下限 9px：低于这个值文字已经无法辨认，不如直接不画。
  const fontPx = Math.min(14, Math.max(9, radiusPx * 0.8));
  if (radiusPx < 7) {
    return { halfW: radiusPx, halfH: radiusPx, radius: radiusPx, fontPx, showLabel: false, text: "" };
  }
  // 左右内边距里已经含了左侧色条的位置，绘制时文字才不会被色条压住。
  const pad = fontPx * 0.7 + BLUEPRINT_CONFIG.accentWidth;
  const capHalfW = Math.max(radiusPx, CAPSULE_MAX_HALF_W);
  let text = name;
  let tw = textWidth(text, fontPx);
  while (text.length > 1 && tw / 2 + pad > capHalfW) {
    text = text.slice(0, -1);
    tw = textWidth(text + "…", fontPx);
  }
  if (text !== name) text += "…";
  // 半高取 max(半径, 文字高的一半)：卡片必须同时装得下名字和层级尺度。
  const halfH = Math.max(radiusPx, fontPx * 0.85);
  return {
    halfW: Math.max(radiusPx, tw / 2 + pad),
    halfH,
    radius: halfH,
    fontPx,
    showLabel: true,
    text,
  };
}

/** 把后端原始数据转成画布节点；id 去重保证增量合并安全。 */
export function toNode(raw: RawNode): GNode {
  const isFolder = raw.node_type === "folder";
  return {
    id: raw.id,
    name: raw.name || raw.path || "根目录",
    type: isFolder ? "folder" : "file",
    kind: raw.kind || "",
    path: raw.path || "",
    size: raw.size || 0,
    count: raw.count || 0,
    depth: raw.depth || 0,
    mtime: raw.mtime || 0,
    color: nodeColor(raw.kind, raw.name, isFolder),
    r: radiusOf(raw.count || 0, isFolder, raw.depth || 0),
    childCount: isFolder ? 1 : 0,
    x: 0,
    y: 0,
    vx: 0,
    vy: 0,
    hidden: false,
  };
}

/** REQ-015：螺旋初始布局，避免所有节点从原点炸开导致首屏乱飞。 */
export function spiralLayout(nodes: GNode[]): void {
  const golden = Math.PI * (3 - Math.sqrt(5));
  nodes.forEach((n, i) => {
    if (n.type === "folder") {
      const a = i * golden;
      const rad = 16 * Math.sqrt(i + 1);
      n.x = Math.cos(a) * rad;
      n.y = Math.sin(a) * rad;
    } else {
      const a = i * golden;
      const rad = 90 + 9 * Math.sqrt(i + 1);
      n.x = Math.cos(a) * rad;
      n.y = Math.sin(a) * rad;
    }
    n.vx = 0;
    n.vy = 0;
  });
}

/** 人类可读大小（与 store.ts 的 humanSize 同规则，独立一份避免图上再引全局状态）。 */
export function fmtSize(n: number): string {
  if (!n || n < 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 || i === 0 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`;
}
