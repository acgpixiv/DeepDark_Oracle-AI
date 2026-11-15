<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from "vue";
import { FileArchive, FileText, Film, Image as ImageIcon, Music2, PackageOpen } from "lucide-vue-next";
import { convertFileSrc } from "@tauri-apps/api/core";
import { api, type Hit } from "../api";
import PdfThumbnail from "./PdfThumbnail.vue";
import PdfPreview from "./PdfPreview.vue";
import { fmtTime, humanSize, kindColor, toast } from "../store";
import { t } from "../i18n";

const props = defineProps<{
  hits: Hit[];
  selectedId: number | null;
  selectedIds: Set<number>;
  thumbDir: string;
  loading?: boolean;
  query?: string;
  searchMode?: "fulltext" | "semantic" | "image" | "audio" | string;
}>();

const emit = defineEmits<{
  (e: "select", hit: Hit): void;
  (e: "open", hit: Hit): void;
  (e: "toggle", id: number): void;
  (e: "reveal", hit: Hit): void;
  (e: "rename", hit: Hit): void;
  (e: "reach-end"): void;
}>();

const iconFor: Record<string, any> = {
  image: ImageIcon,
  video: Film,
  audio: Music2,
  document: FileText,
  text: FileText,
  archive: FileArchive,
  other: PackageOpen,
};
const rows = computed(() => props.hits);
const hoverHit = ref<Hit | null>(null);
const hoverStyle = ref<Record<string, string>>({});
const contextHit = ref<Hit | null>(null);
const contextStyle = ref<Record<string, string>>({});
const hoverAnchor = ref<DOMRect | null>(null);
const failedThumbs = ref(new Set<number>());
const readyThumbs = ref(new Set<number>());
const previewRequests = new Set<number>();
let hoverTimer: number | undefined;
let closeTimer: number | undefined;

function thumbnail(hit: Hit): string {
  if (!(Boolean(hit.thumb_key) || readyThumbs.value.has(hit.id)) || !props.thumbDir || failedThumbs.value.has(hit.id)) return "";
  const sep = props.thumbDir.includes("\\") ? "\\" : "/";
  return convertFileSrc(`${props.thumbDir}${sep}${hit.id}.jpg`);
}
function markThumbFailed(id: number) {
  failedThumbs.value = new Set(failedThumbs.value).add(id);
}
function markThumbReady(id: number) {
  const nextFailed = new Set(failedThumbs.value);
  nextFailed.delete(id);
  failedThumbs.value = nextFailed;
  readyThumbs.value = new Set(readyThumbs.value).add(id);
}
function isDocumentThumbCandidate(hit: Hit): boolean {
  const ext = hit.ext.toLowerCase().replace(/^\./, "");
  // PDF cards render their first page with PdfThumbnail. Avoid asking the
  // backend to inspect every PDF again when the result grid is large.
  return ext === "docx";
}
async function ensureDocumentThumb(hit: Hit) {
  if (!isDocumentThumbCandidate(hit) || previewRequests.has(hit.id)) return;
  previewRequests.add(hit.id);
  try {
    if (await api.ensurePreview(hit.id, hit.path)) {
      const nextFailed = new Set(failedThumbs.value);
      nextFailed.delete(hit.id);
      failedThumbs.value = nextFailed;
      readyThumbs.value = new Set(readyThumbs.value).add(hit.id);
    }
  } catch {
    // The file may have disappeared between search and preview generation.
  }
}
function onThumbError(hit: Hit) {
  markThumbFailed(hit.id);
  void ensureDocumentThumb(hit);
}
function sourceUrl(hit: Hit): string { return convertFileSrc(hit.path); }
function isPdf(hit: Hit): boolean { return [".pdf", "pdf"].includes(hit.ext.toLowerCase()); }
function hitPage(hit: Hit): number | undefined {
  return hit.page != null && Number.isFinite(Number(hit.page)) ? Math.max(1, Math.floor(Number(hit.page))) : undefined;
}
function isVideo(hit: Hit): boolean { return hit.kind === "video"; }
function scoreClass(hit: Hit): string {
  const score = Math.max(0, Math.min(1, Number(hit.score) || 0));
  if (score >= 0.75) return "score-high";
  if (score >= 0.45) return "score-medium";
  return "score-low";
}
function displayScore(hit: Hit): string {
  return Math.max(0, Math.min(1, Number(hit.score) || 0)).toFixed(2);
}
function scoreHint(hit: Hit): string {
  const score = Math.max(0, Math.min(1, Number(hit.score) || 0));
  const reason = hit.reason || "";
  if (/全文|full/i.test(reason)) {
    // Full-text is the selected retrieval lane; its numeric score is still
    // a term-coverage score. A value below 1 means only part of a multi-term
    // query was present (for example 2/3 -> 0.76), rather than a semantic
    // penalty applied to an exact phrase.
    return score >= 0.999
      ? "全文匹配：完整短语或全部查询词命中"
      : "全文匹配：部分查询词命中，分数按词覆盖率计算";
  }
  return "相似度";
}
function updateHoverStyle() {
  const rect = hoverAnchor.value;
  if (!rect) return;
  const availableWidth = Math.max(1, window.innerWidth - 16);
  const availableHeight = Math.max(1, window.innerHeight - 16);
  clearTimeout(hoverTimer); clearTimeout(closeTimer);
  // Adapt the enlarged preview to the current WebView instead of using a
  // fixed 430x520 rectangle. Keep enough room for a readable PDF page while
  // respecting small laptop windows and high-DPI displays.
  const width = Math.min(availableWidth, Math.max(1, 200, Math.min(680, Math.round(window.innerWidth * 0.42))));
  const height = Math.min(availableHeight, Math.max(1, 240, Math.min(760, Math.round(window.innerHeight * 0.78))));
  const left = rect.right + width + 8 <= window.innerWidth ? rect.right + 8 : Math.max(8, rect.left - width - 8);
  const top = Math.max(8, Math.min(window.innerHeight - height - 8, rect.top));
  hoverStyle.value = { left: `${left}px`, top: `${top}px`, width: `${width}px`, height: `${height}px` };
}
function hoverStart(hit: Hit, event: MouseEvent) {
  clearTimeout(hoverTimer); clearTimeout(closeTimer);
  hoverAnchor.value = (event.currentTarget as HTMLElement).getBoundingClientRect();
  updateHoverStyle();
  hoverTimer = window.setTimeout(() => {
    void ensureDocumentThumb(hit);
    hoverHit.value = hit;
  }, 1500);
}
function hoverLeave() {
  clearTimeout(hoverTimer);
  closeTimer = window.setTimeout(() => { hoverHit.value = null; }, 260);
}
function onWindowResize() {
  if (hoverHit.value) updateHoverStyle();
}
function popoverEnter() { clearTimeout(closeTimer); }
function popoverLeave() { hoverHit.value = null; }
function openContext(hit: Hit, event: MouseEvent) {
  event.preventDefault(); event.stopPropagation(); contextHit.value = hit;
  contextStyle.value = { left: `${Math.min(event.clientX, window.innerWidth - 178)}px`, top: `${Math.min(event.clientY, window.innerHeight - 142)}px` };
}
function closeContext() { contextHit.value = null; }
async function copyPath(hit: Hit) {
  try { await navigator.clipboard.writeText(hit.path); toast(t("已复制文件路径"), "ok"); }
  catch (error) { toast(`${t("复制失败：")}${error}`, "error"); }
  closeContext();
}
function menuRename(hit: Hit) { emit("rename", hit); closeContext(); }
function menuReveal(hit: Hit) { emit("reveal", hit); closeContext(); }
function menuOpen(hit: Hit) { emit("open", hit); closeContext(); }
function escapeHtml(s: string): string { return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;"); }
function escapeRx(s: string): string { return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"); }
function highlight(s: string): string {
  const safe = escapeHtml(s); const query = (props.query || "").trim(); if (!query) return safe;
  const terms = Array.from(new Set([query, ...query.split(/[\s,，、]+/)] )).map((term) => term.trim()).filter(Boolean).sort((a, b) => b.length - a.length);
  return safe.replace(new RegExp(terms.map(escapeRx).join("|"), "gi"), (match) => `<mark>${match}</mark>`);
}
function tags(hit: Hit): string[] { return (hit.tags || "").split(",").filter(Boolean).slice(0, 2); }
function matchLabel(hit: Hit): string {
  // The selected retrieval lane defines the meaning of the result list.
  // Backend reasons may describe a secondary signal (for example a filename
  // match) and must not change the visible modality badge.
  switch (props.searchMode) {
    case "fulltext": return t("全文匹配");
    case "semantic": return t("语义相似");
    case "image": return t("图像相似");
    case "audio": return t("音频相似");
  }
  const reason = hit.reason || "";
  if (/音频|声音|audio/i.test(reason)) return t("音频相似");
  if (/图像|画面|图片|image/i.test(reason)) return t("图像相似");
  if (/全文|名称|路径|拼音|模糊|full/i.test(reason)) return t("全文匹配");
  if (/语义|semantic/i.test(reason)) return t("语义相似");
  return t("语义相似");
}
function hitLocation(hit: Hit): string {
  if (hit.page != null) return `P. ${hit.page}`;
  if (hit.start_seconds != null) {
    const format = (value: number) => {
      const seconds = Math.max(0, Math.floor(value));
      const minutes = Math.floor(seconds / 60);
      const remaining = seconds % 60;
      if (minutes >= 60) {
        return `${String(Math.floor(minutes / 60)).padStart(2, "0")}:${String(minutes % 60).padStart(2, "0")}:${String(remaining).padStart(2, "0")}`;
      }
      return `${String(minutes).padStart(2, "0")}:${String(remaining).padStart(2, "0")}`;
    };
    const start = Math.max(0, Number(hit.start_seconds));
    const end = Number(hit.end_seconds);
    // Video vectors currently identify a temporal window. Show its bounds so
    // a first-window hit is not mistaken for an exact match at 00:00; subtitle
    // matches are refined to the narrower cue bounds by the backend.
    return Number.isFinite(end) && end > start
      ? `${format(start)}-${format(end)}`
      : format(start);
  }
  return "";
}
function handleDocumentClick() { closeContext(); }
function onGalleryScroll(event: Event) {
  const element = event.currentTarget as HTMLElement;
  if (element.scrollTop + element.clientHeight >= element.scrollHeight - 560) emit("reach-end");
}
window.addEventListener("click", handleDocumentClick);
window.addEventListener("resize", onWindowResize);
onBeforeUnmount(() => {
  clearTimeout(hoverTimer); clearTimeout(closeTimer);
  window.removeEventListener("click", handleDocumentClick);
  window.removeEventListener("resize", onWindowResize);
});
</script>

<template>
  <div class="gallery" @scroll.passive="onGalleryScroll">
    <div v-if="loading" class="gallery-state">正在检索…</div>
    <div v-else-if="!rows.length" class="gallery-state"><div class="empty-title">没有找到匹配的文件</div><div>换一种描述，或到设置中的“文件索引”添加目录。</div></div>
    <div v-else class="grid">
      <article v-for="hit in rows" :key="hit.id" v-memo="[hit.id, hit.path, hit.mtime, hit.page, hit.thumb_key, hit.score, selectedId === hit.id, selectedIds.has(hit.id), readyThumbs.has(hit.id), failedThumbs.has(hit.id)]" class="result" :class="{ active: hit.id === selectedId }" tabindex="0"
        @click="emit('select', hit)" @dblclick="emit('open', hit)" @contextmenu="openContext(hit, $event)"
        @mouseenter="hoverStart(hit, $event)" @mouseleave="hoverLeave"
        @keydown.enter="emit('select', hit)" @keydown.space.prevent="emit('select', hit)">
        <div class="media" :style="{ '--kind-color': kindColor(hit.kind) }">
          <PdfThumbnail v-if="isPdf(hit)" :file-id="hit.id" :path="hit.path" :page="hitPage(hit)" :mtime="hit.mtime" @ready="markThumbReady(hit.id)" />
          <img v-else-if="thumbnail(hit)" :src="thumbnail(hit)" loading="lazy" decoding="async" fetchpriority="low" alt="" @error="onThumbError(hit)" />
          <video v-else-if="isVideo(hit)" class="video-card" :src="sourceUrl(hit)" muted preload="metadata"></video>
          <component :is="iconFor[hit.kind] || PackageOpen" v-else :size="30" stroke-width="1.5" />
          <span class="score" :class="scoreClass(hit)" :title="scoreHint(hit)" @click.stop @dblclick.stop>{{ displayScore(hit) }}</span>
          <input class="check" type="checkbox" :aria-label="`${t('选择')} ${hit.name} ${t('用于导出')}`" :checked="selectedIds.has(hit.id)" @click.stop @change="emit('toggle', hit.id)" />
        </div>
        <div class="details">
          <div class="title-line"><span class="name" :title="hit.name">{{ hit.name }}</span><span class="kind">{{ hit.kind_label }}</span></div>
          <div class="reason-line"><span class="reason">{{ matchLabel(hit) }}</span><span v-if="hitLocation(hit)" class="location">{{ hitLocation(hit) }}</span><span v-for="tag in tags(hit)" :key="tag" class="tag">{{ tag }}</span></div>
          <div v-if="hit.snippet || hit.subtitle" class="snippet" v-html="highlight(hit.snippet || hit.subtitle || '')"></div>
          <div class="meta"><span>{{ hit.size_label || humanSize(hit.size) }}</span><span>{{ hit.mtime_label || fmtTime(hit.mtime) }}</span></div>
        </div>
      </article>
    </div>
    <div v-if="hoverHit" class="preview-popover" :style="hoverStyle" @mouseenter="popoverEnter" @mouseleave="popoverLeave">
      <PdfPreview v-if="isPdf(hoverHit)" :path="hoverHit.path" :initial-page="hitPage(hoverHit)" />
      <img v-else-if="thumbnail(hoverHit)" :src="thumbnail(hoverHit)" alt="" @error="markThumbFailed(hoverHit.id)" />
        <video v-else-if="isVideo(hoverHit)" :src="sourceUrl(hoverHit)" controls></video>
    </div>
    <div v-if="contextHit" class="context-menu" :style="contextStyle" @click.stop>
      <button @click="menuOpen(contextHit)">打开</button><button @click="menuRename(contextHit)">重命名</button><button @click="menuReveal(contextHit)">在资源管理器中打开</button><button @click="copyPath(contextHit)">复制路径</button>
    </div>
  </div>
</template>

<style scoped>
.gallery { position: relative; min-height: 0; height: 100%; overflow: auto; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(180px, 1fr)); align-content: start; gap: 12px; padding: 2px 2px 18px; }
.result { min-width: 0; overflow: hidden; border: 1px solid transparent; border-radius: 2px; background: #fff; cursor: pointer; content-visibility: auto; contain-intrinsic-size: 220px; transition: border-color .15s, background .15s; animation: result-in .24s ease both; }
.result:nth-child(2) { animation-delay: 18ms; }.result:nth-child(3) { animation-delay: 36ms; }.result:nth-child(4) { animation-delay: 54ms; }.result:nth-child(5) { animation-delay: 72ms; }.result:nth-child(6) { animation-delay: 90ms; }.result:nth-child(7) { animation-delay: 108ms; }.result:nth-child(8) { animation-delay: 126ms; }
@keyframes result-in { from { opacity: 0; transform: translateY(7px); } to { opacity: 1; transform: translateY(0); } }
.result:hover { border-color: #111; background: #f5f5f5; }.result.active { border-color: #111; background: #f0f0f0; }
.media { position: relative; aspect-ratio: 4 / 3; display: grid; place-items: center; overflow: hidden; color: var(--kind-color); background: #f2f2f2; }
.media img, .video-card, .pdf-card { width: 100%; height: 100%; object-fit: cover; border: 0; }.pdf-card { background: #fff; pointer-events: none; }.video-card { object-fit: contain; }
.score { position: absolute; left: 8px; bottom: 8px; padding: 3px 6px; border: 1px solid currentColor; border-radius: 2px; background: rgba(255,255,255,.92); font: 11px Consolas, monospace; font-variant-numeric: tabular-nums; user-select: none; }.score-high { color: #16733a; }.score-medium { color: #a46300; }.score-low { color: #8c1d24; }.check { position: absolute; top: 8px; right: 8px; width: 17px; height: 17px; accent-color: #111; cursor: pointer; }
.details { padding: 9px 10px 10px; }.title-line { display: flex; align-items: baseline; gap: 6px; min-width: 0; }.name { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12px; font-weight: 600; }.kind { flex: none; color: #666; font-size: 10px; }.reason-line { min-height: 18px; display: flex; gap: 4px; overflow: hidden; padding-top: 5px; }.reason, .tag { flex: none; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; padding: 1px 5px; border-radius: 2px; font-size: 10px; }.reason { color: #333; background: #f1f1f1; }.tag { color: #222; background: #f7f7f7; border: 1px solid #aaa; }.snippet { margin-top: 5px; min-height: 30px; color: #444; font-size: 10px; line-height: 1.5; display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden; }.snippet :deep(mark) { color: #111; background: #eee; border-bottom: 1px solid #111; }.meta { display: flex; justify-content: space-between; gap: 6px; margin-top: 8px; color: #666; font-size: 10px; font-variant-numeric: tabular-nums; }.meta span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.location { flex: none; padding: 1px 5px; border: 1px solid #111; border-radius: 2px; color: #111; font: 10px Consolas, monospace; font-variant-numeric: tabular-nums; }
.preview-popover { position: fixed; z-index: 100; width: 430px; height: 520px; display: flex; flex-direction: column; border: 1px solid #111; border-radius: 2px; background: #fff; box-shadow: 0 16px 40px rgba(0,0,0,.24); overflow: hidden; }.popover-head { display: none; }.preview-popover > .pdf-preview, .preview-popover > img, .preview-popover > video { flex: 1; width: 100%; min-height: 0; border: 0; object-fit: contain; background: #fff; overflow: hidden; }.page-indicator { position: absolute; left: 50%; bottom: 12px; transform: translateX(-50%); padding: 4px 10px; border-radius: 2px; background: rgba(0,0,0,.62); color: #fff; font: 12px Consolas, monospace; pointer-events: none; }.popover-empty { display: grid; place-items: center; flex: 1; color: #666; font-size: 12px; }
.context-menu { position: fixed; z-index: 120; width: 170px; padding: 4px; border: 1px solid #111; border-radius: 2px; background: #fff; box-shadow: 0 8px 24px rgba(0,0,0,.18); }.context-menu button { display: block; width: 100%; padding: 8px 10px; border: 0; background: transparent; color: #111; text-align: left; font-size: 12px; cursor: pointer; }.context-menu button:hover { background: #eee; }
.gallery-state { min-height: 240px; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 8px; color: #666; font-size: 12px; }.empty-title { color: #111; font-size: 14px; }
@media (max-width: 700px) { .grid { grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 8px; } .preview-popover { width: min(430px, calc(100vw - 16px)); height: min(520px, calc(100vh - 16px)); } }
@media (prefers-reduced-motion: reduce) { .result, .result:nth-child(n) { animation: none; } }
</style>
