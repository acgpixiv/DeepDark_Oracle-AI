<script setup lang="ts">
// 全屏图谱视图（REQ-001~041 的 2D 星系图 + 《2D转3D与星图》的 3D 星图）。
//
// 两种渲染共用一个视图：2D 用 Canvas 力导向（快、稳、适合老显卡），
// 3D 用 InstancedMesh 星图（深空氛围 + 钻取探索）。切换状态存在
// renderSettings.dim 里，下次进来还是你上次选的那个。
import { computed, defineAsyncComponent, onMounted, ref, watch } from "vue";
import {
  ArrowLeft,
  Boxes,
  ChevronRight,
  Database,
  Layers,
  Network,
  Orbit,
  Rotate3d,
  Sparkles,
  Wand2,
} from "lucide-vue-next";
import { api } from "../api";
import { store, toast } from "../store";
import GalaxyCanvas from "../galaxy/GalaxyCanvas.vue";
// three.js 有 1.2 MB，静态引入会让搜索页也背着它。异步组件 + manualChunks
// 之后，这段代码只在真正打开星图时才下载。
const StarCanvas = defineAsyncComponent(() => import("../galaxy/star/StarCanvas.vue"));
import type { GNode } from "../galaxy/model";
import type { StarNodeOut } from "../api";
import { renderSettings, notifyRenderSettingsChanged } from "../galaxy/renderSettings";

const props = defineProps<{
  /** 进入时指定的根目录（从目录树点进来时带路径）。 */
  root?: string;
}>();

const emit = defineEmits<{ (e: "back"): void }>();

const currentRoot = ref(props.root || "");
const canvasRef = ref<InstanceType<typeof GalaxyCanvas> | null>(null);
const starRef = ref<InstanceType<typeof StarCanvas> | null>(null);
const gSelected = ref<GNode | null>(null);
const sSelected = ref<StarNodeOut | null>(null);
const depth = ref(3);
const fileLimit = ref(40);
/** 2D 星图的整理方式（力导向 / 簇团 / 层级树 / 径向树）。 */
type LayoutKey = "force" | "cluster" | "tree" | "radial";
const layout = ref<LayoutKey>("force");
/** 画布当前的数据源：语义图没有层级，只能"簇团"，不能"层级树"。 */
const source = ref<"tree" | "semantic">("semantic");

/**
 * "整理"下拉的选项。
 *
 * 为什么按数据源分：语义星图的节点都是文件、没有父子关系，
 * 选"层级树"会把 200 多个节点全排在圆心叠成一坨（实测过）。
 * 与其让用户选到一个必然出错的选项，不如直接不提供。
 */
const layoutOptions = computed<Array<{ v: LayoutKey; label: string }>>(() =>
  source.value === "semantic"
    ? [{ v: "cluster", label: "簇团" }]
    : [
        { v: "tree", label: "层级树" },
        { v: "radial", label: "径向树" },
      ]
);

/** 数据源切换后，旧布局可能不适用（语义图不能留在"层级树"），回到力导向。 */
watch(source, (s) => {
  if (s === "semantic" && (layout.value === "tree" || layout.value === "radial")) {
    layout.value = "force";
    canvasRef.value?.setLayout("force");
  }
});

function onLayoutChange(e: Event) {
  const v = (e.target as HTMLSelectElement).value as LayoutKey;
  layout.value = v;
  canvasRef.value?.setLayout(v);
}

/** 2D 画布重建（换了根目录等）后，把当前整理方式补回去。 */
function reapplyLayout() {
  if (layout.value !== "force") canvasRef.value?.setLayout(layout.value);
}
/**
 * 3D 星图里每个目录最多带出多少颗"文件行星"。
 *
 * 需求 #6 的原话是「点开节点会冒出一大堆行星」—— 一个真实目录动辄
 * 上千个文件，全画出来既看不清也让轨道挤成一团。这里按 2D 的选择再
 * 压一档：不显示 = 0，10/40/120 分别收敛到 10/16/24，上限 24。
 * 想看某个目录的全部文件，用「以此为根」或切到列表视图。
 */
const starFileLimit = computed(() => (fileLimit.value === 0 ? 0 : Math.min(24, Math.max(8, Math.round(fileLimit.value / 5)))));

/** 节点类型 → 中文（3D 星图里是"磁盘/分类/文件夹"，2D 是"分类/文件夹"）。 */
const KIND_LABEL: Record<string, string> = {
  root: "全部索引",
  drive: "磁盘",
  kind: "分类",
  folder: "文件夹",
  file: "文件",
};

const dim = computed(() => renderSettings.dim);
const selected = computed<(GNode | StarNodeOut) | null>(() =>
  dim.value === "3d" ? sSelected.value : gSelected.value,
);

/** 两侧节点的字段口径不同，统一给模板用的视图模型。 */
const sel = computed(() => {
  const n: any = selected.value;
  if (!n) return null;
  const isFile = n.type === "file" || n.node_type === "file";
  return {
    name: n.name as string,
    path: (n.path || "") as string,
    color: (n.color || "#4E79A7") as string,
    isFile,
    kind: (n.kind || "") as string,
    count: Number(n.count || 0),
    size: Number(n.size || 0),
    typeLabel: isFile
      ? KIND_LABEL[n.kind] || n.kind || "文件"
      : KIND_LABEL[n.type || n.node_type] || "文件夹",
  };
});

function setDim(v: "2d" | "3d") {
  if (renderSettings.dim === v) return;
  renderSettings.dim = v;
  gSelected.value = null;
  sSelected.value = null;
  notifyRenderSettingsChanged();
}

function applyRoot(path: string) {
  currentRoot.value = path;
  gSelected.value = null;
  sSelected.value = null;
}

function onSelect(node: GNode | null) {
  gSelected.value = node;
}
function onStarSelect(node: StarNodeOut | null) {
  sSelected.value = node;
}

async function openNode(path: string) {
  if (!path) return;
  try {
    await api.openPath(path);
  } catch (e) {
    toast(String(e), "error");
  }
}

function parentOf(path: string): string {
  const p = (path || "").replace(/\\+$/, "");
  const i = p.lastIndexOf("\\");
  if (i < 0) return "";
  const head = p.slice(0, i);
  if (!head) return "";
  if (head.length === 2 && head[1] === ":") return head + "\\";
  return head;
}

function focusSelected() {
  if (dim.value === "3d") starRef.value?.focus();
  else if (gSelected.value) canvasRef.value?.focus(gSelected.value);
}

function rootFromSelection() {
  const s = sel.value;
  if (!s) return;
  applyRoot(s.isFile ? parentOf(s.path) : s.path);
}

onMounted(() => {
  void store.stats;
});

// 从 2D 切到 3D 时，把当前的根目录带过去，别让用户重新找一遍。
watch(dim, (v) => {
  if (v === "3d") sSelected.value = null;
  else gSelected.value = null;
});
</script>

<template>
  <div class="gv">
    <header class="gv-bar">
      <button class="back" title="返回搜索" @click="emit('back')">
        <ArrowLeft :size="16" />
      </button>
      <div class="crumb">
        <Network :size="15" />
        <span class="lbl">{{ dim === "3d" ? "数据星图" : "星系图谱" }}</span>
        <ChevronRight :size="13" class="sep" />
        <span v-if="currentRoot" class="path" :title="currentRoot">{{ currentRoot }}</span>
        <span v-else class="path muted">所有索引根</span>
      </div>

      <div class="gv-controls">
        <div class="seg">
          <button :class="{ on: dim === '2d' }" title="二维力导向图谱" @click="setDim('2d')">
            <Network :size="12" />2D
          </button>
          <button :class="{ on: dim === '3d' }" title="三维数据星图" @click="setDim('3d')">
            <Orbit :size="12" />3D
          </button>
        </div>

        <template v-if="dim === '2d'">
          <label class="ctl">
            <Layers :size="13" /><span>层数</span>
            <select v-model.number="depth">
              <option :value="2">2</option>
              <option :value="3">3</option>
              <option :value="4">4</option>
              <option :value="5">5</option>
            </select>
          </label>
          <!-- 一键整理：选项随数据源变（语义图没有层级，只有簇团） -->
          <label class="ctl">
            <Wand2 :size="13" /><span>整理</span>
            <select :value="layout" @change="onLayoutChange">
              <option value="force">力导向</option>
              <option v-for="o in layoutOptions" :key="o.v" :value="o.v">{{ o.label }}</option>
            </select>
          </label>
        </template>
        <label class="ctl">
          <Boxes :size="13" /><span>每目录文件</span>
          <select v-model.number="fileLimit">
            <option :value="0">不显示</option>
            <option :value="10">10</option>
            <option :value="40">40</option>
            <option :value="120">120</option>
          </select>
        </label>
        <button class="btn" @click="applyRoot('')"><Database :size="13" />全部根</button>
        <button v-if="selected" class="btn" @click="focusSelected">
          <Sparkles :size="13" />聚焦选中
        </button>
      </div>
    </header>

    <div class="gv-body" :class="{ 'is-2d': dim === '2d' }">
      <GalaxyCanvas
        v-if="dim === '2d'"
        ref="canvasRef"
        :root="currentRoot"
        :depth="depth"
        :file-limit="fileLimit"
        :max-nodes="1600"
        @select="onSelect"
        @change-root="applyRoot"
        @open-path="openNode"
        @source="(s) => (source = s)"
      />
      <StarCanvas
        v-else
        ref="starRef"
        :root="currentRoot"
        :file-limit="starFileLimit"
        :dir-limit="24"
        @select="onStarSelect"
        @open-path="openNode"
        @drill="() => {}"
      />
    </div>

    <aside v-if="sel" class="gv-inspector" :class="{ 'is-2d': dim === '2d' }">
      <div class="ih">
        <span class="dot" :style="{ background: sel.color }"></span>
        <span class="nm" :title="sel.name">{{ sel.name }}</span>
        <button class="x" @click="dim === '3d' ? (sSelected = null) : (gSelected = null)">×</button>
      </div>
      <dl class="kv">
        <dt>类型</dt>
        <dd>{{ sel.typeLabel }}</dd>
        <dt>路径</dt>
        <dd class="pathv" :title="sel.path">{{ sel.path || "（全库）" }}</dd>
        <template v-if="!sel.isFile">
          <dt>文件数</dt>
          <dd>{{ sel.count.toLocaleString() }}</dd>
        </template>
        <dt>大小</dt>
        <dd>{{ (sel.size / 1024 / 1024).toFixed(1) }} MB</dd>
        <template v-if="dim === '2d' && sel.kind">
          <dt>类型</dt>
          <dd>{{ sel.typeLabel || sel.kind }}</dd>
        </template>
      </dl>
      <div class="ia">
        <button v-if="sel.isFile" class="btn wide" @click="openNode(sel.path)">打开文件</button>
        <button v-else class="btn wide" @click="dim === '3d' ? starRef?.drill(sSelected?.id || '') : rootFromSelection()">
          <Rotate3d :size="13" />钻取这一层
        </button>
        <button v-if="dim === '2d'" class="btn wide" @click="rootFromSelection">以此为根</button>
      </div>
    </aside>
  </div>
</template>

<style scoped>
.gv { display: flex; flex-direction: column; height: 100%; min-height: 0; background: #080b14; position: relative; }
.gv-bar {
  display: flex; align-items: center; gap: 10px;
  height: 46px; padding: 0 12px; flex: 0 0 auto;
  border-bottom: 1px solid #1d2637;
  background: #0d1220;
  color: #dbe6f7;
}
.back {
  width: 28px; height: 28px; display: grid; place-items: center;
  background: transparent; border: 1px solid #26324a; border-radius: 3px;
  color: #c8d6ee; cursor: pointer;
}
.back:hover { background: #16203a; }
.crumb { display: flex; align-items: center; gap: 6px; font-size: 12px; min-width: 0; }
.crumb .lbl { font-weight: 600; }
.crumb .sep { color: #4d5c78; }
.crumb .path { color: #9fb1cf; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 380px; }
.crumb .muted { color: #6c7a94; }
.gv-controls { margin-left: auto; display: flex; align-items: center; gap: 8px; }
.seg { display: inline-flex; border: 1px solid #26324a; border-radius: 3px; overflow: hidden; }
.seg button {
  display: inline-flex; align-items: center; gap: 4px;
  padding: 3px 9px; font-size: 11px; cursor: pointer;
  background: #131a2a; color: #93a5c4; border: 0;
}
.seg button.on { background: #24406e; color: #e8f0ff; }
.ctl { display: inline-flex; align-items: center; gap: 5px; font-size: 11px; color: #93a5c4; }
.ctl select {
  background: #131a2a; color: #dbe6f7; border: 1px solid #26324a;
  border-radius: 2px; font-size: 11px; padding: 2px 4px;
}
.btn {
  display: inline-flex; align-items: center; gap: 5px;
  background: #131a2a; color: #dbe6f7; border: 1px solid #26324a;
  border-radius: 3px; font-size: 11px; padding: 4px 9px; cursor: pointer;
}
.btn:hover { background: #1b2540; }
.btn.wide { width: 100%; justify-content: center; }
.gv-body { flex: 1 1 auto; min-height: 0; position: relative; }
.gv-inspector {
  position: absolute; right: 12px; bottom: 44px;
  width: 260px; max-height: 46%;
  overflow: auto;
  background: rgba(13, 18, 32, .95);
  border: 1px solid #26324a; border-radius: 3px;
  padding: 8px 10px;
  color: #dbe6f7; font-size: 11px;
  z-index: 6;
}
.ih { display: flex; align-items: center; gap: 6px; margin-bottom: 6px; }
.ih .dot { width: 8px; height: 8px; border-radius: 50%; flex: 0 0 auto; }
.ih .nm { font-weight: 600; font-size: 12px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ih .x { margin-left: auto; background: transparent; border: 0; color: #93a5c4; cursor: pointer; font-size: 15px; line-height: 1; }
.kv { display: grid; grid-template-columns: 52px 1fr; gap: 3px 8px; margin: 0; }
.kv dt { color: #7d8dab; }
.kv dd { margin: 0; color: #dbe6f7; overflow: hidden; text-overflow: ellipsis; }
.kv .pathv { font-size: 10px; color: #9fb1cf; white-space: normal; word-break: break-all; }
.ia { display: flex; flex-direction: column; gap: 5px; margin-top: 8px; }

/* 2D 蓝图视图：画布是浅色方格纸，容器若仍留深色底，圆角四角会漏出黑边。
   3D 星图保持原样，所以只在 dim === '2d' 时挂 is-2d。 */
.gv-body.is-2d { background: #dcebf5; }
.gv-inspector.is-2d {
  background: rgba(255, 255, 255, .97);
  border: 3px solid #2c3e50;
  border-radius: 12px;
  box-shadow: 8px 8px 0 rgba(44, 62, 80, .18);
  color: #2c3e50;
}
.gv-inspector.is-2d .ih .nm,
.gv-inspector.is-2d .kv dd { color: #2c3e50; }
.gv-inspector.is-2d .kv dt,
.gv-inspector.is-2d .kv .pathv,
.gv-inspector.is-2d .ih .x { color: #5d768f; }
.gv-inspector.is-2d .ih .x:hover { color: #b3261e; }
.gv-inspector.is-2d .btn {
  background: #ffffff; color: #2c3e50;
  border: 3px solid #2c3e50; border-radius: 12px;
}
.gv-inspector.is-2d .btn:hover { background: #fffbe6; }
</style>
