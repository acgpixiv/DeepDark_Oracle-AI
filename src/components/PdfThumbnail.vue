<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { FileText } from "lucide-vue-next";
import { convertFileSrc } from "@tauri-apps/api/core";
import workerUrl from "pdfjs-dist/build/pdf.worker.min.mjs?url";
import { api } from "../api";
import { getPdfThumbnail, pdfThumbnailCacheKey, rememberPdfThumbnail } from "./pdfThumbnailCache";

const props = defineProps<{ fileId: number; path: string; page?: number; mtime?: number }>();
const emit = defineEmits<{ (event: "ready"): void }>();

function pageCacheKey() {
  return pdfThumbnailCacheKey(props.fileId, props.path, props.mtime, props.page);
}

let pdfJsModule: Promise<typeof import("pdfjs-dist")> | undefined;
async function loadPdfJs() {
  pdfJsModule ??= import("pdfjs-dist");
  const pdfjs = await pdfJsModule;
  pdfjs.GlobalWorkerOptions.workerSrc = workerUrl;
  return pdfjs;
}

function resourceUrl(kind: "cmaps" | "standard_fonts") {
  return new URL(`/pdfjs/${kind}/`, window.location.origin).toString();
}

let activeRenders = 0;
const renderWaiters: Array<() => void> = [];
async function acquireRenderSlot() {
  if (activeRenders >= 2) await new Promise<void>((resolve) => renderWaiters.push(resolve));
  activeRenders++;
}
function releaseRenderSlot() {
  activeRenders--;
  renderWaiters.shift()?.();
}

const host = ref<HTMLElement | null>(null);
const canvas = ref<HTMLCanvasElement | null>(null);
const rendered = ref(false);
const failed = ref(false);
const cachedSrc = ref<string | null>(null);
let disposed = false;
let observer: IntersectionObserver | undefined;
let loadingTask: { destroy: () => Promise<void> } | undefined;
let renderTask: { cancel: () => void; promise: Promise<void> } | undefined;
let renderGeneration = 0;

function currentRequest() {
  return {
    fileId: props.fileId,
    path: props.path,
    page: Number.isFinite(props.page) ? Math.max(1, Math.floor(props.page || 1)) : 1,
    mtime: props.mtime,
  };
}

function requestKey(request: ReturnType<typeof currentRequest>) {
  return pdfThumbnailCacheKey(request.fileId, request.path, request.mtime, request.page);
}

function requestStillCurrent(request: ReturnType<typeof currentRequest>, generation: number) {
  return !disposed && generation === renderGeneration
    && request.fileId === props.fileId
    && request.path === props.path
    && request.page === (Number.isFinite(props.page) ? Math.max(1, Math.floor(props.page || 1)) : 1)
    && request.mtime === props.mtime;
}

async function renderCover() {
  const request = currentRequest();
  const cacheKey = requestKey(request);
  const cached = getPdfThumbnail(cacheKey);
  if (cached) {
    cachedSrc.value = cached;
    rendered.value = true;
    emit("ready");
    return;
  }
  // Capture the request generation before waiting for a render slot.  A card
  // can be reused for another hit while it is queued; capturing it after the
  // wait lets the old PDF raster be stored under the new page's cache key.
  const generation = renderGeneration;
  await acquireRenderSlot();
  let pdf: any;
  let localLoadingTask: { destroy: () => Promise<void> } | undefined;
  try {
    if (!requestStillCurrent(request, generation) || !canvas.value) return;
    const canvasElement = canvas.value;
    const { getDocument } = await loadPdfJs();
    if (!requestStillCurrent(request, generation)) return;
    const url = convertFileSrc(request.path);
    // WebView2 can reject a module worker for local asset URLs. Try the
    // normal worker path first, then render in the current WebView thread so
    // one unsupported worker does not leave every PDF card blank.
    const pdfResources = {
      cMapUrl: resourceUrl("cmaps"),
      cMapPacked: true,
      standardFontDataUrl: resourceUrl("standard_fonts"),
      // Fetch CMaps/fonts from the WebView main thread.  This is more
      // reliable than worker fetches under the tauri:// custom origin.
      useWorkerFetch: false,
      useSystemFonts: true,
      fontExtraProperties: true,
      stopAtErrors: false,
    };
    let loading = getDocument({ url, ...pdfResources, isEvalSupported: false });
    localLoadingTask = loading;
    loadingTask = loading;
    let pdf;
    try {
      pdf = await loading.promise;
    } catch (workerError) {
      try { await loading.destroy(); } catch { /* already failed */ }
      if (disposed) return;
      loading = getDocument({ url, ...pdfResources, disableWorker: true, useWorkerFetch: false, isEvalSupported: false });
      localLoadingTask = loading;
      loadingTask = loading;
      pdf = await loading.promise;
    }
    if (!requestStillCurrent(request, generation)) return;
    const page = await pdf.getPage(Math.min(pdf.numPages || 1, request.page));
    const base = page.getViewport({ scale: 1 });
    // Fill the card by width for both portrait and landscape pages. A tall
    // portrait page may be cropped vertically, but its text stays readable.
    const displayWidth = Math.max(1, host.value?.clientWidth || 320);
    const scale = displayWidth / base.width;
    const outputScale = Math.min(window.devicePixelRatio || 1, 2);
    const viewport = page.getViewport({ scale: scale * outputScale });
    canvasElement.width = Math.max(1, Math.floor(viewport.width));
    canvasElement.height = Math.max(1, Math.floor(viewport.height));
    canvasElement.style.width = `${displayWidth}px`;
    canvasElement.style.height = `${Math.ceil(viewport.height / outputScale)}px`;
    renderTask = page.render({ canvas: canvasElement, canvasContext: canvasElement.getContext("2d")!, viewport });
    await renderTask.promise;
    if (!requestStillCurrent(request, generation)) return;
    // Cache the completed raster even if the result card was unmounted while
    // PDF.js was drawing it (for example, when the user changed a filter).
    // This avoids paying the render cost again when that result reappears.
    const dataUrl = canvasElement.toDataURL("image/jpeg", 0.84);
    rememberPdfThumbnail(cacheKey, dataUrl);
    if (!requestStillCurrent(request, generation)) return;
    rendered.value = true;
    try {
      await api.savePdfThumbnail(request.fileId, request.path, dataUrl, request.page);
      emit("ready");
    } catch {
      // Keep the rendered canvas visible when the persistent cache cannot be written.
    }
  } catch {
    if (!disposed && generation === renderGeneration) failed.value = true;
  } finally {
    renderTask = undefined;
    try { await pdf?.destroy(); } catch { /* best effort */ }
    if (loadingTask === localLoadingTask) loadingTask = undefined;
    releaseRenderSlot();
  }
}

onMounted(() => {
  if (!host.value || !("IntersectionObserver" in window)) {
    void renderCover();
    return;
  }
  observer = new IntersectionObserver((entries) => {
    if (!entries.some((entry) => entry.isIntersecting)) return;
    observer?.disconnect();
    void renderCover();
  }, { rootMargin: "180px" });
  observer.observe(host.value);
});

watch(() => [props.path, props.page, props.mtime], () => {
  if (disposed) return;
  renderGeneration++;
  renderTask?.cancel();
  failed.value = false;
  rendered.value = false;
  cachedSrc.value = null;
  if (loadingTask) void loadingTask.destroy();
  void renderCover();
});

onBeforeUnmount(() => {
  disposed = true;
  renderGeneration++;
  observer?.disconnect();
  renderTask?.cancel();
  if (loadingTask) void loadingTask.destroy();
});
</script>

<template>
  <div ref="host" class="pdf-thumb">
    <img v-if="cachedSrc" class="cached-page" :src="cachedSrc" alt="" />
    <canvas v-else v-show="rendered" ref="canvas"></canvas>
    <FileText v-if="!rendered && failed" :size="30" stroke-width="1.5" />
    <span v-else-if="!rendered" class="pdf-loading"></span>
  </div>
</template>

<style scoped>
.pdf-thumb { position: absolute; inset: 0; display: flex; align-items: flex-start; justify-content: flex-start; overflow: hidden; background: #fff; }
canvas, .cached-page { display: block; width: 100%; max-width: none; height: auto; }
.pdf-loading { width: 18px; height: 18px; border: 2px solid #bbb; border-top-color: #111; border-radius: 50%; animation: turn .8s linear infinite; }
@keyframes turn { to { transform: rotate(360deg); } }
</style>
