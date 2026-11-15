<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { convertFileSrc } from "@tauri-apps/api/core";
import workerUrl from "pdfjs-dist/build/pdf.worker.min.mjs?url";

const props = defineProps<{ path: string; initialPage?: number }>();
const emit = defineEmits<{
  (event: "page-count", count: number): void;
  (event: "page-change", page: number): void;
}>();

let pdfJsModule: Promise<typeof import("pdfjs-dist")> | undefined;
async function loadPdfJs() {
  pdfJsModule ??= import("pdfjs-dist");
  const pdfjs = await pdfJsModule;
  pdfjs.GlobalWorkerOptions.workerSrc = workerUrl;
  return pdfjs;
}

function resourceUrl(kind: "cmaps" | "standard_fonts") {
  // Vite emits these PDF.js resources next to the application bundle. Keeping
  // the URL relative to the current Tauri/WebView origin also works in the
  // installed app where the frontend is served from a custom origin.
  return new URL(`/pdfjs/${kind}/`, window.location.origin).toString();
}

const host = ref<HTMLElement | null>(null);
const canvas = ref<HTMLCanvasElement | null>(null);
const loading = ref(true);
const failed = ref(false);
const page = ref(1);
const pageCount = ref(0);
let disposed = false;
let pdf: any;
let loadingTask: { destroy: () => Promise<void> } | undefined;
let renderTask: { cancel: () => void; promise: Promise<void> } | undefined;
let wheelLock = false;
let resizeObserver: ResizeObserver | undefined;
let resizeFrame = 0;
let renderGeneration = 0;
let loadGeneration = 0;

async function renderPage() {
  if (disposed || !pdf || !canvas.value || !host.value) return;
  const generation = ++renderGeneration;
  renderTask?.cancel();
  const pdfPage = await pdf.getPage(page.value);
  if (disposed || generation !== renderGeneration || !canvas.value || !host.value) return;
  const base = pdfPage.getViewport({ scale: 1 });
  const width = Math.max(1, host.value.clientWidth || 430);
  // The preview is a width-first surface.  Keeping the page width equal to
  // the WebView/hover window makes portrait and landscape pages equally
  // readable; a tall page may extend below the clipped preview area and can
  // be inspected in full by opening the source document.
  const scale = width / base.width;
  const outputScale = Math.min(window.devicePixelRatio || 1, 2);
  const viewport = pdfPage.getViewport({ scale: scale * outputScale });
  canvas.value.width = Math.max(1, Math.floor(viewport.width));
  canvas.value.height = Math.max(1, Math.floor(viewport.height));
  canvas.value.style.width = `${Math.ceil(viewport.width / outputScale)}px`;
  canvas.value.style.height = `${Math.ceil(viewport.height / outputScale)}px`;
  const context = canvas.value.getContext("2d");
  if (!context) return;
  renderTask = pdfPage.render({ canvas: canvas.value, canvasContext: context, viewport });
  try { await renderTask.promise; } catch (error) {
    if ((error as Error)?.name !== "RenderingCancelledException") throw error;
  }
  if (disposed || generation !== renderGeneration) return;
  pdfPage.cleanup();
  loading.value = false;
  emit("page-change", page.value);
}

async function openPdf() {
  const generation = ++loadGeneration;
  try {
    await loadPdf(generation);
  } catch (error) {
    // A request for an older file can finish after the preview has moved on.
    // Ignore that stale failure so it cannot hide the newer page.
    if (disposed || generation !== loadGeneration) return;
    throw error;
  }
}

async function loadPdf(generation: number) {
  renderGeneration++;
  renderTask?.cancel();
  const oldTask = loadingTask;
  const oldPdf = pdf;
  loadingTask = undefined;
  pdf = undefined;
  if (oldTask) { try { await oldTask.destroy(); } catch { /* best effort */ } }
  if (oldPdf) { try { await oldPdf.destroy(); } catch { /* best effort */ } }
  if (disposed || generation !== loadGeneration) return;
  loading.value = true;
  failed.value = false;
  const { getDocument } = await loadPdfJs();
  if (disposed || generation !== loadGeneration) return;
  const url = convertFileSrc(props.path);
  const pdfResources = {
    cMapUrl: resourceUrl("cmaps"),
    cMapPacked: true,
    standardFontDataUrl: resourceUrl("standard_fonts"),
    // Tauri's WebView origin does not always allow a PDF.js worker to fetch
    // auxiliary CMaps/font files directly.  Let the main WebView fetch and
    // transfer them instead; this keeps CJK glyphs visible for PDFs whose
    // embedded font has an incomplete ToUnicode map.
    useWorkerFetch: false,
    // Some manuals use embedded CID fonts with incomplete ToUnicode maps.
    // Keep the browser/system font fallback enabled and ask PDF.js to retain
    // the extra font metadata needed by its fallback path.  Without these
    // options, PDF.js can successfully load the page but paint only its
    // images/background while silently dropping glyphs.
    useSystemFonts: true,
    fontExtraProperties: true,
    stopAtErrors: false,
  };
  let task = getDocument({ url, ...pdfResources, isEvalSupported: false });
  loadingTask = task;
  let usedMainThreadFallback = false;
  try {
    const loaded = await task.promise;
    if (disposed || generation !== loadGeneration) { await loaded.destroy(); return; }
    pdf = loaded;
  } catch {
    try { await task.destroy(); } catch { /* already failed */ }
    if (disposed || generation !== loadGeneration) return;
    usedMainThreadFallback = true;
    task = getDocument({ url, ...pdfResources, disableWorker: true, useWorkerFetch: false, isEvalSupported: false });
    loadingTask = task;
    const loaded = await task.promise;
    if (disposed || generation !== loadGeneration) { await loaded.destroy(); return; }
    pdf = loaded;
  }
  if (disposed || generation !== loadGeneration) return;
  pageCount.value = pdf.numPages || 1;
  page.value = Math.min(pageCount.value, Math.max(1, Math.floor(props.initialPage || 1)));
  emit("page-count", pageCount.value);
  try {
    await renderPage();
  } catch (error) {
    // A worker can load the document successfully and still fail while
    // resolving a CJK font/CMap. Retry once in the WebView thread so the
    // preview remains usable on Tauri/WebView2 installations with stricter
    // worker URL policies.
    if (disposed || generation !== loadGeneration) return;
    if (usedMainThreadFallback) throw error;
    try { await task.destroy(); } catch { /* best effort */ }
    try { await pdf?.destroy(); } catch { /* best effort */ }
    if (generation !== loadGeneration) return;
    usedMainThreadFallback = true;
    task = getDocument({ url, ...pdfResources, disableWorker: true, useWorkerFetch: false, isEvalSupported: false });
    loadingTask = task;
    pdf = await task.promise;
    if (disposed || generation !== loadGeneration) return;
    pageCount.value = pdf.numPages || 1;
    page.value = Math.min(pageCount.value, Math.max(1, Math.floor(props.initialPage || 1)));
    emit("page-count", pageCount.value);
    await renderPage();
  }
}

function movePage(delta: number) {
  if (!pageCount.value || wheelLock) return;
  const next = Math.max(1, Math.min(pageCount.value, page.value + delta));
  if (next === page.value) return;
  page.value = next;
  wheelLock = true;
  window.setTimeout(() => { wheelLock = false; }, 120);
  void renderPage().catch(() => { failed.value = true; });
}

function onWheel(event: WheelEvent) {
  event.preventDefault();
  if (Math.abs(event.deltaY) >= 2) movePage(event.deltaY > 0 ? 1 : -1);
}

onMounted(() => {
  void openPdf().catch(() => { loading.value = false; failed.value = true; });
  if (host.value && "ResizeObserver" in window) {
    resizeObserver = new ResizeObserver(() => {
      if (resizeFrame) cancelAnimationFrame(resizeFrame);
      resizeFrame = requestAnimationFrame(() => {
        resizeFrame = 0;
        void renderPage().catch(() => { failed.value = true; });
      });
    });
    resizeObserver.observe(host.value);
  }
});
watch(() => [props.path, props.initialPage], () => {
  page.value = Math.max(1, Math.floor(props.initialPage || 1));
  pageCount.value = 0;
  void openPdf().catch(() => { loading.value = false; failed.value = true; });
});
onBeforeUnmount(() => {
  disposed = true;
  loadGeneration++;
  renderGeneration++;
  resizeObserver?.disconnect();
  if (resizeFrame) cancelAnimationFrame(resizeFrame);
  renderTask?.cancel();
  if (loadingTask) void loadingTask.destroy();
  if (pdf) void pdf.destroy();
});
</script>

<template>
  <div ref="host" class="pdf-preview" @wheel="onWheel">
    <canvas v-show="!failed" ref="canvas"></canvas>
    <span v-if="loading && !failed" class="loading"></span>
    <span v-if="failed" class="failed">PDF</span>
    <span v-if="pageCount" class="page-indicator">{{ page }} / {{ pageCount }}</span>
  </div>
</template>

<style scoped>
.pdf-preview { position: relative; display: flex; align-items: flex-start; justify-content: center; width: 100%; height: 100%; overflow: hidden; background: #fff; }
canvas { display: block; width: 100%; max-width: none; height: auto; background: #fff; }
.loading { position: absolute; left: 50%; top: 50%; width: 18px; height: 18px; margin: -9px; border: 2px solid #bbb; border-top-color: #111; border-radius: 50%; animation: spin .8s linear infinite; }
.failed { position: absolute; inset: 0; display: grid; place-items: center; color: #555; font-size: 12px; }
.page-indicator { position: absolute; left: 50%; bottom: 12px; transform: translateX(-50%); padding: 4px 10px; border-radius: 2px; background: rgba(0,0,0,.62); color: #fff; font: 12px Consolas, monospace; pointer-events: none; }
@keyframes spin { to { transform: rotate(360deg); } }
</style>
