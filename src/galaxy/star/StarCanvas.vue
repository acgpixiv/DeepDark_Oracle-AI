<script setup lang="ts">
// 3D 星图画布（《2D转3D与星图需求明细》#1~#33 的前端主体）。
//
// 分层思路：引擎负责一切「3D 与性能」，这个组件只负责 Vue 侧的编排
// （加载、钻取、面包屑、HUD、菜单）。标签层用独立 DOM 绝对定位，
// 而不是在 three 里做 TextGeometry —— 中文字体转几何体要几 MB，
// 而且一万个标签的更新完全没必要走 GPU。
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import {
  ArrowLeft,
  ChevronRight,
  Crosshair,
  LoaderCircle,
  Maximize2,
  Minus,
  Plus,
  RefreshCw,
  RotateCcw,
  Sparkles,
} from "lucide-vue-next";
import { api, type DriveOut, type StarGraphOut, type StarNodeOut } from "../../api";
import { renderSettings, RENDER_SETTINGS_EVENT } from "../renderSettings";
import { fmtSize } from "../model";
import { STAR_CLASS, StarEngine3D, type SNode } from "./engine3d";

/** kind → 中文名（与后端 star.rs 的 kind_label 保持一致）。 */
const KIND_LABEL: Record<string, string> = {
  drive: "磁盘",
  folder: "文件夹",
  image: "图片",
  video: "视频",
  audio: "音频",
  document: "文档",
  text: "文本",
  archive: "压缩包",
  app: "程序",
  other: "其他",
};

const props = defineProps<{
  /** 起始目录；空串表示从"全部索引"开始。 */
  root?: string;
  /** 每层最多画多少个子目录。 */
  dirLimit?: number;
  /** 每个目录抽样显示多少个文件。 */
  fileLimit?: number;
  /** 紧凑模式：用在右上角小面板里。 */
  compact?: boolean;
}>();

const emit = defineEmits<{
  (e: "select", node: StarNodeOut | null): void;
  (e: "open-path", path: string): void;
  (e: "drill", id: string): void;
}>();

const wrap = ref<HTMLElement | null>(null);
const host = ref<HTMLElement | null>(null);
const labelLayer = ref<HTMLElement | null>(null);

const loading = ref(true);
const error = ref("");
/** 轻量提示条：钻取这类"局部失败"不该整屏盖住星图（需求 #4）。 */
const notice = ref("");
let noticeTimer = 0;
function flash(text: string, ms = 2800) {
  notice.value = text;
  if (noticeTimer) window.clearTimeout(noticeTimer);
  noticeTimer = window.setTimeout(() => {
    notice.value = "";
    noticeTimer = 0;
  }, ms);
}
const graph = ref<StarGraphOut | null>(null);
const selected = ref<SNode | null>(null);
const hovered = ref<SNode | null>(null);
const hoverPos = ref({ x: 0, y: 0 });
const menu = ref<{ node: SNode; x: number; y: number } | null>(null);
const fps = ref(0);
const drawCalls = ref(0);
const camDist = ref(0);
/** 面包屑：从全局概览到当前层级的路径（#23）。 */
const crumbs = ref<Array<{ id: string; name: string }>>([{ id: ":root", name: "全部索引" }]);
/** 历史栈：支持前进/后退（#23）。 */
const history = ref<string[]>([":root"]);
const historyAt = ref(0);
const degraded = ref(false);
const labels = ref<Array<{ id: string; text: string; x: number; y: number; visible: boolean; kind: string }>>([]);

/** 本机盘符物理信息：恒星大小/引力/颜色的数据源。 */
const drives = ref<DriveOut[]>([]);
let engine: StarEngine3D | null = null;
let ro: ResizeObserver | null = null;
let hudTimer = 0;
let labelTimer = 0;
let slowHits = 0;
/** 图例是引擎内部状态，改动后靠这个计数器触发 computed 重算。 */
const legendTick = ref(0);

const title = computed(() => graph.value?.name || "星图");
/**
 * 引擎里实际存在的天体总数。
 *
 * 不能只读 graph.value.nodes：那是**最近一次**钻取返回的那一层，
 * addData 增量并入的旧层根本不在里面，HUD 会永远显示"3 节点"。
 * 也不能直接依赖 engine.allNodes()（普通数组，不是响应式），
 * 所以用 legendTick 当版本号：任何增删/显隐都 +1，逼 computed 重算。
 */
const nodeCount = computed(() => {
  void legendTick.value;
  return engine?.allNodes().length ?? graph.value?.nodes.length ?? 0;
});
const totalText = computed(() => {
  const g = graph.value;
  if (!g) return "";
  return `${g.total_files.toLocaleString()} 个文件 · ${fmtSize(g.total_bytes)}`;
});
const showLabels = computed(() => !props.compact);
/**
 * 图例（#5 分组着色 / #22 标签过滤）：按 kind 聚合节点数与颜色，可点击筛选。
 *
 * 数据源必须是引擎的全量节点，而不是 graph.value.nodes：钻取是增量
 * 合并的，读 graph.value 只会统计到最新那一层，图例会长期停在
 * "图片 1 / 其他 1" 这种第一层的数字上。
 */
const legend = computed(() => {
  void legendTick.value;
  const all = engine?.allNodes() ?? [];
  if (!all.length) return [] as Array<{ kind: string; label: string; color: string; count: number; off: boolean }>;
  const map = new Map<string, { kind: string; color: string; count: number }>();
  for (const n of all) {
    if (n.type === "root") continue;
    const k = n.kind || n.type;
    const cur = map.get(k);
    if (cur) cur.count += 1;
    else map.set(k, { kind: k, color: n.color || "#9C755F", count: 1 });
  }
  const off = new Set(engine?.hiddenKindList() ?? []);
  return [...map.values()]
    .sort((a, b) => b.count - a.count)
    .slice(0, 8)
    .map((x) => ({ ...x, label: KIND_LABEL[x.kind] || x.kind, off: off.has(x.kind) }));
});

/** 恒星图例（按光谱型分组，需求 #1「不同盘符类型视觉区分明显」）。 */
const starLegend = computed(() => {
  const out: Array<{ cls: string; label: string; core: string; corona: string; count: number }> = [];
  const byCls = new Map<string, number>();
  for (const n of engine?.allNodes() ?? []) {
    if (n.type !== "drive") continue;
    const c = n.starClass || "data";
    byCls.set(c, (byCls.get(c) ?? 0) + 1);
  }
  for (const [cls, count] of byCls) {
    const pal = STAR_CLASS[cls] ?? STAR_CLASS.data;
    out.push({ cls, label: pal.label, core: pal.core, corona: pal.corona, count });
  }
  return out;
});

/**
 * 恒星清单：盘符 → 容量 / 引力条。
 *
 * 为什么要在界面上把它列出来：用户的核心抱怨是「不能跟随盘符大小改变
 * 恒星的大小和引力」。光把算法改了不够 —— 他需要一个能对照的地方，
 * 否则只是"看起来好像大了点"，没法确认。这一列把容量、引力百分比
 * 和恒星的真实颜色并排放，一眼就能验证对应关系。
 */
const driveRows = computed(() => {
  void legendTick.value;
  const rows: Array<{
    id: string;
    name: string;
    core: string;
    corona: string;
    capacityText: string;
    gravity: number;
    gravityText: string;
  }> = [];
  for (const n of engine?.allNodes() ?? []) {
    if (n.type !== "drive") continue;
    const pal = STAR_CLASS[n.starClass || "data"] ?? STAR_CLASS.data;
    const cap = n.capacity ?? 0;
    const g = n.gravity ?? 0;
    rows.push({
      id: n.id,
      name: n.name + ":",
      core: pal.core,
      corona: pal.corona,
      capacityText: cap > 0 ? fmtSize(cap) : "容量未知",
      gravity: g,
      gravityText: (g * 100).toFixed(0) + "%",
    });
  }
  // 容量大的排前面，和恒星的视觉大小顺序一致。
  rows.sort((a, b) => b.gravity - a.gravity);
  return rows;
});

/** 本机有、但还没被索引的盘：提示用户"这里还有一盘没进星图"。 */
const unindexedDrives = computed(() => {
  const known = new Set(
    (engine?.allNodes() ?? [])
      .filter((n) => n.type === "drive")
      .map((n) => (n.path || "").replace(/\\+$/, "").toUpperCase())
  );
  return drives.value.filter((d) => !known.has(d.mount.replace(/\\+$/, "").toUpperCase()));
});

// ------------------------------------------------------------ 引擎

function ensureEngine() {
  if (!host.value || engine) return;
  engine = new StarEngine3D(
    host.value,
    {
      onHover: (n) => {
        hovered.value = n;
        if (n) {
          const p = engine?.projectNodes([n.id]).get(n.id);
          if (p) hoverPos.value = { x: p.x, y: p.y };
        }
      },
      onSelect: (n) => {
        selected.value = n;
        emit("select", n ? toOut(n) : null);
      },
      onActivate: (n) => {
        // 文件是叶子，直接交给系统打开；其余一律展开下一层。
        // 这里必须 catch：drill 是 async，不接住的话异常会变成
        // unhandled rejection，表现就是「点一下没反应，控制台一个红叉」。
        if (n.type === "file") {
          emit("open-path", n.path);
          return;
        }
        drill(n.id).catch((e: any) => {
          error.value = String(e?.message ?? e);
        });
      },
      onMenu: (n, x, y) => {
        menu.value = { node: n, x, y };
      },
      onFrame: () => {
        // 每帧刷 HUD 太浪费：250ms 一次已经够"实时"了。
        const now = performance.now();
        if (now - hudTimer > 250) {
          hudTimer = now;
          fps.value = engine?.fps() ?? 0;
          drawCalls.value = engine?.drawCalls() ?? 0;
          camDist.value = Math.round(engine?.cameraDistance() ?? 0);
        }
        if (showLabels.value && now - labelTimer > 60) {
          labelTimer = now;
          updateLabels();
        }
      },
    },
    {
      deepSpace: renderSettings.deepSpace,
      bloom: renderSettings.bloom,
      sketch: renderSettings.sketch,
      dprOverride: renderSettings.dprOverride,
      onSlowFrame: onSlowFrame,
    },
  );
  engine.resize(wrap.value?.clientWidth || 1, wrap.value?.clientHeight || 1);
}

function toOut(n: SNode): StarNodeOut {
  return {
    id: n.id,
    name: n.name,
    node_type: n.type,
    path: n.path,
    kind: n.kind,
    size: n.size,
    count: n.count,
    depth: n.depth,
    mtime: n.mtime,
    parent: n.parent,
    has_children: n.hasChildren,
    cluster: n.cluster,
    color: n.color,
  };
}

/** 帧率持续偏低 → 逐级关特效（需求 #31/#33）。 */
function onSlowFrame(avg: number) {
  if (!renderSettings.autoDegrade || degraded.value) return;
  slowHits += 1;
  if (slowHits < 2) return;
  degraded.value = true;
  // 顺序有讲究：先关最贵的（Bloom → 星尘），铅绘本来默认就关。
  if (renderSettings.bloom) {
    renderSettings.bloom = false;
    engine?.setBloom(false);
  }
  if (renderSettings.deepSpace && avg < 20) {
    renderSettings.deepSpace = false;
    engine?.setDeepSpace(false);
  }
  engine?.setDprOverride(1);
}

// ------------------------------------------------------------ 标签层

function updateLabels() {
  if (!engine || !labelLayer.value) return;
  const all = engine.allNodes();
  // 只给"值得标"的节点画字：根、类型、目录、以及尺寸大的文件。
  const picks = all.filter((n) => n.type !== "file" || n.size > 1_000_000);
  const list = picks.slice(0, props.compact ? 24 : 90);
  const proj = engine.projectNodes(list.map((n) => n.id));
  const out: typeof labels.value = [];
  for (const n of list) {
    const p = proj.get(n.id);
    if (!p || !p.visible) continue;
    out.push({ id: n.id, text: n.name, x: p.x, y: p.y, visible: true, kind: n.kind || n.type });
  }
  labels.value = out;
}

// ------------------------------------------------------------ 数据

/** 拉一次盘符物理信息（容量 / 可用 / 是否移动盘），失败不影响星图本身。 */
async function loadDrives() {
  try {
    drives.value = await api.starDrives();
  } catch (e) {
    // 拿不到容量时恒星会退回"按文件数估算"，这是可接受的降级，
    // 不该让整张星图报错。
    console.warn("[star] star_drives failed", e);
    drives.value = [];
  }
}

async function loadInitial() {
  loading.value = true;
  error.value = "";
  void loadDrives();
  try {
    const root = (props.root || "").trim();
    let g: StarGraphOut;
    if (root) {
      // 指定了根目录：直接从该目录展开，面包屑补上"全部索引 > 根"。
      g = await api.starChildren("dir:" + root, fileLimit(), dirLimit());
      crumbs.value = [
        { id: ":root", name: "全部索引" },
        { id: "dir:" + root, name: g.name || root },
      ];
      history.value = [crumbs.value[0].id, crumbs.value[1].id];
      historyAt.value = 1;
    } else {
      g = await api.starOverview();
      crumbs.value = [{ id: ":root", name: "全部索引" }];
      history.value = [":root"];
      historyAt.value = 0;
    }
    applyGraph(g, { reset: true });
  } catch (e: any) {
    error.value = String(e?.message || e);
  } finally {
    loading.value = false;
  }
}

function fileLimit() {
  return props.fileLimit ?? (props.compact ? 12 : 60);
}
function dirLimit() {
  return props.dirLimit ?? (props.compact ? 24 : 80);
}

function applyGraph(g: StarGraphOut, opts: { reset: boolean }) {
  graph.value = g;
  if (!engine) return;
  if (opts.reset) {
    engine.setData(g.nodes, g.edges);
    engine.fit(0);
  }
  legendTick.value += 1;
  void restoreLayout(g);
}

/** 后端"这一层没有可展开内容"时会回一张空图；不能拿它覆盖当前层。 */
function isEmptyGraph(g: StarGraphOut | null | undefined) {
  return !g || (g.nodes.length === 0 && g.edges.length === 0);
}

/**
 * 布局缓存（#52）在轨道模式下不再适用。
 *
 * 旧实现把力导向收敛后的坐标存到后端，再次进入时回灌。现在坐标由
 * 「父天体 + 轨道相位」每帧解析算出，回灌只会让天体抖一下再被拉回去。
 * 保留这两个空实现是因为调用点分散在加载/钻取/卸载三处，删掉反而要
 * 改更多地方；以后若要支持"自由布局"模式，把旧实现挪回来即可。
 */
async function restoreLayout(_g: StarGraphOut) {
  /* 轨道模式：位置由引擎计算，无需回灌 */
}

async function saveLayout() {
  /* 轨道模式：位置可随时重算，无需持久化 */
}

/** 钻取到某个节点（#20/#21）：加载反馈 → 增量加入 → 相机移动。 */
async function drill(id: string) {
  if (!engine) return;
  const node = engine.nodeById(id);
  if (!node) return;
  if (node.type === "file") {
    emit("open-path", node.path);
    return;
  }
  if (node.expanded) {
    // 已展开过就只是聚焦，不重复拉数据。
    engine.focusOn(node, 700);
    pushHistory(id, node.name);
    return;
  }
  loading.value = true;
  try {
    const g = await api.starChildren(id, fileLimit(), dirLimit());
    const fresh = engine.addData(g.nodes, g.edges, node);
    // 关键修复（需求 #4「点开就报错」）：
    //  - 只有真的新增了子体才算展开成功，否则下次还能重试；
    //  - 空图绝不能覆盖 graph.value，否则标题/统计/图例瞬间清零，
    //    看起来就像"点一下就报错"。
    if (!fresh.length || isEmptyGraph(g)) {
      node.expanded = false;
      flash(`${node.name} 没有可展开的下一层`);
      return;
    }
    node.expanded = true;
    graph.value = g;
    // 增量并入后引擎节点变了，HUD 计数与图例都要重算。
    legendTick.value += 1;
    await nextTick();
    // 新层级淡入、旧层级退到背景（#19 旧节点淡出 / #21 平滑展开）。
    engine.spotlight(id);
    engine.focusOn(node, 1000);
    pushHistory(id, g.name || node.name);
    emit("drill", id);
    void saveLayout();
  } catch (e: any) {
    // 钻取失败是局部问题：清掉展开标记让人能重试，用提示条而不是整屏遮罩。
    node.expanded = false;
    console.error("[star] drill failed", id, e);
    flash(`展开失败：${String(e?.message ?? e)}`);
  } finally {
    loading.value = false;
  }
}

function pushHistory(id: string, name: string) {
  if (history.value[historyAt.value] === id) {
    setCrumbName(id, name);
    return;
  }
  history.value = history.value.slice(0, historyAt.value + 1).concat(id);
  historyAt.value = history.value.length - 1;
  rebuildCrumbs();
}

function rebuildCrumbs() {
  if (!engine) return;
  const path = history.value.slice(0, historyAt.value + 1);
  const out: Array<{ id: string; name: string }> = [];
  for (const id of path) {
    const n = engine.nodeById(id);
    out.push({ id, name: n?.name || (id === ":root" ? "全部索引" : id.replace(/^\w+:/, "")) });
  }
  crumbs.value = out.length ? out : [{ id: ":root", name: "全部索引" }];
}

function setCrumbName(id: string, name: string) {
  const c = crumbs.value.find((x) => x.id === id);
  if (c) c.name = name;
}

/** 返回上一级（#23）：相机退回父节点并聚焦。 */
function goBack() {
  if (historyAt.value <= 0) return;
  historyAt.value -= 1;
  const id = history.value[historyAt.value];
  rebuildCrumbs();
  const n = engine?.nodeById(id);
  // 返回上一级同时解除聚光，让整张图恢复亮度（#23 平滑收起）。
  engine?.spotlight(id === ":root" ? null : id);
  if (n) engine?.focusOn(n, 800);
  else engine?.fit(700);
}

function goForward() {
  if (historyAt.value >= history.value.length - 1) return;
  historyAt.value += 1;
  const id = history.value[historyAt.value];
  rebuildCrumbs();
  const n = engine?.nodeById(id);
  if (n) engine?.focusOn(n, 800);
}

function jumpCrumb(id: string) {
  const at = history.value.indexOf(id);
  if (at >= 0) {
    historyAt.value = at;
    rebuildCrumbs();
  } else {
    pushHistory(id, "");
  }
  const n = engine?.nodeById(id);
  if (n) engine.focusOn(n, 800);
  else engine?.fit(700);
}

// ------------------------------------------------------------ 交互

function closeMenu() {
  menu.value = null;
}

function menuAction(kind: "open" | "focus" | "drill" | "copy") {
  const m = menu.value;
  closeMenu();
  if (!m) return;
  if (kind === "open") emit("open-path", m.node.path);
  else if (kind === "focus") engine?.focusOn(m.node, 800);
  else if (kind === "drill") {
    // 右键菜单是最容易"点了没反应还说没报错"的地方：这里兜住异常。
    drill(m.node.id).catch((e: any) => flash(`展开失败：${String(e?.message ?? e)}`));
  }
  else if (kind === "copy") void navigator.clipboard?.writeText(m.node.path);
}

/** 点击图例切显示/隐藏某类节点（#5 支持动态切换分组配色 / #22 标签过滤）。 */
function toggleLegend(kind: string) {
  engine?.toggleKind(kind);
  legendTick.value += 1;
}

function fitView() {
  engine?.fit(700);
}
function zoomIn() {
  engine?.zoomBy(0.78);
}
function zoomOut() {
  engine?.zoomBy(1.28);
}
function focusSelected() {
  if (selected.value) engine?.focusOn(selected.value, 700);
  else engine?.fit(700);
}

function onKey(e: KeyboardEvent) {
  if (e.key === "Escape") {
    if (menu.value) closeMenu();
    else if (selected.value) engine?.select(null);
  }
  if (e.key === "Backspace" && !(e.target as HTMLElement)?.matches?.("input,textarea")) {
    e.preventDefault();
    goBack();
  }
}

function onSettings() {
  engine?.setDeepSpace(renderSettings.deepSpace);
  engine?.setBloom(renderSettings.bloom);
  engine?.setSketch(renderSettings.sketch);
  engine?.setDprOverride(renderSettings.dprOverride);
  degraded.value = false;
  slowHits = 0;
}

// ------------------------------------------------------------ 生命周期

function resize() {
  if (!wrap.value || !engine) return;
  engine.resize(wrap.value.clientWidth || 1, wrap.value.clientHeight || 1);
}

onMounted(async () => {
  await nextTick();
  ensureEngine();
  resize();
  ro = new ResizeObserver(() => resize());
  if (wrap.value) ro.observe(wrap.value);
  window.addEventListener("keydown", onKey);
  window.addEventListener(RENDER_SETTINGS_EVENT, onSettings);
  await loadInitial();
});

onBeforeUnmount(() => {
  if (noticeTimer) window.clearTimeout(noticeTimer);
  noticeTimer = 0;
  window.removeEventListener("keydown", onKey);
  window.removeEventListener(RENDER_SETTINGS_EVENT, onSettings);
  ro?.disconnect();
  ro = null;
  void saveLayout();
  engine?.dispose();
  engine = null;
});

watch(
  () => [props.root, props.dirLimit, props.fileLimit],
  () => {
    void loadInitial();
  },
);

defineExpose({
  reload: loadInitial,
  focus: focusSelected,
  drill,
  goBack,
  goForward,
});
</script>

<template>
  <div ref="wrap" class="sc" :class="{ compact }">
    <div ref="host" class="sc-host"></div>
    <div ref="labelLayer" class="sc-labels">
      <template v-if="showLabels">
        <div
          v-for="l in labels"
          :key="l.id"
          class="sc-label"
          :class="'k-' + l.kind"
          :style="{ transform: `translate(${l.x}px, ${l.y}px)` }"
        >
          {{ l.text }}
        </div>
      </template>
    </div>

    <div v-if="showLabels" class="sc-hud">
      <div class="sc-title">{{ title }}</div>
      <div class="sc-stat">
        <span>{{ nodeCount }} 节点</span>
        <span>{{ fps }} fps</span>
        <span>{{ drawCalls }} 批次</span>
        <span v-if="camDist">{{ camDist }} 距</span>
      </div>
    </div>

    <div v-if="showLabels" class="sc-tools">
      <button title="放大" @click="zoomIn"><Plus :size="15" /></button>
      <button title="缩小" @click="zoomOut"><Minus :size="15" /></button>
      <button title="看全图" @click="fitView"><Maximize2 :size="15" /></button>
      <button title="聚焦选中" @click="focusSelected">
        <Crosshair :size="15" />
      </button>
      <button title="返回上一级" :disabled="historyAt <= 0" @click="goBack">
        <ArrowLeft :size="15" />
      </button>
      <button title="前进" :disabled="historyAt >= history.length - 1" @click="goForward">
        <ChevronRight :size="15" />
      </button>
      <button title="重新加载" @click="loadInitial"><RefreshCw :size="15" /></button>
    </div>

    <div v-if="showLabels && degraded" class="sc-note">
      <Sparkles :size="13" />
      帧率偏低，已自动关闭部分特效
      <button @click="onSettings">恢复</button>
    </div>

    <!-- 恒星图例：按光谱型分组（系统盘 / 数据盘 / 归档盘 / 移动盘） -->
    <div v-if="showLabels && starLegend.length" class="sc-stars">
      <span v-for="s in starLegend" :key="s.cls" class="st" :title="s.label + ' · ' + s.count + ' 颗'">
        <i :style="{ background: s.core, boxShadow: '0 0 6px ' + s.corona }"></i>{{ s.label }}
      </span>
    </div>

    <!-- 未索引的盘：插了新盘却看不到恒星时，至少告诉用户它在哪 -->
    <div v-if="showLabels && unindexedDrives.length" class="sc-note unindexed">
      <Sparkles :size="13" />
      未建索引：{{ unindexedDrives.map((d) => d.mount + " " + fmtSize(d.total_bytes)).join("、") }}
    </div>

    <div v-if="showLabels && legend.length" class="sc-legend">
      <button
        v-for="l in legend"
        :key="l.kind"
        class="lg"
        :class="{ off: l.off }"
        :title="l.off ? `显示${l.label}` : `隐藏${l.label}`"
        @click="toggleLegend(l.kind)"
      >
        <i :style="{ background: l.color }"></i>
        <span>{{ l.label }}</span>
        <em>{{ l.count }}</em>
      </button>
    </div>

    <div v-if="showLabels" class="sc-crumbs">
      <template v-for="(c, i) in crumbs" :key="c.id + i">
        <button class="crumb" :class="{ last: i === crumbs.length - 1 }" @click="jumpCrumb(c.id)">
          {{ c.name }}
        </button>
        <ChevronRight v-if="i < crumbs.length - 1" :size="12" class="sep" />
      </template>
      <span class="total">{{ totalText }}</span>
    </div>

    <div
      v-if="hovered && !compact"
      class="sc-tip"
      :style="{ transform: `translate(${hoverPos.x + 14}px, ${hoverPos.y + 10}px)` }"
    >
      <b>{{ hovered.name }}</b>
      <span class="kind">{{ hovered.kind || hovered.type }}</span>
      <span v-if="hovered.type !== 'file'">{{ hovered.count }} 个文件</span>
      <span v-else>{{ fmtSize(hovered.size) }}</span>
    </div>

    <!-- 恒星清单：把"容量 → 大小 → 引力"的对应关系摆出来，
         用户不用猜为什么这颗恒星更大 -->
    <div v-if="showLabels && driveRows.length" class="sc-drives">
      <div v-for="d in driveRows" :key="d.id" class="dr" :class="{ sel: selected?.id === d.id }">
        <i :style="{ background: d.core, boxShadow: '0 0 8px ' + d.corona }"></i>
        <span class="nm">{{ d.name }}</span>
        <span class="cap">{{ d.capacityText }}</span>
        <span class="bars" :title="'引力 ' + d.gravityText">
          <b :style="{ width: Math.round(d.gravity * 100) + '%' }"></b>
        </span>
      </div>
    </div>

    <div v-if="loading" class="sc-mask">
      <LoaderCircle class="spin" :size="22" />
      <span>加载星图…</span>
    </div>
    <!-- 整屏遮罩只在"一张图都没有"时用；已经有星图时改用下面的提示条，
         避免钻取一个空目录就把整个画面盖住（需求 #4）。 -->
    <div v-else-if="error && !graph" class="sc-mask err">
      <span>{{ error }}</span>
      <button @click="loadInitial">重试</button>
    </div>

    <div v-if="notice" class="sc-notice">
      <span>{{ notice }}</span>
    </div>

    <div v-if="menu" class="sc-menu-mask" @pointerdown="closeMenu"></div>
    <div
      v-if="menu"
      class="sc-menu"
      :style="{ left: `${menu.x}px`, top: `${menu.y}px` }"
    >
      <button v-if="menu.node.type === 'file'" @click="menuAction('open')">打开文件</button>
      <button v-else @click="menuAction('drill')">展开这一层</button>
      <button @click="menuAction('focus')">聚焦</button>
      <button @click="menuAction('copy')">复制路径</button>
    </div>
  </div>
</template>

<style scoped>
.sc-stars {
  position: absolute;
  left: 14px;
  bottom: 74px;
  display: flex;
  flex-wrap: wrap;
  gap: 4px 10px;
  font-size: 11px;
  color: #9fb1cf;
  pointer-events: none;
}
.sc-stars .st { display: inline-flex; align-items: center; gap: 5px; }
.sc-stars i {
  width: 9px; height: 9px; border-radius: 50%;
  display: inline-block;
}
/* 恒星清单：右侧竖排，和工具条错开 */
.sc-drives {
  position: absolute;
  right: 14px;
  bottom: 74px;
  width: 208px;
  display: flex;
  flex-direction: column;
  gap: 3px;
  padding: 6px 8px;
  background: rgba(8, 12, 26, 0.72);
  border: 1px solid rgba(110, 145, 200, 0.28);
  border-radius: 4px;
  font-size: 11px;
  color: #c2d2ea;
  pointer-events: none;
}
.sc-drives .dr { display: grid; grid-template-columns: 10px 1fr auto 46px; align-items: center; gap: 6px; }
.sc-drives .dr.sel { color: #ffffff; }
.sc-drives i { width: 9px; height: 9px; border-radius: 50%; display: inline-block; }
.sc-drives .nm { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-weight: 600; }
.sc-drives .cap { color: #8ea4c4; font-variant-numeric: tabular-nums; }
.sc-drives .bars {
  height: 4px; border-radius: 2px; overflow: hidden;
  background: rgba(120, 150, 200, 0.22);
}
.sc-drives .bars b { display: block; height: 100%; background: #6ea8ff; border-radius: 2px; }
/* 「未建索引」提示条。
 *
 * 必须同时清掉 top：基础规则 .sc-note 是 top:12px + translateX(-50%) 居中，
 * 只追加 bottom 会让元素**同时有 top 和 bottom** —— 绝对定位下这等于
 * "从 12px 拉到离底 104px"，元素被撑成一块 265×704 的半透明矩形。
 * 用户看到的就是画布正中一块莫名其妙的褐色方块（2026-11-04 报障）。
 * 改成贴在底部：top:auto 覆盖基础值。 */
.sc-note.unindexed {
  top: auto;
  bottom: 104px;
  /* 长文本时不让它撑宽：一行内省略，完整内容看 title。 */
  max-width: min(560px, calc(100% - 32px));
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.sc {
  position: relative;
  width: 100%;
  height: 100%;
  overflow: hidden;
  background: #050510;
}
.sc-host {
  position: absolute;
  inset: 0;
  cursor: grab;
}
.sc-host:active {
  cursor: grabbing;
}
.sc-labels {
  position: absolute;
  inset: 0;
  pointer-events: none;
  overflow: hidden;
}
.sc-label {
  position: absolute;
  left: 0;
  top: 0;
  padding: 1px 5px;
  border-radius: 3px;
  font-size: 11px;
  line-height: 15px;
  white-space: nowrap;
  color: #cfe0ff;
  background: rgba(6, 10, 24, 0.55);
  text-shadow: 0 0 6px rgba(0, 0, 0, 0.9);
  will-change: transform;
}
.sc-label.k-kind,
.sc-label.k-root {
  color: #fff;
  font-weight: 600;
  font-size: 12px;
}
.sc-hud {
  position: absolute;
  left: 14px;
  top: 12px;
  pointer-events: none;
}
.sc-title {
  font-size: 14px;
  font-weight: 600;
  color: #e8f0ff;
  text-shadow: 0 0 10px rgba(0, 0, 0, 0.8);
}
.sc-stat {
  margin-top: 3px;
  display: flex;
  gap: 10px;
  font-size: 11px;
  color: #7f93b5;
}
.sc-tools {
  position: absolute;
  right: 14px;
  top: 12px;
  display: flex;
  gap: 4px;
  padding: 4px;
  border-radius: 6px;
  background: rgba(10, 16, 32, 0.72);
  border: 1px solid rgba(90, 120, 180, 0.3);
  backdrop-filter: blur(6px);
}
.sc-tools button {
  width: 26px;
  height: 26px;
  display: grid;
  place-items: center;
  border: 0;
  border-radius: 4px;
  background: transparent;
  color: #9fb4d8;
  cursor: pointer;
}
.sc-tools button:hover:not(:disabled) {
  background: rgba(90, 130, 200, 0.22);
  color: #e6f1ff;
}
.sc-tools button:disabled {
  opacity: 0.35;
  cursor: default;
}
.sc-note {
  position: absolute;
  left: 50%;
  top: 12px;
  transform: translateX(-50%);
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 4px 10px;
  border-radius: 20px;
  font-size: 11px;
  color: #ffd9a0;
  background: rgba(60, 40, 10, 0.72);
  border: 1px solid rgba(200, 150, 70, 0.35);
}
.sc-note button {
  border: 0;
  background: transparent;
  color: #ffd9a0;
  text-decoration: underline;
  cursor: pointer;
  font-size: 11px;
}
.sc-notice {
  position: absolute;
  left: 50%;
  bottom: 64px;
  transform: translateX(-50%);
  max-width: min(520px, calc(100% - 32px));
  padding: 6px 14px;
  border-radius: 4px;
  font-size: 12px;
  line-height: 1.5;
  color: #ffe2b8;
  background: rgba(46, 30, 8, 0.9);
  border: 1px solid rgba(214, 158, 76, 0.5);
  box-shadow: 0 8px 20px rgba(0, 0, 0, 0.45);
  pointer-events: none;
  z-index: 4;
}
.sc-legend {
  position: absolute;
  left: 14px;
  bottom: 38px;
  right: 14px;
  display: flex;
  flex-wrap: wrap;
  gap: 5px;
}
.sc-legend .lg {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 20px;
  padding: 0 7px;
  border-radius: 10px;
  border: 1px solid rgba(80, 106, 152, 0.35);
  background: rgba(10, 16, 30, 0.66);
  color: #b8c9e6;
  font-size: 11px;
  cursor: pointer;
  backdrop-filter: blur(4px);
}
.sc-legend .lg i {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  box-shadow: 0 0 6px currentColor;
}
.sc-legend .lg em {
  font-style: normal;
  color: #6f83a4;
}
.sc-legend .lg:hover {
  border-color: rgba(120, 160, 220, 0.6);
  color: #e6f1ff;
}
.sc-legend .lg.off {
  opacity: 0.4;
}
.sc-legend .lg.off i {
  box-shadow: none;
}
.sc-crumbs {
  position: absolute;
  left: 14px;
  bottom: 12px;
  display: flex;
  align-items: center;
  gap: 2px;
  max-width: calc(100% - 28px);
  flex-wrap: wrap;
  font-size: 11px;
}
.crumb {
  border: 0;
  background: rgba(10, 16, 32, 0.7);
  color: #8fa8ce;
  padding: 2px 7px;
  border-radius: 3px;
  cursor: pointer;
  font-size: 11px;
}
.crumb:hover {
  color: #dbe9ff;
}
.crumb.last {
  color: #e8f0ff;
  background: rgba(60, 100, 170, 0.38);
}
.sep {
  color: #46608a;
}
.total {
  margin-left: 8px;
  color: #6f83a4;
}
.sc-tip {
  position: absolute;
  left: 0;
  top: 0;
  padding: 4px 8px;
  border-radius: 4px;
  background: rgba(8, 14, 28, 0.92);
  border: 1px solid rgba(90, 120, 180, 0.35);
  font-size: 11px;
  color: #cfdcf5;
  pointer-events: none;
  display: flex;
  gap: 8px;
  max-width: 340px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.sc-tip b {
  color: #eaf2ff;
  font-weight: 600;
}
.sc-tip .kind {
  color: #7f93b5;
}
.sc-mask {
  position: absolute;
  inset: 0;
  display: grid;
  place-content: center;
  justify-items: center;
  gap: 8px;
  font-size: 12px;
  color: #9fb4d8;
  background: rgba(5, 5, 16, 0.55);
}
.sc-mask.err {
  color: #ff9c9c;
}
.sc-mask button {
  padding: 3px 12px;
  border-radius: 4px;
  border: 1px solid rgba(120, 150, 210, 0.4);
  background: rgba(30, 45, 80, 0.8);
  color: #cfe0ff;
  cursor: pointer;
  font-size: 11px;
}
.spin {
  animation: spin 1s linear infinite;
}
@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}
.sc-menu-mask {
  position: absolute;
  inset: 0;
}
.sc-menu {
  position: absolute;
  min-width: 132px;
  padding: 4px;
  border-radius: 6px;
  background: #0d1526;
  border: 1px solid #2a3a58;
  box-shadow: 0 12px 28px rgba(0, 0, 0, 0.55);
  z-index: 5;
}
.sc-menu button {
  display: block;
  width: 100%;
  text-align: left;
  padding: 6px 10px;
  border: 0;
  border-radius: 4px;
  background: transparent;
  color: #cfe0ff;
  font-size: 12px;
  cursor: pointer;
}
.sc-menu button:hover {
  background: rgba(70, 110, 180, 0.3);
}
.sc.compact .sc-hud,
.sc.compact .sc-tools,
.sc.compact .sc-crumbs,
.sc.compact .sc-labels {
  display: none;
}
</style>
