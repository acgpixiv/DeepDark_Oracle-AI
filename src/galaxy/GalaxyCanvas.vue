<script setup lang="ts">
// 星系图谱画布：Canvas 渲染 + 工具栏 + 右键菜单。
// 全屏视图（GalaxyView）与右上角面板的小图共用这个组件。
// 需求：REQ-001~041 的视图部分；数据来自 cmd::graph_data，展开子层由引擎本地折叠。
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import {
  AlignHorizontalJustifyStart,
  ChevronDown,
  ChevronRight,
  Crosshair,
  ExternalLink,
  FolderOpen,
  GitBranch,
  Info,
  LoaderCircle,
  Maximize2,
  Minus,
  Plus,
  RefreshCw,
  Scan,
  Sparkles,
  X,
} from "lucide-vue-next";
import { api, type GraphDataOut, type SemanticGraphOut } from "../api";
import { toast } from "../store";
import { GalaxyEngine } from "../galaxy/engine";
import { renderSettings, RENDER_SETTINGS_EVENT, backendLabel } from "../galaxy/renderSettings";
import {
  BLUEPRINT_FORCE_CANVAS2D,
  LAYOUT_LABEL,
  LEGEND,
  fmtSize,
  toNode,
  type GEdge,
  type GNode,
  type LayoutMode,
} from "../galaxy/model";

const props = defineProps<{
  /** 图谱根目录；空串表示「所有索引根」。 */
  root?: string;
  /** 首屏展开层数。 */
  depth?: number;
  /** 首屏节点上限。 */
  maxNodes?: number;
  /** 每个目录首屏带出的文件数。 */
  fileLimit?: number;
  /** 紧凑模式：面板小图，隐藏大部分工具栏。 */
  compact?: boolean;
}>();

const emit = defineEmits<{
  (e: "select", node: GNode | null): void;
  (e: "open-path", path: string): void;
  (e: "change-root", root: string): void;
  /** 数据源变化：上层据此决定"整理"下拉给哪些布局选项。 */
  (e: "source", src: "tree" | "semantic"): void;
}>();

const wrap = ref<HTMLDivElement | null>(null);
const canvasRef = ref<HTMLCanvasElement | null>(null);
// WebGL 模式下主画布被 GL 独占，文字标签画在这张覆盖层（REQ-103）。
const labelRef = ref<HTMLCanvasElement | null>(null);
// 一块 canvas 一旦绑定过某种上下文（2d / webgl）就不能再换另一种，
// 所以切换渲染后端时必须换一块新的 DOM 画布，用 key 强制 Vue 重建。
const canvasKey = ref(0);
const loading = ref(false);
const error = ref("");
const meta = ref<GraphDataOut | null>(null);
const selected = ref<GNode | null>(null);
const hovered = ref<GNode | null>(null);
const zoom = ref(1);
const fps = ref(0);
const fisheye = ref(false);
/** 当前布局模式（需求「一键整理」）：力导向 / 层级树 / 径向树。 */
const layoutMode = ref<LayoutMode>("force");

/**
 * 数据来源：目录树 or 语义星图（"排序模式"）。
 *
 * 用户的原话是「2D 星图打开来是一坨，要用模型给它们分类分好，排序排好，
 * 用 AI 去读取之前索引的向量」。这说的是：星图的**数据来源本身**要换，
 * 不是给目录树换个排法 —— 按文件夹建边密度低、没有信息量。
 *
 * 所以这里是两条完全独立的数据链：
 *   tree：graph_data → 目录/文件节点，contains 边（结构视图）
 *   semantic：star_semantic → 文件节点，similar 边（内容视图）
 * 两条共享同一个引擎与同一套视觉语言，切换只换数据。
 */
const dataSource = ref<"tree" | "semantic">("semantic");
/** 语义星图的相似度阈值：越低连线越多、图越密。 */
const threshold = ref(0.55);
/** 语义星图的节点上限。 */
const maxSemantic = ref(260);
/** 语义聚类结果（用于图例与"以簇筛选"）。 */
const clusters = ref<SemanticGraphOut["clusters"]>([]);
const semanticMeta = ref<{ available: number; dim: number; truncated: boolean; total: number } | null>(null);
const semanticLoading = ref(false);
/** 引擎节点集合变化时 +1，逼 nodeCount 这类 computed 重算（引擎不是响应式的）。 */
const dataTick = ref(0);
/** 被"只看这一簇"筛掉的簇 id 集合（点图例切换）。 */
const hiddenClusters = ref<Set<number>>(new Set());
const collapsed = ref<Set<string>>(new Set());
const menu = ref<{ open: boolean; x: number; y: number; node: GNode | null }>({
  open: false,
  x: 0,
  y: 0,
  node: null,
});

let engine: GalaxyEngine | null = null;
let ro: ResizeObserver | null = null;
let fpsTimer = 0;
// 重建引擎（切换渲染后端）时用来恢复场景，避免重新请求后端。
let cachedNodes: GNode[] = [];
let cachedEdges: GEdge[] = [];

const title = computed(() => (meta.value && meta.value.name) || "星系图谱");

const summary = computed(() => {
  const m = meta.value;
  if (!m) return "";
  const parts = [m.total_files.toLocaleString() + " 个文件", fmtSize(m.total_bytes)];
  if (m.truncated > 0) parts.push("已折叠 " + m.truncated);
  return parts.join(" · ");
});

/**
 * 画布上的节点数。
 *
 * 不能只读 meta：语义模式下 meta 是 null（它对应目录树的数据结构），
 * 于是 HUD 会一直显示 "0 节点"，看起来像加载失败。统一以引擎里的
 * 实际节点数为准，语义与目录两种数据源都对得上。
 */
const nodeCount = computed(() => {
  void dataTick.value;
  return engine ? engine.allNodes().length : meta.value?.nodes.length ?? 0;
});

// 后端是在 mount 之后才确定的，读 engine 不是响应式的，用一个计数器驱动刷新。
const backendTick = ref(0);

// 右上角显示实际生效的渲染后端（REQ-108 的诊断信息之一）。
const backendName = computed(() => {
  backendTick.value;
  return backendLabel(engine ? engine.renderBackend : "canvas2d");
});

// WebGL 初始化失败、或驱动命中已知问题被降级时，把原因写在画布上（REQ-103 / REQ-105）。
// 不提示的话用户只会看到"帧率突然变低了"，根本不知道发生了什么。
const renderNote = computed(() => {
  backendTick.value;
  const eng = engine;
  if (!eng) return "";
  if (eng.fallbackReason) return eng.fallbackReason;
  if (eng.renderBackend !== "canvas2d") return "";
  // 蓝图风是主动选 canvas2d 的：此时再报"已降级"等于诬告显卡，用户会去折腾驱动。
  if (BLUEPRINT_FORCE_CANVAS2D) return "";
  const support = eng.glSupport;
  if (support.level === "none") return "当前环境不支持 WebGL，已使用 Canvas 2D 渲染。";
  const patch = support.patches.find((p) => p.action === "force-canvas");
  return patch ? patch.detail : "";
});

// ------------------------------------------------------------ 引擎生命周期

function ensureEngine(): GalaxyEngine | null {
  if (engine) return engine;
  const cv = canvasRef.value;
  if (!cv) return null;
  engine = new GalaxyEngine(cv, {
    onSelect(node) {
      selected.value = node;
      emit("select", node);
    },
    onHover(node) {
      hovered.value = node;
    },
    onActivate(node) {
      // 语义模式下所有节点都是文件（没有层级），双击一律打开；
      // 仍保留 folder 分支，因为目录模式共用同一个引擎。
      if (node.type === "folder" && dataSource.value === "tree") toggleFolder(node);
      else openPath(node.path);
    },
    onMenu(node, clientX, clientY) {
      menu.value = { open: true, x: clientX, y: clientY, node };
    },
    onCamera(cam) {
      zoom.value = cam.k;
    },
    onPinchPair(a, b) {
      // REQ-074：把两个节点「捏」到一起 → 建立临时的关联预览
      toast("已建立「" + a.name + "」与「" + b.name + "」的关联预览");
      linkPreview.value = { a: a.name, b: b.name };
    },
  }, {
    // 渲染后端与画质来自全局设置（REQ-103 / REQ-108）。
    quality: renderSettings.quality,
    backend: renderSettings.backend,
    dprOverride: renderSettings.dprOverride,
    labelCanvas: labelRef.value,
    onSlowFrame(frameMs) {
      // 只有主视图才提示，面板小图提示会显得很吵。
      if (props.compact) return;
      toast("单帧耗时 " + frameMs + "ms，可在「设置 → 图形与 GPU」里切到更低的画质", "warn", 5200);
    },
  });
  // 调试挂钩：CDP 里可直接读引擎内部状态（生产构建同样保留，
  // 体积可忽略，且真 GUI 排障时是唯一入口）。
  (window as any).__ddoEngine = engine;
  const r = wrap.value;
  if (r) engine.resize(r.clientWidth, r.clientHeight);
  if (cachedNodes.length) engine.setData(cachedNodes, cachedEdges);
  backendTick.value += 1;
  fpsTimer = window.setInterval(() => {
    if (engine) fps.value = engine.stats.fps;
  }, 600);
  return engine;
}

const linkPreview = ref<{ a: string; b: string } | null>(null);

function destroyEngine() {
  if (fpsTimer) window.clearInterval(fpsTimer);
  fpsTimer = 0;
  engine?.destroy();
  engine = null;
}

function resize() {
  const r = wrap.value;
  if (!r || !engine) return;
  engine.resize(r.clientWidth, r.clientHeight);
}

// ------------------------------------------------------------ 数据

/**
 * 载入语义星图（"排序模式"）。
 *
 * 与 `load()` 的差别不只是数据源：语义图**没有层级**，所以：
 *  - 不需要 depth / fileLimit，换成了 阈值 + 节点上限；
 *  - 不参与折叠展开，双击节点=打开文件而不是展开；
 *  - 图例按"簇"分组而不是按文件类型。
 */
async function loadSemantic() {
  const eng = ensureEngine();
  if (!eng) return;
  semanticLoading.value = true;
  error.value = "";
  try {
    const g = await api.starSemantic(maxSemantic.value, threshold.value);
    // 向量库是空的（还没建过语义索引）：语义模式此时什么都画不出来，
    // 直接停在空白画布上会让人以为"星图坏了"。退回目录视图并说明原因。
    if (!g.nodes.length) {
      semanticMeta.value = { available: g.available, dim: g.dim, truncated: false, total: 0 };
      dataSource.value = "tree";
      await load();
      toast(
        g.available === 0
          ? "还没有可用的语义向量，已切回目录视图。先在设置里建立索引，再回来看语义星图。"
          : "语义向量不足，已切回目录视图。",
        "warn",
        5200
      );
      return;
    }
    clusters.value = g.clusters;
    semanticMeta.value = {
      available: g.available,
      dim: g.dim,
      truncated: g.truncated,
      total: g.total,
    };
    // 语义图节点本来就是扁平的（没有父子关系），直接构造 GNode。
    // 半径用 centrality：越靠近簇心的文件"代表性越强"，画得越大。
    // 名字截断到前 12 个字符：语义图上的 250 个 pixiv 文件名都以
    // "123456789_p0-" 开头，前 18 个字符里没有区分度，反而把卡片撑得很宽
    // （capFor 半宽最多 105），布局被撑大、缩放被压到 40% 以下。
    // 12 个字符 + 省略号，既能认出是哪张图，又把卡片宽度砍掉一半。
    const shortName = (s: string) => (s.length > 13 ? s.slice(0, 12) + "…" : s);
    const nodes: GNode[] = g.nodes.map((n) => ({
      id: n.id,
      name: shortName(n.name || n.path),
      type: "file",
      kind: n.kind,
      path: n.path,
      size: n.size,
      count: 1,
      depth: 1,
      mtime: n.mtime,
      color: n.color,
      r: 12 + n.centrality * 16,
      childCount: 0,
      x: 0,
      y: 0,
      vx: 0,
      vy: 0,
      hidden: false,
      parentId: null,
      clusterId: n.cluster,
    }));
    const byId = new Map(nodes.map((n) => [n.id, n]));
    // 记录每个节点属于哪个簇：图例点选时要用它做过滤。
    clusterIndex = new Map(g.nodes.map((n) => [n.id, n.cluster]));
    fullNameOf = new Map(g.nodes.map((n) => [n.id, n.name || n.path]));
    hiddenClusters.value = new Set();
    const edges: GEdge[] = [];
    for (const e of g.edges) {
      const s = byId.get(e.source);
      const t = byId.get(e.target);
      if (!s || !t) continue;
      edges.push({ source: s, target: t, type: e.edge_type, strength: e.strength });
    }
    cachedNodes = nodes;
    cachedEdges = edges;
    meta.value = null;
    // 把簇信息交给引擎：簇团布局要画簇包络与簇名。
    eng.setClusters(
      g.clusters.map((c) => ({
        id: c.id,
        label: c.label,
        color: c.color,
        size: c.size,
      }))
    );
    eng.setData(nodes, edges);
    dataTick.value += 1;
    // 语义图默认就用力导向（它本来就是按相似度自然聚拢的），
    // 但如果用户手动选了树/径向，保持他的选择。
    if (layoutMode.value !== "force") eng.setLayout(layoutMode.value, false);
    selected.value = null;
  } catch (e) {
    error.value = String(e);
  } finally {
    semanticLoading.value = false;
  }
}

/** 按当前数据源重新取数。 */
async function reload() {
  if (dataSource.value === "semantic") await loadSemantic();
  else await load();
}

async function load(keepView = false) {
  const eng = ensureEngine();
  if (!eng) return;
  loading.value = true;
  error.value = "";
  try {
    const data = await api.graphData(
      props.root || undefined,
      props.depth === undefined ? (props.compact ? 2 : 3) : props.depth,
      props.maxNodes === undefined ? (props.compact ? 400 : 1200) : props.maxNodes,
      props.fileLimit === undefined ? (props.compact ? 6 : 40) : props.fileLimit
    );
    meta.value = data;
    const nodes = data.nodes.map(toNode);
    const byId = new Map(nodes.map((n) => [n.id, n]));
    const edges: GEdge[] = [];
    for (const e of data.edges) {
      const s = byId.get(e.source);
      const t = byId.get(e.target);
      if (!s || !t) continue;
      edges.push({ source: s, target: t, type: e.edge_type, strength: e.strength });
    }
    const childCount = new Map<string, number>();
    for (const e of edges) childCount.set(e.source.id, (childCount.get(e.source.id) || 0) + 1);
    for (const n of nodes) n.childCount = childCount.get(n.id) || 0;
    cachedNodes = nodes;
    cachedEdges = edges;
    eng.setClusters([]);
    eng.setData(nodes, edges);
    dataTick.value += 1;
    // setData 会在非力导向模式下自动按当前布局落位；这里再补一次是
    // 为了让"换了根目录/层数"之后仍然保持用户选好的整理方式。
    if (layoutMode.value !== "force") eng.setLayout(layoutMode.value, false);
    if (!keepView) {
      collapsed.value = new Set();
      selected.value = null;
      linkPreview.value = null;
    }
  } catch (e) {
    error.value = String(e);
  } finally {
    loading.value = false;
  }
}

// ------------------------------------------------------------ 交互

function toggleFolder(node: GNode) {
  const eng = engine;
  if (!eng) return;
  const key = node.id;
  if (collapsed.value.has(key)) {
    collapsed.value.delete(key);
    eng.expand(key);
  } else {
    collapsed.value.add(key);
    eng.collapse(key);
  }
  collapsed.value = new Set(collapsed.value);
  eng.select(node);
  selected.value = node;
  emit("select", node);
}

function openPath(path: string) {
  if (!path) return;
  emit("open-path", path);
  api.openPath(path).catch((e) => toast(String(e), "error"));
}

function reveal(node: GNode) {
  api.revealPath(node.path).catch((e) => toast(String(e), "error"));
}

function focusNode(node: GNode | null) {
  engine?.focusOn(node);
  selected.value = node;
  emit("select", node);
}

function fitView() {
  engine?.fit(false);
}

function zoomIn() {
  engine?.zoomBy(1.25);
}

function zoomOut() {
  engine?.zoomBy(0.8);
}

function setZoom(k: number) {
  engine?.setZoom(k);
}

function toggleFisheye() {
  fisheye.value = !fisheye.value;
  if (engine) engine.fisheye = fisheye.value;
}

/**
 * 一键整理（需求 #7）：在三种布局之间切换。
 *
 * 按钮点一次换一种，顺序 力导向 → 层级树 → 径向树 → 力导向。
 * 为什么不做成下拉框：这三个是"看看换个样子"的低频操作，
 * 多一层弹窗只会多一次点击；循环按钮一步到位，图标即当前状态。
 */
function cycleLayout() {
  // 语义图只有"力导向 / 簇团"两种有意义；目录图才有层级树与径向树。
  const order: LayoutMode[] =
    dataSource.value === "semantic" ? ["force", "cluster"] : ["force", "tree", "radial"];
  const i = order.indexOf(layoutMode.value);
  setLayout(order[(i + 1) % order.length]);
}

function setLayout(mode: LayoutMode) {
  if (layoutMode.value === mode) return;
  layoutMode.value = mode;
  engine?.setLayout(mode, true);
  toast(
    mode === "force"
      ? "已切回力导向布局"
      : mode === "tree"
        ? "已整理为层级树布局"
        : "已整理为径向树布局",
    "info",
    1600
  );
}

function closeMenu() {
  menu.value = { open: false, x: 0, y: 0, node: null };
}

function menuAction(act: string) {
  const node = menu.value.node;
  closeMenu();
  if (!node) return;
  if (act === "open") openPath(node.path);
  else if (act === "reveal") reveal(node);
  else if (act === "focus") focusNode(node);
  else if (act === "unfocus") focusNode(null);
  else if (act === "toggle") toggleFolder(node);
  else if (act === "root") emit("change-root", node.path);
  else if (act === "similar") {
    // 语义图里"看邻居"就是选中它：引擎会把一跳邻居提亮、其余压暗，
    // 这正是用户想看的"这个文件和谁最像"。不该跳去打开文件（那是另一个动作）。
    focusNode(node);
    toast("已高亮「" + node.name + "」的语义邻居", "info", 1800);
  }
}

// ------------------------------------------------------------ 挂载

function onKey(e: KeyboardEvent) {
  if (e.key === "Escape") {
    if (menu.value.open) closeMenu();
    else if (engine && engine.focusState === "FOCUSED") focusNode(null);
  }
}

/** 设置页切换了渲染后端或画质：重建引擎（画质可以不重建，后端必须重建）。 */
function onRenderSettings() {
  const eng = engine;
  if (!eng) return;
  if (eng.qualityKey !== renderSettings.quality) eng.setQuality(renderSettings.quality);
  eng.setDprOverride(renderSettings.dprOverride);
  const needRebuild = eng.setBackendPreference(renderSettings.backend);
  if (needRebuild) {
    // 同一个 canvas 已经绑定过 WebGL 上下文，切回去只能换一块画布，
    // 所以这里销毁引擎但保留数据，由 nextTick 后的 ensureEngine 重新创建。
    destroyEngine();
    canvasKey.value += 1;
    nextTick(() => {
      ensureEngine();
      resize();
    });
  }
}

onMounted(async () => {
  await nextTick();
  ensureEngine();
  resize();
  if (wrap.value) {
    ro = new ResizeObserver(() => resize());
    ro.observe(wrap.value);
  }
  window.addEventListener("keydown", onKey);
  window.addEventListener(RENDER_SETTINGS_EVENT, onRenderSettings);
  emit("source", dataSource.value);
  await reload();
});

onBeforeUnmount(() => {
  ro?.disconnect();
  ro = null;
  window.removeEventListener("keydown", onKey);
  window.removeEventListener(RENDER_SETTINGS_EVENT, onRenderSettings);
  destroyEngine();
});

watch(
  () => [props.root, props.depth, props.maxNodes, props.fileLimit],
  () => reload()
);

/**
 * 只看某一簇 / 恢复显示。
 *
 * 语义图最容易出现的场景是"28 个簇糊在一起"，用户想单独看清一簇时
 * 不该逼他去调阈值（那是全局的，改了整张图都变）。按簇隐藏是局部操作，
 * 点两下就回来，不影响别人的布局。
 */
function toggleCluster(id: number) {
  if (hiddenClusters.value.has(id)) hiddenClusters.value.delete(id);
  else hiddenClusters.value.add(id);
  hiddenClusters.value = new Set(hiddenClusters.value);
  // 引擎按 kind 做过滤，语义节点的 kind 是文件类型，不能直接用；
  // 这里改成"按节点 id 集合"过滤。
  const keep = new Set<string>();
  for (const n of cachedNodes) {
    const clusterOf = clusterIndex.get(n.id);
    if (clusterOf === undefined || !hiddenClusters.value.has(clusterOf)) keep.add(n.id);
  }
  engine?.setVisibleIds(keep);
}

/** 语义节点 → 簇 id（用于按簇过滤）。 */
let clusterIndex = new Map<string, number>();
/** 语义节点 id → 原始完整文件名（点云里显示的是截断名）。 */
let fullNameOf = new Map<string, string>();

const KIND_CN: Record<string, string> = {
  image: "图片", video: "视频", audio: "音频", document: "文档",
  text: "文本", archive: "压缩包", app: "程序", other: "其他",
};

/** 点云悬停时显示完整文件名（节点上的 name 为了排版被截到 12 字）。 */
function hoverFullName(n: GNode): string {
  return fullNameOf.get(n.id) || n.name;
}

/** 悬停时显示"属于哪一簇"，把分类结果直接告诉用户。 */
function hoverCluster(n: GNode): string {
  const cid = clusterIndex.get(n.id);
  const c = clusters.value.find((x) => x.id === cid);
  return c ? "簇：" + c.label : "";
}

/** 切数据源：目录树 ↔ 语义。切回去时不清空缓存，靠 reload 重新取数。 */
async function setSource(src: "tree" | "semantic") {
  if (dataSource.value === src) return;
  dataSource.value = src;
  emit("source", src);
  await reload();
}

/**
 * 阈值/节点数改了要重算——但滑块拖动会连发几十次，
 * 每次都跑一遍后端聚类会让 UI 卡成幻灯片。300ms 防抖足够"跟手"。
 */
let semanticTimer = 0;
function scheduleSemantic() {
  if (semanticTimer) window.clearTimeout(semanticTimer);
  semanticTimer = window.setTimeout(() => {
    semanticTimer = 0;
    void loadSemantic();
  }, 300);
}

defineExpose({
  reload,
  loadSemantic,
  setSource,
  focus: focusNode,
  setLayout,
  cycleLayout,
  layout: () => layoutMode.value,
});
</script>
<template>
  <div class="gx" :class="{ compact }" ref="wrap">
    <canvas ref="canvasRef" :key="'main-' + canvasKey" class="gx-canvas"></canvas>
    <!-- WebGL 模式下的文字标签覆盖层（Canvas 2D 回退时留空，不参与交互） -->
    <canvas ref="labelRef" :key="'label-' + canvasKey" class="gx-label"></canvas>

    <!-- 左上角：标题 + 规模统计（REQ-021） -->
    <div class="gx-head">
      <div class="gx-title">{{ title }}</div>
      <div class="gx-sub">{{ summary }}</div>
    </div>

    <!-- 右上角：视图状态（节点数 / 缩放 / 帧率） -->
    <div v-if="!compact" class="gx-stats">
      <span>{{ nodeCount }} 节点</span>
      <span>{{ Math.round(zoom * 100) }}%</span>
      <span :class="{ warn: fps < 30 }">{{ fps }} FPS</span>
      <span class="gx-backend">{{ backendName }}</span>
    </div>

    <!-- 数据源切换（"排序模式"）：目录树 ↔ 语义聚类 -->
    <div v-if="!compact" class="gx-src">
      <button :class="{ on: dataSource === 'tree' }" title="按文件夹结构排布" @click="setSource('tree')">
        <FolderOpen :size="12" />目录
      </button>
      <button
        :class="{ on: dataSource === 'semantic' }"
        title="用 AI 读取已索引的向量，按内容相似度重排"
        @click="setSource('semantic')"
      >
        <Sparkles :size="12" />语义
      </button>
    </div>

    <!-- 语义参数：阈值与节点数 -->
    <div v-if="!compact && dataSource === 'semantic'" class="gx-semctl">
      <label class="row">
        <span>相似度 ≥ {{ threshold.toFixed(2) }}</span>
        <input
          v-model.number="threshold"
          type="range"
          min="0.35"
          max="0.95"
          step="0.01"
          @change="scheduleSemantic"
        />
      </label>
      <label class="row">
        <span>文件数 {{ maxSemantic }}</span>
        <input
          v-model.number="maxSemantic"
          type="range"
          min="60"
          max="900"
          step="20"
          @change="scheduleSemantic"
        />
      </label>
      <div v-if="semanticMeta" class="semin">
        {{ semanticMeta.total }} 个已排 · 向量库 {{ semanticMeta.available }} 条 ·
        {{ semanticMeta.dim }} 维
        <span v-if="semanticMeta.truncated">（已取样）</span>
      </div>
    </div>

    <!-- 右侧工具条 -->
    <div class="gx-tools">
      <button class="tool" title="放大" @click="zoomIn"><Plus :size="15" /></button>
      <button class="tool" title="缩小" @click="zoomOut"><Minus :size="15" /></button>
      <button class="tool" title="复位视图" @click="fitView"><Maximize2 :size="15" /></button>
      <button
        class="tool"
        :class="{ on: layoutMode !== 'force' }"
        :title="'一键整理：' + LAYOUT_LABEL[layoutMode] + '（点击切换布局）'"
        @click="cycleLayout"
      >
        <GitBranch v-if="layoutMode === 'force'" :size="15" />
        <AlignHorizontalJustifyStart v-else-if="layoutMode === 'tree'" :size="15" />
        <Sparkles v-else :size="15" />
      </button>
      <button class="tool" :class="{ on: fisheye }" title="鱼眼透镜（REQ-077）" @click="toggleFisheye">
        <Scan :size="15" />
      </button>
      <button class="tool" title="重新加载" @click="reload()"><RefreshCw :size="15" /></button>
    </div>

    <!-- 渲染后端降级提示（REQ-103 / REQ-105）：只在不顺的时候出现，正常时不占地方 -->
    <div v-if="renderNote && !compact" class="gx-note" :title="renderNote">
      <Info :size="12" />
      <span>{{ renderNote }}</span>
    </div>

    <!-- 左下角图例：语义模式显示"簇"，目录模式显示文件类型（REQ-008） -->
    <div class="gx-legend">
      <template v-if="dataSource === 'semantic' && clusters.length">
        <span
          v-for="c in clusters.slice(0, 8)"
          :key="c.id"
          class="lg clickable"
          :class="{ off: hiddenClusters.has(c.id) }"
          :title="(hiddenClusters.has(c.id) ? '显示' : '隐藏') + c.label + '（' + c.size + ' 个文件）'"
          @click="toggleCluster(c.id)"
        >
          <i :style="{ background: c.color }"></i>{{ c.label || "簇 " + c.id }}
          <em>{{ c.size }}</em>
        </span>
      </template>
      <template v-else>
        <span v-for="l in LEGEND" :key="l.label" class="lg">
          <i :style="{ background: l.color }"></i>{{ l.label }}
        </span>
      </template>
    </div>

    <!-- 悬停提示：点云模式下节点是圆点、没有常驻标签，
         这里是读文件名的唯一入口，所以路径与类型都要给全。 -->
    <div v-if="hovered && !menu.open" class="gx-tip">
      <template v-if="dataSource === 'semantic'">
        <strong>{{ hoverFullName(hovered) }}</strong>
        <span>{{ hoverCluster(hovered) }}</span>
        <span>{{ fmtSize(hovered.size) }} · {{ KIND_CN[hovered.kind] || hovered.kind || "文件" }}</span>
      </template>
      <template v-else>
        <strong>{{ hovered.name }}</strong>
        <span v-if="hovered.type === 'folder'">
          {{ hovered.count.toLocaleString() }} 个文件 · {{ fmtSize(hovered.size) }}
        </span>
        <span v-else>{{ fmtSize(hovered.size) }} · {{ hovered.kind || "文件" }}</span>
      </template>
    </div>

    <!-- 捏合建边预览（REQ-074） -->
    <div v-if="linkPreview" class="gx-link-preview">
      <span>关联预览：{{ linkPreview.a }} ↔ {{ linkPreview.b }}</span>
      <button class="mini" @click="linkPreview = null">取消</button>
    </div>

    <!-- 加载 / 错误 -->
    <div v-if="loading || semanticLoading" class="gx-mask">
      <LoaderCircle :size="18" class="spin" />
      <span v-if="semanticLoading">正在用向量语义重排星图…</span>
      <span v-else>正在构建星系图谱…</span>
    </div>
    <div v-else-if="error" class="gx-mask err">
      <X :size="18" />
      <span>{{ error }}</span>
    </div>

    <!-- 右键菜单（REQ-022） -->
    <div
      v-if="menu.open"
      class="gx-menu"
      :style="{ left: menu.x + 'px', top: menu.y + 'px' }"
      @click.stop
    >
      <div class="mhead">{{ menu.node ? menu.node.name : "空白处" }}</div>
      <button v-if="menu.node" @click="menuAction('open')"><ExternalLink :size="13" />打开</button>
      <button v-if="menu.node" @click="menuAction('reveal')"><FolderOpen :size="13" />在文件夹中显示</button>
      <button v-if="menu.node" @click="menuAction('focus')"><Crosshair :size="13" />聚焦此节点</button>
      <button
        v-if="menu.node && menu.node.type === 'folder' && dataSource === 'tree'"
        @click="menuAction('toggle')"
      >
        <ChevronRight :size="13" />展开 / 折叠
      </button>
      <button
        v-if="menu.node && menu.node.type === 'folder' && dataSource === 'tree'"
        @click="menuAction('root')"
      >
        <ChevronDown :size="13" />以此为根
      </button>
      <!-- 语义模式下最有用的一条：以这个文件为种子重新看"相近的一群" -->
      <button v-if="menu.node && dataSource === 'semantic'" @click="menuAction('similar')">
        <Sparkles :size="13" />看这个文件的语义邻居
      </button>
      <button v-if="!menu.node" @click="menuAction('unfocus')"><Maximize2 :size="13" />复位视图</button>
    </div>
    <div v-if="menu.open" class="gx-menu-backdrop" @click="closeMenu" @contextmenu.prevent="closeMenu"></div>
  </div>
</template>

<style scoped>
/* 蓝图风：白纸 + 深蓝墨线，全部走"硬"外观——无渐变、无模糊、无发光。
   canvas 自己已经画了 40px 网格，这里不再叠网格，否则两种网格错位会发脏。 */
.gx {
  position: relative;
  width: 100%;
  height: 100%;
  min-height: 160px;
  overflow: hidden;
  background: #dcebf5;
  border-radius: 12px;
  color: #2c3e50;
}
.gx-canvas {
  position: absolute;
  inset: 0;
  display: block;
  cursor: grab;
  touch-action: none;
}
/* 文字标签覆盖层：不吃鼠标事件，WebGL 模式下才画内容 */
.gx-label {
  position: absolute;
  inset: 0;
  display: block;
  pointer-events: none;
}
.gx-head { position: absolute; left: 12px; top: 10px; pointer-events: none; }
.gx-title {
  font-family: "Courier New", Consolas, monospace;
  font-size: 14px; font-weight: 700; letter-spacing: .04em;
  color: #2c3e50;
  background: rgba(255, 255, 255, .95);
  border: 3px solid #2c3e50;
  border-radius: 12px;
  padding: 3px 10px;
  box-shadow: 8px 8px 0 rgba(44, 62, 80, .18);
}
.gx-sub { font-size: 11px; color: #5d768f; margin-top: 6px; padding-left: 4px; }

/* 面板统一规格：白底、3px 深边框、12px 圆角、无模糊硬投影 */
.gx-stats, .gx-legend, .gx-tip, .gx-menu, .gx-link-preview, .gx-note {
  background: rgba(255, 255, 255, .95);
  border: 3px solid #2c3e50;
  border-radius: 12px;
  box-shadow: 8px 8px 0 rgba(44, 62, 80, .18);
  color: #2c3e50;
}
.gx-stats {
  position: absolute; right: 12px; top: 10px;
  display: flex; gap: 10px;
  font-size: 11px; color: #2c3e50;
  padding: 3px 8px;
  font-variant-numeric: tabular-nums;
}
.gx-stats .warn { color: #b3261e; font-weight: 700; }
.gx-backend { color: #5d768f; }
.gx-tools {
  position: absolute; right: 12px; top: 52px;
  display: flex; flex-direction: column; gap: 6px;
}
/* 数据源分段控件：贴在标题下方，和"面板统一规格"同一套硬边样式 */
.gx-src {
  position: absolute; left: 12px; top: 62px;
  display: inline-flex;
  background: rgba(255,255,255,.95);
  border: 3px solid #2c3e50;
  border-radius: 12px;
  box-shadow: 8px 8px 0 rgba(44,62,80,.18);
  overflow: hidden;
}
.gx-src button {
  display: inline-flex; align-items: center; gap: 5px;
  padding: 3px 10px; font-size: 11px; cursor: pointer;
  background: transparent; color: #5d768f; border: 0;
  font-family: "Courier New", Consolas, monospace;
  transition: background-color .18s ease, color .18s ease;
}
.gx-src button:hover { background: #fffbe6; }
.gx-src button.on { background: #2c3e50; color: #ffffff; font-weight: 700; }
/* 语义参数面板：阈值 + 节点数两个滑块 */
.gx-semctl {
  position: absolute; left: 12px; top: 100px;
  width: 232px;
  display: flex; flex-direction: column; gap: 6px;
  background: rgba(255,255,255,.95);
  border: 3px solid #2c3e50;
  border-radius: 12px;
  box-shadow: 8px 8px 0 rgba(44,62,80,.18);
  padding: 7px 9px;
  font-size: 11px; color: #2c3e50;
}
.gx-semctl .row { display: flex; flex-direction: column; gap: 3px; }
.gx-semctl .row > span { font-weight: 700; letter-spacing: .02em; }
.gx-semctl input[type="range"] {
  width: 100%; height: 4px; cursor: pointer;
  accent-color: #2c3e50;
}
.gx-semctl .semin { font-size: 10px; color: #5d768f; line-height: 1.45; }
.gx-legend .lg em {
  font-style: normal; color: #5d768f; font-size: 10px; margin-left: 2px;
}
.gx-legend .lg.clickable {
  cursor: pointer;
  padding: 1px 5px;
  border-radius: 6px;
  transition: background-color .18s ease, opacity .18s ease;
}
.gx-legend .lg.clickable:hover { background: #fffbe6; }
.gx-legend .lg.off { opacity: .34; }
.gx-legend .lg.off i { filter: grayscale(1); }
.tool {
  width: 28px; height: 28px;
  display: grid; place-items: center;
  background: #ffffff;
  border: 3px solid #2c3e50;
  border-radius: 12px;
  color: #2c3e50;
  box-shadow: 4px 4px 0 rgba(44, 62, 80, .18);
  cursor: pointer;
  transition: background-color .18s ease, box-shadow .18s ease, transform .12s ease;
}
.tool:hover { background: #fffbe6; }
.tool:active { transform: translate(2px, 2px); box-shadow: 2px 2px 0 rgba(44, 62, 80, .18); }
.tool.on { background: #fffbe6; border-color: #ffeb3b; }
.gx-legend {
  position: absolute; left: 12px; bottom: 10px;
  display: flex; flex-wrap: wrap; gap: 10px;
  font-size: 11px; color: #2c3e50;
  padding: 4px 8px;
  max-width: calc(100% - 24px);
}
.lg { display: inline-flex; align-items: center; gap: 4px; }
/* 图例色块用同款类型色条形状，和胶囊左边那条对应得上 */
.lg i { width: 4px; height: 12px; border-radius: 1px; display: inline-block; }
.gx-note {
  position: absolute; left: 12px; bottom: 46px;
  display: flex; align-items: center; gap: 6px;
  max-width: calc(100% - 24px);
  font-size: 11px; color: #8a5b00;
  background: #fffbe6;
  padding: 3px 8px;
  overflow: hidden; white-space: nowrap; text-overflow: ellipsis;
}
.gx-tip {
  position: absolute; left: 50%; bottom: 12px; transform: translateX(-50%);
  display: flex; gap: 8px; align-items: baseline; flex-wrap: wrap;
  max-width: min(760px, 82%);
  font-size: 11px; color: #5d768f;
  padding: 5px 10px;
  pointer-events: none;
  /* 语义模式下文件名可能很长：允许换行，别用省略号把关键信息吃掉。 */
  white-space: normal;
  word-break: break-all;
  line-height: 1.45;
}
.gx-tip strong { font-weight: 700; color: #2c3e50; }
.gx-link-preview {
  position: absolute; left: 50%; top: 46px; transform: translateX(-50%);
  display: flex; align-items: center; gap: 10px;
  font-size: 11px; color: #2c3e50;
  background: #fffbe6;
  border-style: dashed;
  padding: 4px 10px;
}
.mini {
  background: #ffffff; border: 2px solid #2c3e50; color: #2c3e50;
  border-radius: 8px; padding: 1px 6px; font-size: 11px; cursor: pointer;
}
.mini:hover { background: #fffbe6; }
.gx-mask {
  position: absolute; inset: 0;
  display: flex; align-items: center; justify-content: center; gap: 8px;
  font-size: 12px; color: #5d768f;
  background: rgba(220, 235, 245, .78);
}
.gx-mask.err { color: #b3261e; }
.spin { animation: spin 1s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
.gx-menu {
  position: fixed; z-index: 300;
  min-width: 180px;
  padding: 4px;
}
.mhead {
  font-size: 11px; color: #5d768f;
  padding: 4px 8px 5px;
  border-bottom: 2px solid #cce0f0;
  margin-bottom: 3px;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.gx-menu button {
  width: 100%;
  display: flex; align-items: center; gap: 7px;
  background: transparent; border: 0;
  color: #2c3e50; font-size: 12px;
  padding: 5px 8px; border-radius: 8px;
  cursor: pointer; text-align: left;
}
.gx-menu button:hover { background: #fffbe6; }
.gx-menu-backdrop { position: fixed; inset: 0; z-index: 299; }

.compact .gx-head { left: 8px; top: 7px; }
.compact .gx-title { font-size: 11px; border-width: 2px; padding: 2px 7px; box-shadow: 4px 4px 0 rgba(44, 62, 80, .18); }
.compact .gx-sub { font-size: 10px; }
.compact .gx-legend { display: none; }
</style>
