<script setup lang="ts">
// 树状图（Treemap）：squarify 布局 + 悬停高亮 + 点击下钻。
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import type { TreeNode } from "../api";
import { humanSize, kindColor } from "../store";
import { t } from "../i18n";

const props = defineProps<{ node: TreeNode | null }>();
const emit = defineEmits<{
  (e: "drill", node: TreeNode): void;
  (e: "hover", node: TreeNode | null): void;
}>();

const box = ref<HTMLDivElement | null>(null);
const w = ref(800);
const h = ref(460);
const hovered = ref<string | null>(null);

let ro: ResizeObserver | null = null;

onMounted(() => {
  if (!box.value) return;
  ro = new ResizeObserver((es) => {
    const r = es[0].contentRect;
    w.value = Math.max(120, Math.floor(r.width));
    h.value = Math.max(120, Math.floor(r.height));
  });
  ro.observe(box.value);
});

onBeforeUnmount(() => ro?.disconnect());

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** squarify：把一组数值按面积铺满矩形，尽量接近正方形。 */
function squarify(values: number[], rect: Rect): Rect[] {
  const total = values.reduce((a, b) => a + b, 0);
  if (total <= 0) return values.map(() => ({ x: rect.x, y: rect.y, w: 0, h: 0 }));

  const out: Rect[] = [];
  let rx = rect.x;
  let ry = rect.y;
  let rw = rect.w;
  let rh = rect.h;
  let i = 0;
  const n = values.length;

  while (i < n) {
    const remaining = values.slice(i).reduce((a, b) => a + b, 0);
    if (remaining <= 0) break;
    const scale = (rw * rh) / remaining;

    // 贪心：不断往当前行塞，直到"最差长宽比"变差为止
    const side = Math.min(rw, rh);
    let rowSum = 0;
    let best = Infinity;
    let count = 0;
    for (let j = i; j < n; j++) {
      const area = values[j] * scale;
      const nextSum = rowSum + area;
      const rowThick = nextSum / side;
      // 行内最宽的一块
      const maxArea = Math.max(
        area,
        ...values.slice(i, j).map((v) => v * scale)
      );
      const worst = Math.max(
        (rowThick * rowThick) / Math.max(maxArea, 1e-6),
        Math.max(maxArea, 1e-6) / Math.max(rowThick * rowThick, 1e-6)
      );
      if (worst > best && count > 0) break;
      best = worst;
      rowSum = nextSum;
      count++;
    }
    if (count === 0) count = 1;

    const rowValues = values.slice(i, i + count);
    const rowTotal = rowValues.reduce((a, b) => a + b, 0) * scale;

    if (rw >= rh) {
      const colW = rowTotal / rh;
      let cy = ry;
      for (const v of rowValues) {
        const ch = (v * scale) / Math.max(colW, 1e-6);
        out.push({ x: rx, y: cy, w: colW, h: ch });
        cy += ch;
      }
      rx += colW;
      rw -= colW;
    } else {
      const rowH = rowTotal / rw;
      let cx = rx;
      for (const v of rowValues) {
        const cw = (v * scale) / Math.max(rowH, 1e-6);
        out.push({ x: cx, y: ry, w: cw, h: rowH });
        cx += cw;
      }
      ry += rowH;
      rh -= rowH;
    }
    i += count;
  }
  return out;
}

interface Cell {
  node: TreeNode;
  rect: Rect;
  color: string;
}

const cells = computed<Cell[]>(() => {
  const node = props.node;
  if (!node || !node.children?.length) return [];
  const kids = [...node.children]
    .filter((c) => c.size > 0)
    .sort((a, b) => b.size - a.size)
    .slice(0, 120);
  const pad = 2;
  const rects = squarify(
    kids.map((k) => k.size),
    { x: 0, y: 0, w: w.value, h: h.value }
  );
  return kids.map((k, i) => {
    const r = rects[i];
    const top = Object.entries(k.kinds || {}).sort((a, b) => b[1] - a[1])[0];
    return {
      node: k,
      rect: {
        x: r.x + pad,
        y: r.y + pad,
        w: Math.max(0, r.w - pad * 2),
        h: Math.max(0, r.h - pad * 2),
      },
      color: kindColor(top ? top[0] : "other"),
    };
  });
});

const totalSize = computed(() =>
  (props.node?.children || []).reduce((a, c) => a + c.size, 0)
);

function enter(c: Cell) {
  hovered.value = c.node.path;
  emit("hover", c.node);
}
function leave() {
  hovered.value = null;
  emit("hover", null);
}
function click(c: Cell) {
  if (c.node.children?.length) emit("drill", c.node);
}
</script>

<template>
  <div class="tm" ref="box">
    <div v-if="!cells.length" class="empty">
      暂无数据 —— 请先在「扫描」页建立索引
    </div>
    <template v-else>
      <div
        v-for="c in cells"
        :key="c.node.path"
        class="cell"
        :class="{ hot: hovered === c.node.path }"
        :style="{
          left: c.rect.x + 'px',
          top: c.rect.y + 'px',
          width: c.rect.w + 'px',
          height: c.rect.h + 'px',
          background: c.color + '22',
          borderColor: c.color + '88',
        }"
        @mouseenter="enter(c)"
        @mouseleave="leave"
        @click="click(c)"
        :title="`${c.node.name}\n${humanSize(c.node.size)} · ${c.node.count} ${t('个文件')}`"
      >
        <div class="nm" v-if="c.rect.w > 64 && c.rect.h > 26">
          {{ c.node.name }}
        </div>
        <div class="sz" v-if="c.rect.w > 64 && c.rect.h > 44">
          {{ humanSize(c.node.size) }}
        </div>
        <div v-if="c.node.children?.length && c.rect.w > 110 && c.rect.h > 90" class="nested-boxes">
          <div
            v-for="child in c.node.children.slice(0, 8)"
            :key="child.path"
            class="nested-box"
            :title="`${child.name} ${humanSize(child.size)}`"
            :style="{ flex: `${Math.max(1, child.size)} 1 0` }"
            @click.stop="child.children?.length && emit('drill', child)"
          >{{ child.name }}</div>
        </div>
      </div>
    </template>
    <div class="legend">
      <span v-for="(col, k) in {
        image: '#38bdf8', video: '#a78bfa', audio: '#34d399',
        document: '#f59e0b', text: '#94a3b8', archive: '#fb923c', other: '#64748b'
      }" :key="k" class="lg">
        <i :style="{ background: col }"></i>
        {{ { image: t('图片'), video: t('视频'), audio: t('音频'), document: t('文档'), text: t('文本'), archive: t('压缩包'), other: t('其它') }[k as string] }}
      </span>
      <span class="sum">{{ t("共") }} {{ humanSize(totalSize) }}</span>
    </div>
  </div>
</template>

<style scoped>
.tm {
  position: relative;
  width: 100%;
  height: 100%;
  min-height: 200px;
  overflow: hidden;
}
.empty {
  height: 100%;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--muted);
  font-size: 13px;
}
.cell {
  position: absolute;
  border: 1px solid;
  border-radius: 4px;
  overflow: hidden;
  cursor: pointer;
  transition: background .12s, transform .06s;
  padding: 4px 6px;
}
.cell.hot { background: rgba(79, 140, 255, 0.24) !important; }
.nm {
  font-size: 12px;
  font-weight: 600;
  color: #e2ebf8;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  text-shadow: 0 1px 2px #000;
}
.sz { font-size: 11px; color: #a9b8cf; margin-top: 2px; }
.nested-boxes { position: absolute; left: 4px; right: 4px; bottom: 4px; height: 34%; display: flex; gap: 2px; align-items: stretch; }
.nested-box { min-width: 0; overflow: hidden; display: flex; align-items: flex-end; padding: 3px 4px; border: 1px solid rgba(17,17,17,.42); background: rgba(255,255,255,.42); color: #333; font-size: 9px; text-overflow: ellipsis; white-space: nowrap; cursor: pointer; }
.nested-box:hover { background: rgba(17,17,17,.13); color: #111; }
.legend {
  position: absolute;
  right: 8px;
  bottom: 6px;
  display: flex;
  gap: 10px;
  align-items: center;
  font-size: 11px;
  color: var(--muted);
  background: rgba(11, 15, 22, 0.75);
  padding: 3px 8px;
  border-radius: 6px;
}
.lg { display: inline-flex; align-items: center; gap: 4px; }
.lg i { width: 8px; height: 8px; border-radius: 2px; display: inline-block; }
.sum { color: #c4d0e3; }
/* Preserve type-color coding while keeping text and controls monochrome. */
.cell { border-radius: 2px; }
.cell.hot { background: rgba(17, 17, 17, .12) !important; }
.nm { color: #111; text-shadow: none; }
.sz { color: #444; }
.legend { color: #333; background: rgba(255, 255, 255, .94); border: 1px solid #111; border-radius: 2px; }
.sum { color: #111; }
</style>
