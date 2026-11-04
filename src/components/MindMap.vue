<script setup lang="ts">
// 八种思维模型图（REQ-053~061）。
// 数据来自 cmd::mind_map_data：统一是「分组 + 条目」，这里按模型切换画法：
// definition 嵌套圆 / description 中心辐射 / comparison 韦恩图 /
// classification 树状图 / decomposition 旭日图 / sequence 时间线 /
// causality 桑基流向 / analogy 平行对照。
import { computed, ref } from "vue";
import { ExternalLink, FolderOpen, X } from "lucide-vue-next";
import type { MindGroup, MindItem } from "../api";

const props = defineProps<{
  model: string;
  groups: MindGroup[];
  title: string;
  subtitle: string;
}>();

const emit = defineEmits<{
  (e: "open", path: string): void;
  (e: "reveal", path: string): void;
}>();

const W = 900;
const H = 520;
const hovered = ref("");
const picked = ref<MindItem | null>(null);

interface Circle {
  g: MindGroup;
  x: number;
  y: number;
  r: number;
}

function packCircles(groups: MindGroup[], cx: number, cy: number, maxR: number): Circle[] {
  const total = groups.reduce((a, g) => a + Math.max(1, g.count), 0) || 1;
  const out: Circle[] = [];
  let angle = 0;
  for (const g of groups.slice(0, 12)) {
    const share = Math.max(1, g.count) / total;
    const r = Math.max(22, Math.sqrt(share) * maxR);
    const ringR = out.length === 0 ? 0 : Math.min(maxR * 0.78, r + 52);
    out.push({ g, x: cx + Math.cos(angle) * ringR, y: cy + Math.sin(angle) * ringR, r });
    angle += 0.85 + (1 - share) * 1.8;
  }
  return out;
}

/** 界定图：同心圆，面积正比于组大小。 */
const definitionCircles = computed<Circle[]>(() => packCircles(props.groups, W / 2, H / 2, 185));

/** 描述图：中心 + 向外辐射。 */
const descriptionCircles = computed<Circle[]>(() => {
  const gs = props.groups.slice(0, 10);
  const cx = W / 2;
  const cy = H / 2;
  const step = (Math.PI * 2) / Math.max(1, gs.length);
  return gs.map((g, i) => {
    const a = -Math.PI / 2 + i * step;
    return {
      g,
      x: cx + Math.cos(a) * 178,
      y: cy + Math.sin(a) * 152,
      r: Math.max(20, Math.min(56, 18 + Math.sqrt(g.count) * 5)),
    };
  });
});

/** 比较图：前 3 组画韦恩图。 */
const venn = computed(() => {
  const gs = props.groups.slice(0, 3);
  const cx = W / 2;
  const cy = H / 2 + 6;
  const pos = [
    { x: cx - 118, y: cy - 58 },
    { x: cx + 118, y: cy - 58 },
    { x: cx, y: cy + 104 },
  ];
  return gs.map((g, i) => ({
    g,
    x: pos[i].x,
    y: pos[i].y,
    r: Math.max(105, Math.min(160, 88 + Math.sqrt(g.count) * 7)),
  }));
});

/** 分类图：横向树状图。 */
const dendro = computed(() => {
  const gs = props.groups.slice(0, 14);
  const rowH = Math.min(32, (H - 70) / Math.max(1, gs.length));
  const top = (H - rowH * gs.length) / 2;
  return gs.map((g, i) => ({ g, y: top + rowH * i + rowH / 2, rowH }));
});

function polar(cx: number, cy: number, r: number, a: number) {
  return { x: cx + Math.cos(a) * r, y: cy + Math.sin(a) * r };
}

function ringArc(cx: number, cy: number, r0: number, r1: number, a0: number, a1: number) {
  const large = a1 - a0 > Math.PI ? 1 : 0;
  const p0 = polar(cx, cy, r1, a0);
  const p1 = polar(cx, cy, r1, a1);
  const p2 = polar(cx, cy, r0, a1);
  const p3 = polar(cx, cy, r0, a0);
  const f = (v: number) => v.toFixed(1);
  return [
    "M" + f(p0.x) + "," + f(p0.y),
    "A" + r1 + "," + r1 + " 0 " + large + " 1 " + f(p1.x) + "," + f(p1.y),
    "L" + f(p2.x) + "," + f(p2.y),
    "A" + r0 + "," + r0 + " 0 " + large + " 0 " + f(p3.x) + "," + f(p3.y),
    "Z",
  ].join(" ");
}

/** 拆解图：旭日图（外环=组，内环=组份额）。 */
const sunburst = computed(() => {
  const gs = props.groups.slice(0, 12);
  const total = gs.reduce((a, g) => a + Math.max(1, g.count), 0) || 1;
  const cx = W / 2;
  const cy = H / 2;
  let start = -Math.PI / 2;
  const out: { g: MindGroup; d1: string; d2: string; mid: number; lx: number; ly: number }[] = [];
  for (const g of gs) {
    const span = (Math.max(1, g.count) / total) * Math.PI * 2;
    const a0 = start;
    const a1 = start + span;
    const mid = (a0 + a1) / 2;
    out.push({
      g,
      d1: ringArc(cx, cy, 88, 168, a0, a1),
      d2: ringArc(cx, cy, 176, 210, a0, a1),
      mid,
      lx: cx + Math.cos(mid) * 126,
      ly: cy + Math.sin(mid) * 126,
    });
    start = a1;
  }
  return { cx, cy, out };
});

/** 顺序图：横向时间线。 */
const timeline = computed(() => {
  const gs = props.groups.slice(0, 12);
  const left = 92;
  const right = W - 92;
  const step = gs.length > 1 ? (right - left) / (gs.length - 1) : 0;
  return gs.map((g, i) => ({ g, x: left + step * i, y: H / 2, up: i % 2 === 0 }));
});

/** 因果图：堆叠条形（带宽正比于流量）。 */
const sankey = computed(() => {
  const gs = props.groups.slice(0, 10);
  const total = gs.reduce((a, g) => a + Math.max(1, g.count), 0) || 1;
  const usable = H - 120;
  let y = 60;
  return gs.map((g) => {
    const h = Math.max(14, (Math.max(1, g.count) / total) * usable);
    const node = { g, y, h };
    y += h + 6;
    return node;
  });
});

/** 类比图：左右两栏平行对照。 */
const analogy = computed(() => {
  const gs = props.groups.slice(0, 8);
  const step = (H - 140) / Math.max(1, gs.length);
  return gs.map((g, i) => ({ g, y: 70 + i * step, items: g.items.slice(0, 3) }));
});

function pickItem(item: MindItem) {
  picked.value = item;
}

function labelOf(g: MindGroup) {
  return g.label.length > 13 ? g.label.slice(0, 12) + "…" : g.label;
}

function fmtMB(n: number) {
  if (n >= 1024 * 1024 * 1024) return (n / 1024 / 1024 / 1024).toFixed(1) + " GB";
  if (n >= 1024 * 1024) return (n / 1024 / 1024).toFixed(1) + " MB";
  if (n >= 1024) return (n / 1024).toFixed(0) + " KB";
  return n + " B";
}
</script>
<template>
  <div class="mm">
    <svg :viewBox="'0 0 ' + W + ' ' + H" class="mm-svg" preserveAspectRatio="xMidYMid meet">
      <defs>
        <radialGradient id="mm-core" cx="50%" cy="50%" r="50%">
          <stop offset="0%" stop-color="#25314d" />
          <stop offset="100%" stop-color="#101728" />
        </radialGradient>
      </defs>

      <!-- 界定图：同心圆 -->
      <g v-if="model === 'definition'">
        <circle v-for="c in definitionCircles" :key="c.g.key"
          :cx="c.x" :cy="c.y" :r="c.r"
          :fill="c.g.color" fill-opacity=".17"
          :stroke="c.g.color" stroke-width="1.4"
          @mouseenter="hovered = c.g.key" @mouseleave="hovered = ''"
          @click="picked = c.g.items[0] || null"
          class="hit" />
        <text v-for="c in definitionCircles" :key="c.g.key + 't'"
          :x="c.x" :y="c.y - 4" text-anchor="middle" class="lb">
          {{ labelOf(c.g) }}
        </text>
        <text v-for="c in definitionCircles" :key="c.g.key + 'n'"
          :x="c.x" :y="c.y + 12" text-anchor="middle" class="lbn">
          {{ c.g.count }}
        </text>
      </g>

      <!-- 描述图：中心辐射 -->
      <g v-else-if="model === 'description'">
        <circle :cx="W / 2" :cy="H / 2" :r="62" fill="url(#mm-core)" stroke="#4E79A7" stroke-width="1.4" />
        <text :x="W / 2" :y="H / 2 - 4" text-anchor="middle" class="lb">主体</text>
        <text :x="W / 2" :y="H / 2 + 14" text-anchor="middle" class="lbn">{{ groups.length }} 组</text>
        <line v-for="c in descriptionCircles" :key="'ln' + c.g.key"
          :x1="W / 2" :y1="H / 2" :x2="c.x" :y2="c.y"
          stroke="#4E79A7" stroke-opacity=".35" stroke-width="1" />
        <g v-for="c in descriptionCircles" :key="c.g.key"
          @mouseenter="hovered = c.g.key" @mouseleave="hovered = ''"
          @click="picked = c.g.items[0] || null" class="hit">
          <circle :cx="c.x" :cy="c.y" :r="c.r" :fill="c.g.color" fill-opacity=".22"
            :stroke="c.g.color" stroke-width="1.4" />
          <text :x="c.x" :y="c.y - 2" text-anchor="middle" class="lb">{{ labelOf(c.g) }}</text>
          <text :x="c.x" :y="c.y + 14" text-anchor="middle" class="lbn">{{ c.g.count }}</text>
        </g>
      </g>

      <!-- 比较图：韦恩 -->
      <g v-else-if="model === 'comparison'">
        <circle v-for="c in venn" :key="c.g.key" :cx="c.x" :cy="c.y" :r="c.r"
          :fill="c.g.color" fill-opacity=".18" :stroke="c.g.color" stroke-width="1.6"
          @mouseenter="hovered = c.g.key" @mouseleave="hovered = ''" class="hit" />
        <text v-for="c in venn" :key="c.g.key + 't'" :x="c.x" :y="c.y - c.r + 22"
          text-anchor="middle" class="lb">{{ labelOf(c.g) }} · {{ c.g.count }}</text>
        <text v-if="groups.length < 2" :x="W / 2" :y="H - 24" text-anchor="middle" class="lbn">
          至少需要 2 组才能比较
        </text>
      </g>

      <!-- 分类图：树状 -->
      <g v-else-if="model === 'classification'">
        <text :x="120" :y="40" class="lb">全部文件</text>
        <line x1="160" y1="44" x2="320" y2="44" stroke="#4E79A7" stroke-opacity=".5" />
        <path v-for="d in dendro" :key="d.g.key"
          :d="'M160,44 L210,44 L210,' + d.y + ' L300,' + d.y"
          fill="none" stroke="#4E79A7" stroke-opacity=".42" stroke-width="1" />
        <g v-for="d in dendro" :key="d.g.key + 'n'"
          @mouseenter="hovered = d.g.key" @mouseleave="hovered = ''"
          @click="picked = d.g.items[0] || null" class="hit">
          <rect x="300" :y="d.y - d.rowH / 2 + 2" width="420" :height="Math.max(12, d.rowH - 4)"
            :fill="d.g.color" fill-opacity=".2" :stroke="d.g.color" stroke-width="1" rx="2" />
          <text x="310" :y="d.y + 4" class="lb">{{ labelOf(d.g) }}</text>
          <rect x="730" :y="d.y - d.rowH / 2 + 2"
            :width="Math.max(4, Math.min(110, d.g.count / (dendro[0]?.g.count || 1) * 110))"
            :height="Math.max(12, d.rowH - 4)" :fill="d.g.color" fill-opacity=".75" rx="1" />
          <text x="848" :y="d.y + 4" class="lbn">{{ d.g.count }}</text>
        </g>
      </g>

      <!-- 拆解图：旭日 -->
      <g v-else-if="model === 'decomposition'">
        <circle :cx="sunburst.cx" :cy="sunburst.cy" :r="86" fill="url(#mm-core)" stroke="#4E79A7" stroke-opacity=".5" />
        <text :x="sunburst.cx" :y="sunburst.cy - 6" text-anchor="middle" class="lb">总体</text>
        <text :x="sunburst.cx" :y="sunburst.cy + 14" text-anchor="middle" class="lbn">{{ groups.length }} 组</text>
        <g v-for="a in sunburst.out" :key="a.g.key"
          @mouseenter="hovered = a.g.key" @mouseleave="hovered = ''"
          @click="picked = a.g.items[0] || null" class="hit">
          <path :d="a.d1" :fill="a.g.color" fill-opacity=".72" stroke="#0b1020" stroke-width="1" />
          <path :d="a.d2" :fill="a.g.color" fill-opacity=".3" stroke="#0b1020" stroke-width=".6" />
          <text :x="a.lx" :y="a.ly" text-anchor="middle" class="lbn">{{ a.g.count }}</text>
        </g>
      </g>

      <!-- 顺序图：时间线 -->
      <g v-else-if="model === 'sequence'">
        <line x1="80" :y1="H / 2" :x2="W - 80" :y2="H / 2" stroke="#4E79A7" stroke-opacity=".5" stroke-width="1.5" />
        <g v-for="t in timeline" :key="t.g.key"
          @mouseenter="hovered = t.g.key" @mouseleave="hovered = ''"
          @click="picked = t.g.items[0] || null" class="hit">
          <line :x1="t.x" :y1="H / 2" :x2="t.x" :y2="t.up ? H / 2 - 28 : H / 2 + 28"
            stroke="#4E79A7" stroke-opacity=".55" />
          <circle :cx="t.x" :cy="H / 2" :r="6" :fill="t.g.color" />
          <text :x="t.x" :y="t.up ? H / 2 - 38 : H / 2 + 46" text-anchor="middle" class="lb">
            {{ t.g.key }}
          </text>
          <text :x="t.x" :y="t.up ? H / 2 - 56 : H / 2 + 64" text-anchor="middle" class="lbn">
            {{ t.g.count }} 个 · {{ fmtMB(t.g.bytes) }}
          </text>
        </g>
      </g>

      <!-- 因果图：桑基流量 -->
      <g v-else-if="model === 'causality'">
        <rect v-for="s in sankey" :key="s.g.key" x="120" :y="s.y" width="260" :height="s.h"
          :fill="s.g.color" fill-opacity=".26" :stroke="s.g.color" stroke-width="1" rx="2"
          @mouseenter="hovered = s.g.key" @mouseleave="hovered = ''" class="hit" />
        <text v-for="s in sankey" :key="s.g.key + 't'" x="132" :y="s.y + s.h / 2 + 4" class="lb">
          {{ s.g.label }}
        </text>
        <path v-for="s in sankey" :key="s.g.key + 'p'"
          :d="'M380,' + (s.y + s.h / 2) + ' C 480,' + (s.y + s.h / 2) + ' 520,44 620,44'"
          fill="none" :stroke="s.g.color" stroke-opacity=".38" :stroke-width="Math.max(1, s.h * .3)" />
        <rect x="620" y="24" width="30" :height="H - 60" fill="#4E79A7" fill-opacity=".2" stroke="#4E79A7" />
        <text x="662" :y="H / 2 + 4" class="lb">文件条目</text>
      </g>

      <!-- 类比图：平行对照 -->
      <g v-else-if="model === 'analogy'">
        <g v-for="r in analogy" :key="r.g.key"
          @mouseenter="hovered = r.g.key" @mouseleave="hovered = ''" class="hit">
          <rect x="90" :y="r.y - 16" width="300" height="32" :fill="r.g.color" fill-opacity=".22"
            :stroke="r.g.color" rx="2" />
          <text x="100" :y="r.y + 5" class="lb">{{ r.g.label }}</text>
          <text x="378" :y="r.y + 5" text-anchor="end" class="lbn">{{ r.g.count }}</text>
          <text x="410" :y="r.y + 5" class="lbn">↔</text>
          <g v-for="(it, ii) in r.items" :key="it.path">
            <text :x="440 + ii * 150" :y="r.y + 5" class="item" @click="pickItem(it)">
              {{ it.name.slice(0, 16) }}
            </text>
          </g>
        </g>
      </g>

      <!-- 兜底：没有任何分组 -->
      <text v-if="!groups.length" :x="W / 2" :y="H / 2" text-anchor="middle" class="lb">
        没有可用的数据 —— 先建立索引或选择搜索结果
      </text>
    </svg>

    <!-- 选中条目详情 -->
    <div v-if="picked" class="mm-detail">
      <div class="dh">
        <span class="dn" :title="picked.name">{{ picked.name }}</span>
        <button class="dx" @click="picked = null"><X :size="13" /></button>
      </div>
      <div class="dd">{{ picked.kind || "文件" }} · {{ fmtMB(picked.size) }}</div>
      <div class="dp" :title="picked.path">{{ picked.path }}</div>
      <div class="da">
        <button class="dbtn" @click="emit('open', picked.path)"><ExternalLink :size="12" />打开</button>
        <button class="dbtn" @click="emit('reveal', picked.path)"><FolderOpen :size="12" />定位</button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.mm { position: relative; width: 100%; height: 100%; min-height: 300px; background: #0a0e1a; border-radius: 3px; }
.mm-svg { width: 100%; height: 100%; display: block; }
.hit { cursor: pointer; transition: fill-opacity .12s; }
.hit:hover { fill-opacity: .42; }
.lb { fill: #dbe6f7; font-size: 12px; font-weight: 600; }
.lbn { fill: #8fa3c4; font-size: 11px; font-variant-numeric: tabular-nums; }
.item { fill: #9fb1cf; font-size: 10px; cursor: pointer; }
.item:hover { fill: #ffe9a8; }
.mm-detail {
  position: absolute; right: 10px; bottom: 10px;
  width: 260px; padding: 8px 10px;
  background: rgba(13, 18, 32, .96);
  border: 1px solid #26324a; border-radius: 3px;
  color: #dbe6f7; font-size: 11px;
}
.dh { display: flex; align-items: center; gap: 6px; }
.dn { font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.dx { margin-left: auto; background: transparent; border: 0; color: #93a5c4; cursor: pointer; display: grid; place-items: center; }
.dd { color: #8fa3c4; margin-top: 3px; }
.dp { color: #7d8dab; font-size: 10px; margin-top: 4px; word-break: break-all; max-height: 44px; overflow: hidden; }
.da { display: flex; gap: 6px; margin-top: 7px; }
.dbtn {
  display: inline-flex; align-items: center; gap: 4px;
  background: #131a2a; border: 1px solid #26324a; color: #dbe6f7;
  border-radius: 2px; font-size: 11px; padding: 3px 8px; cursor: pointer;
}
.dbtn:hover { background: #1b2540; }
</style>
