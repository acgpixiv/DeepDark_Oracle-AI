<script setup lang="ts">
// 右侧预览：缩略图 + 元信息 + 正文片段
import { computed, ref, watch } from "vue";
import { convertFileSrc } from "@tauri-apps/api/core";
import type { Hit } from "../api";
import { fmtTime, humanSize, kindColor } from "../store";
import { api } from "../api";

const props = defineProps<{ hit: Hit | null; thumbDir: string }>();
const emit = defineEmits<{
  (e: "open", hit: Hit): void;
  (e: "reveal", hit: Hit): void;
  (e: "similar", hit: Hit): void;
}>();

const imgFailed = ref(false);

const thumbUrl = computed(() => {
  const h = props.hit;
  // 后端的 `thumb` 是「有没有缩略图」的布尔值，文件名固定按 file id 生成
  // （索引时写的是 `<fid>.jpg`），这里不能拿布尔值当文件名拼。
  if (!h || !h.thumb || !props.thumbDir) return "";
  const sep = props.thumbDir.includes("\\") ? "\\" : "/";
  // Page-level PDF results persist their rendered page independently. Keep
  // the legacy file thumbnail for older index rows without a page number.
  const page = h.page != null && Number.isFinite(Number(h.page))
    ? Math.max(1, Math.floor(Number(h.page)))
    : undefined;
  const file = page ? `${h.id}_p${page}.jpg` : (h.thumb_key || `${h.id}.jpg`);
  return convertFileSrc(`${props.thumbDir}${sep}${file}`);
});

const fileUrl = computed(() => {
  const h = props.hit;
  if (!h || !["image", "video"].includes(h.kind)) return "";
  return convertFileSrc(h.path);
});

watch(
  () => props.hit?.id,
  () => {
    imgFailed.value = false;
  }
);

async function copyPath() {
  if (!props.hit) return;
  try {
    await navigator.clipboard.writeText(props.hit.path);
  } catch {
    /* ignore */
  }
}
</script>

<template>
  <aside class="preview card">
    <div class="head">
      <span class="t">预览</span>
      <div class="acts" v-if="hit">
        <button class="mini" @click="emit('open', hit)">打开</button>
        <button class="mini" @click="emit('reveal', hit)">定位</button>
        <button class="mini" @click="emit('similar', hit)">找相似</button>
      </div>
    </div>

    <div v-if="!hit" class="none">选择文件查看预览</div>

    <template v-else>
      <div class="thumb">
        <img
          v-if="thumbUrl && !imgFailed"
          :src="thumbUrl"
          @error="imgFailed = true"
          alt=""
        />
        <video
          v-else-if="hit.kind === 'video' && fileUrl"
          :src="fileUrl"
          controls
        ></video>
        <div v-else class="ph" :style="{ color: kindColor(hit.kind) }">
          {{ hit.ext ? hit.ext.replace(".", "").toUpperCase() : "FILE" }}
        </div>
      </div>

      <div class="meta">
        <div class="name" :title="hit.name">{{ hit.name }}</div>
        <dl>
          <dt>类型</dt><dd>{{ hit.kind_label }}</dd>
          <dt>大小</dt><dd>{{ hit.size_label || humanSize(hit.size) }}</dd>
          <dt>修改</dt><dd>{{ hit.mtime_label || fmtTime(hit.mtime) }}</dd>
          <dt v-if="hit.category">分类</dt><dd v-if="hit.category">{{ hit.category }}</dd>
          <dt>匹配</dt>
          <dd>
            {{ (hit.score || 0).toFixed(2) }}
            <span v-if="hit.reason" class="reason">{{ hit.reason }}</span>
          </dd>
        </dl>
        <div class="path mono" :title="hit.path" @click="copyPath">{{ hit.path }}</div>
      </div>

      <div class="body">
        <template v-if="hit.tags">
          <div class="sec">画面标签</div>
          <div class="tags">
            <span v-for="t in hit.tags.split(',').filter(Boolean)" :key="t" class="tag">{{ t }}</span>
          </div>
        </template>
        <template v-if="hit.snippet">
          <div class="sec">命中片段</div>
          <p class="snippet">{{ hit.snippet }}</p>
        </template>
        <div v-else-if="!hit.tags" class="tip">
          双击结果可直接打开；右键或上方按钮可定位到资源管理器。
        </div>
      </div>
    </template>
  </aside>
</template>

<style scoped>
.preview {
  display: flex;
  flex-direction: column;
  min-width: 260px;
  overflow: hidden;
}
.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 12px;
  border-bottom: 1px solid var(--border);
}
.t { font-size: 13px; color: #b7c4da; }
.acts { display: flex; gap: 4px; }
.mini {
  padding: 3px 8px;
  font-size: 11px;
  border-radius: 6px;
}
.none {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--muted);
  font-size: 12px;
}
.thumb {
  height: 190px;
  margin: 10px;
  background: #0d121c;
  border: 1px solid var(--border);
  border-radius: 8px;
  display: flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
}
.thumb img, .thumb video { max-width: 100%; max-height: 100%; object-fit: contain; }
.ph { font-size: 22px; font-weight: 700; letter-spacing: 1px; opacity: .8; }
.meta { padding: 0 12px 10px; }
.name {
  font-size: 13px;
  font-weight: 600;
  margin-bottom: 8px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
dl {
  display: grid;
  grid-template-columns: 46px 1fr;
  gap: 3px 8px;
  margin: 0;
  font-size: 12px;
}
dt { color: var(--muted); }
dd { margin: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.reason { color: var(--muted); font-size: 11px; margin-left: 4px; }
.path {
  margin-top: 8px;
  font-size: 11px;
  color: var(--muted);
  word-break: break-all;
  cursor: copy;
  line-height: 1.4;
}
.body {
  flex: 1;
  overflow: auto;
  padding: 10px 12px;
  border-top: 1px solid var(--border);
  min-height: 0;
}
.sec { font-size: 11px; color: var(--muted); margin-bottom: 6px; }
.snippet { margin: 0; font-size: 12px; line-height: 1.6; color: #c4d0e3; white-space: pre-wrap; }
.tags { display: flex; flex-wrap: wrap; gap: 5px; }
.tag {
  font-size: 11px;
  color: #7fd1c0;
  background: rgba(34, 197, 94, 0.1);
  border: 1px solid rgba(34, 197, 94, 0.25);
  border-radius: 5px;
  padding: 1px 6px;
}
.tip { font-size: 12px; color: var(--muted); line-height: 1.6; }
.t { color: #111; }
.mini { border-radius: 2px; background: #fff; border-color: #111; color: #111; }
.mini:hover { background: #f0f0f0; }
.thumb { background: #f2f2f2; border-radius: 2px; }
.snippet { color: #333; }
.tag { color: #222; background: #f7f7f7; border-color: #aaa; border-radius: 2px; }
</style>
