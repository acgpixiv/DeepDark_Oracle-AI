<script setup lang="ts">
// 扫描视图：选择目录 → 扫描 → 进度 → 建立索引
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { open } from "@tauri-apps/plugin-dialog";
import { Check } from "lucide-vue-next";
import { api, type AppConfig, type ScanProgress, type IndexStats, type RootIndexStats } from "../api";
import { refreshStats, store, toast } from "../store";
import { t } from "../i18n";

const cfg = ref<AppConfig | null>(null);
const roots = ref<string[]>([]);
const rootEnabled = ref<Record<string, boolean>>({});
const selectedRoot = ref<string | null>(null);
const rootStats = ref<Record<string, RootIndexStats>>({});
const progress = ref<ScanProgress | null>(null);
const idx = ref<IndexStats | null>(null);
const polling = ref<number | null>(null);
const operationActive = ref(false);
const cancelPending = ref(false);
const activeOperationRoots = ref<string[]>([]);
const cancelledIndexPct = ref<number | null>(null);
const selectedExtensions = ref<string[]>([]);
const knownExtensions = ref<string[]>([]);
const rootExtensions = ref<Record<string, string[]>>({});
const rootEnabledExtensions = ref<Record<string, string[]>>({});
const extensionGroupOpen = ref("");
const excludeDraft = ref("");
const excludedPaths = ref<string[]>([]);
const modalExcludedPaths = ref<string[]>([]);
const exclusionsOpen = ref(false);
let defaultsAppliedForRun = false;
let lastRootRefresh = 0;

const TYPES = [
  { k: "images", label: "图片" },
  { k: "videos", label: "视频" },
  { k: "audios", label: "音频" },
  { k: "documents", label: "文档" },
  { k: "archives", label: "压缩包" },
  { k: "others", label: "其它" },
];

const typeState = ref<Record<string, boolean>>({
  images: true,
  videos: true,
  audios: true,
  pdf: true,
  documents: true,
  archives: true,
  others: false,
});

async function loadCfg() {
  try {
    cfg.value = await api.getConfig();
    roots.value = [...(cfg.value.scan.roots || [])];
    const disabled = new Set<string>(cfg.value.scan.disabled_roots || []);
    rootEnabled.value = Object.fromEntries(roots.value.map((r) => [r, !disabled.has(r)]));
    selectedRoot.value = roots.value.find((root) => rootEnabled.value[root] !== false) || null;
    const s = cfg.value.scan;
    typeState.value = {
      images: s.images !== false,
      videos: s.videos !== false,
      audios: s.audios !== false,
      pdf: s.pdf !== false,
      documents: s.documents !== false,
      archives: s.archives !== false,
      others: !!s.others,
    };
    selectedExtensions.value = [...(s.enabled_exts || [])];
    knownExtensions.value = [...(s.known_exts || [])];
    rootExtensions.value = { ...(s.root_extensions || {}) };
    rootEnabledExtensions.value = { ...(s.root_enabled_exts || {}) };
    if (selectedRoot.value && rootEnabledExtensions.value[rootKey(selectedRoot.value)]) {
      selectedExtensions.value = [...rootEnabledExtensions.value[rootKey(selectedRoot.value)]];
    }
    excludedPaths.value = [...(s.exclude_paths || [])];
  } catch (e) {
    toast(`${t("读取配置失败：")}${e}`, "error");
  }
}

const activeRoots = computed(() => roots.value.filter((r) => rootEnabled.value[r] !== false));
const hasSelectedRoot = computed(() => !!selectedRoot.value && roots.value.includes(selectedRoot.value));
const rootKey = (root: string) => {
  let normalized = root.replaceAll("/", "\\").replace(/\\+/g, "\\");
  if (!/^[a-z]:\\$/i.test(normalized)) normalized = normalized.replace(/\\+$/, "");
  return normalized.toLowerCase();
};
const selectedRootStats = computed(() => selectedRoot.value ? rootStats.value[rootKey(selectedRoot.value)] : undefined);
const liveSelectedRoot = computed(() => selectedRoot.value
  ? progress.value?.roots?.find((row) => rootKey(row.root) === rootKey(selectedRoot.value!))
  : undefined);
const rootExtensionRows = computed(() => {
  if (liveSelectedRoot.value?.extensions?.length) return liveSelectedRoot.value.extensions;
  const saved = selectedRoot.value ? rootExtensions.value[rootKey(selectedRoot.value)] : undefined;
  if (saved?.length) return saved.map((ext) => ({ ext, kind: kindForExt(ext), count: 0 }));
  return progress.value?.extensions || [];
});

function kindForExt(ext: string): string {
  const value = ext.toLowerCase();
  if (["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "ico", "heic", "avif", "svg"].includes(value)) return "image";
  if (["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts", "3gp"].includes(value)) return "video";
  if (["mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "opus", "aiff", "ape"].includes(value)) return "audio";
  if (["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "iso", "cab", "tgz"].includes(value)) return "archive";
  if (["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "epub", "mobi", "rtf"].includes(value)) return "document";
  if (["txt", "md", "markdown", "log", "csv", "tsv", "json", "jsonl", "xml", "yaml", "yml", "toml", "ini", "cfg", "conf", "html", "htm", "css", "js", "jsx", "ts", "tsx", "vue", "py", "rs", "go", "java", "c", "h", "cpp", "hpp", "cs", "php", "rb", "sh", "bat", "ps1", "sql", "lua", "kt", "swift", "m", "r", "jl", "tex", "rst", "srt", "ass"].includes(value)) return "text";
  return "other";
}

function groupForExt(ext: string): string {
  const kind = kindForExt(ext);
  if (kind === "image") return "images";
  if (kind === "video") return "videos";
  if (kind === "audio") return "audios";
  if (kind === "archive") return "archives";
  if (kind === "document" || kind === "text") return "documents";
  return "others";
}

const groupedExtensions = computed(() => Object.fromEntries(TYPES.map((group) => [
  group.k,
  rootExtensionRows.value.filter((row) => groupForExt(row.ext) === group.k).sort((a, b) => a.ext.localeCompare(b.ext)),
])) as Record<string, { ext: string; kind: string; count: number }[]>);

function categorySelected(key: string): boolean {
  const found = groupedExtensions.value[key] || [];
  if (!found.length) return !!typeState.value[key];
  return found.some((row) => selectedExtensions.value.includes(row.ext));
}

function toggleExtension(ext: string) {
  const selected = new Set(selectedExtensions.value);
  if (selected.has(ext)) selected.delete(ext);
  else selected.add(ext);
  selectedExtensions.value = [...selected];
  const groups = new Set(selectedExtensions.value.map(groupForExt));
  typeState.value = {
    ...typeState.value,
    images: groups.has("images"), videos: groups.has("videos"), audios: groups.has("audios"),
    pdf: groups.has("documents"), documents: groups.has("documents"),
    archives: groups.has("archives"), others: groups.has("others"),
  };
  void api.saveConfig({ scan: scanSettingsPatch() }).catch((error) => toast(`${t("保存文件类型失败：")}${error}`, "error"));
}

async function refreshRootStats() {
  try {
    const rows = await api.indexRoots();
    rootStats.value = Object.fromEntries(rows.map((row) => [rootKey(row.root), row]));
  } catch {
    rootStats.value = {};
  }
}

async function toggleRoot(root: string) {
  rootEnabled.value[root] = rootEnabled.value[root] === false;
  if (rootEnabled.value[root]) {
    setSelectedRootState(root);
  } else if (selectedRoot.value === root) {
    selectedRoot.value = roots.value.find((item) => rootEnabled.value[item] !== false) || null;
    if (selectedRoot.value) setSelectedRootState(selectedRoot.value);
  }
  try {
    await saveRoots();
  } catch (e) {
    rootEnabled.value[root] = !rootEnabled.value[root];
    toast(`${t("保存目录状态失败：")}${e}`, "error");
  }
}

function setSelectedRootState(root: string) {
  selectedRoot.value = root;
  const key = rootKey(root);
  const hasRootSelection = Object.prototype.hasOwnProperty.call(rootEnabledExtensions.value, key);
  if (hasRootSelection) {
    selectedExtensions.value = [...rootEnabledExtensions.value[key]];
  } else {
    // A root without its own extension selection has not been discovered yet.
    // Do not inherit the previous root's allow-list (for example doc/pdf/txt),
    // otherwise a new image-only root is filtered before its formats can be
    // discovered and selected. If discovery data already exists, select all
    // formats found for this root; otherwise let the scanner do a full first
    // pass and persist the per-root list afterwards.
    selectedExtensions.value = [...(rootExtensions.value[key] || [])];
    const discovered = selectedExtensions.value.length > 0;
    const groups = new Set(selectedExtensions.value.map(groupForExt));
    typeState.value = {
      images: discovered ? groups.has("images") : true,
      videos: discovered ? groups.has("videos") : true,
      audios: discovered ? groups.has("audios") : true,
      pdf: discovered ? groups.has("documents") : true,
      documents: discovered ? groups.has("documents") : true,
      archives: discovered ? groups.has("archives") : true,
      others: discovered ? groups.has("others") : true,
    };
    return;
  }
  const groups = new Set(selectedExtensions.value.map(groupForExt));
  typeState.value = {
    images: groups.has("images"), videos: groups.has("videos"), audios: groups.has("audios"),
    pdf: groups.has("documents"), documents: groups.has("documents"),
    archives: groups.has("archives"), others: groups.has("others"),
  };
}

function rootProgress(stats?: RootIndexStats) {
  return stats ? displayProgress(stats.index_progress) : 0;
}

// Keep small but real progress visible while a large index is being built.
// Integer rounding made a few completed videos/pages look stuck at 0% (for
// example 2 of 500 = 0.4%). The progress bar accepts fractional percentages,
// so expose one decimal place and only use a tiny floor for non-zero work.
function displayProgress(value: number): number {
  const clamped = Math.max(0, Math.min(100, Number(value) || 0));
  if (clamped <= 0) return 0;
  if (clamped >= 100) return 100;
  return Math.max(0.1, Math.round(clamped * 10) / 10);
}

const persistedIndexPct = computed(() => {
  const rows = Object.values(rootStats.value);
  const total = rows.reduce((sum, row) => sum + row.files, 0);
  if (!total) return 0;
  const pending = rows.reduce((sum, row) => sum + row.embed_pending, 0);
  return displayProgress(((total - pending) / total) * 100);
});

async function pickDir() {
  try {
    const p = await open({ directory: true, multiple: true, title: t("选择要扫描的目录") });
    if (!p) return;
    const arr = Array.isArray(p) ? p : [p];
    const added: string[] = [];
    for (const d of arr) {
      if (!roots.value.includes(d)) {
        roots.value.push(d);
        rootEnabled.value[d] = true;
        added.push(d);
      }
    }
    if (!added.length) return;
    setSelectedRootState(added[0]);
    await saveRoots();
    await startScan(added);
  } catch (e) {
    toast(`${t("选择目录失败：")}${e}`, "error");
  }
}

async function removeRoot(r: string) {
  if (!window.confirm(t("移除扫描目录将同步删除该目录的正文和向量数据，确定继续吗？"))) return;
  roots.value = roots.value.filter((x) => x !== r);
  delete rootEnabled.value[r];
  delete rootExtensions.value[rootKey(r)];
  delete rootEnabledExtensions.value[rootKey(r)];
  if (selectedRoot.value === r) {
    selectedRoot.value = roots.value.find((root) => rootEnabled.value[root] !== false) || null;
    if (selectedRoot.value) setSelectedRootState(selectedRoot.value);
  }
  try {
    await saveRoots();
  } catch (error) {
    toast(`${t("保存目录状态失败：")}${error}`, "error");
  }
}

function scanSettingsPatch() {
  if (selectedRoot.value) rootEnabledExtensions.value[rootKey(selectedRoot.value)] = [...selectedExtensions.value];
  return {
    roots: roots.value,
    disabled_roots: roots.value.filter((r) => rootEnabled.value[r] === false),
    images: typeState.value.images,
    videos: typeState.value.videos,
    audios: typeState.value.audios,
    pdf: typeState.value.pdf,
    documents: typeState.value.documents,
    archives: typeState.value.archives,
    others: typeState.value.others,
    enabled_exts: selectedExtensions.value,
    known_exts: knownExtensions.value,
    filter_extensions: cfg.value?.scan?.filter_extensions ?? false,
    root_extensions: rootExtensions.value,
    root_enabled_exts: rootEnabledExtensions.value,
    exclude_paths: excludedPaths.value,
  };
}

async function saveRoots() {
  await api.saveConfig({
    scan: scanSettingsPatch(),
  });
  await refreshRootStats();
}

async function startScan(targetRoot?: string | string[]) {
  if (operationBusy.value) return;
  cancelPending.value = false;
  const selected = targetRoot
    ? (Array.isArray(targetRoot) ? targetRoot : [targetRoot])
    : activeRoots.value;
  if (!selected.length) return toast(t("至少启用一个扫描目录"), "warn");
  await api.saveConfig({ scan: scanSettingsPatch() });
  try {
    defaultsAppliedForRun = false;
    progress.value = null;
    activeOperationRoots.value = [...selected];
    await api.scanStart(selected, undefined, true);
    store.scanBusy = true;
    operationActive.value = true;
    startPolling();
    toast(t("开始扫描…"), "ok");
  } catch (e) {
    toast(`${t("扫描失败：")}${e}`, "error");
  }
}

async function selectRootAndScan(root: string) {
  setSelectedRootState(root);
  if (rootEnabled.value[root] !== false) await startScan(root);
}

async function syncDiscoveredExtensions() {
  if (!progress.value?.done || progress.value.running || defaultsAppliedForRun) return;
  defaultsAppliedForRun = true;
  if (progress.value.cancelled) return;
  const oldKnownByRoot = new Map<string, Set<string>>();
  const oldSelectedByRoot = new Map<string, Set<string>>();
  for (const row of progress.value.roots || []) {
    const key = rootKey(row.root);
    oldKnownByRoot.set(key, new Set(rootExtensions.value[key] || []));
    oldSelectedByRoot.set(key, new Set(rootEnabledExtensions.value[key] || []));
    const found = row.extensions.map((item) => item.ext).filter(Boolean).sort();
    const oldKnown = oldKnownByRoot.get(key)!;
    const oldSelected = oldSelectedByRoot.get(key)!;
    rootExtensions.value[key] = found;
    rootEnabledExtensions.value[key] = found.filter((ext) => !oldKnown.has(ext) || oldSelected.has(ext));
  }
  const allKnownExtensions = new Set(Object.values(rootExtensions.value).flat());
  const nextSelected = selectedRoot.value
    ? [...(rootEnabledExtensions.value[rootKey(selectedRoot.value)] || [])]
    : [...new Set(Object.values(rootEnabledExtensions.value).flat())];
  const groups = new Set(nextSelected.map(groupForExt));
  typeState.value = {
    images: groups.has("images"), videos: groups.has("videos"), audios: groups.has("audios"),
    pdf: groups.has("documents"), documents: groups.has("documents"),
    archives: groups.has("archives"), others: groups.has("others"),
  };
  knownExtensions.value = [...allKnownExtensions].sort();
  selectedExtensions.value = nextSelected;
  if (cfg.value) {
    cfg.value.scan.known_exts = knownExtensions.value;
    cfg.value.scan.enabled_exts = selectedExtensions.value;
    cfg.value.scan.filter_extensions = true;
    cfg.value.scan.root_extensions = rootExtensions.value;
    cfg.value.scan.root_enabled_exts = rootEnabledExtensions.value;
  }
  await api.saveConfig({
    scan: {
      ...scanSettingsPatch(),
      enabled_exts: nextSelected,
      known_exts: knownExtensions.value,
      filter_extensions: true,
      root_extensions: rootExtensions.value,
      root_enabled_exts: rootEnabledExtensions.value,
    },
  });
}

async function addExcludedPath() {
  const path = excludeDraft.value.trim();
  if (!path) return;
  if (!excludedPaths.value.some((item) => rootKey(item) === rootKey(path))) excludedPaths.value.push(path);
  excludeDraft.value = "";
  await saveRoots();
}

function openExclusions() {
  modalExcludedPaths.value = [...excludedPaths.value];
  exclusionsOpen.value = true;
}

function closeExclusions() {
  exclusionsOpen.value = false;
}

function removeModalExcludedPath(path: string) {
  modalExcludedPaths.value = modalExcludedPaths.value.filter((item) => rootKey(item) !== rootKey(path));
}

async function confirmExclusions() {
  excludedPaths.value = [...modalExcludedPaths.value];
  try {
    await saveRoots();
    exclusionsOpen.value = false;
  } catch (error) {
    toast(`${t("保存排除项失败：")}${error}`, "error");
  }
}

async function removeExcludedPath(path: string) {
  excludedPaths.value = excludedPaths.value.filter((item) => rootKey(item) !== rootKey(path));
  await saveRoots();
}

async function cancelScan() {
  if (cancelPending.value) return;
  cancelledIndexPct.value = indexPct.value;
  // The backend returns after setting the cooperative stop flag. Polling
  // below observes the final checkpoint without blocking this view.
  cancelPending.value = true;
  try {
    await api.scanCancel();
    // Keep the operation alive while polling waits for the worker to reach a
    // checkpoint; do not mark the UI idle before native inference returns.
    toast(t("正在停止"), "warn");
  } catch (e) {
    cancelPending.value = false;
    toast(`${e}`, "error");
  }
}

function startPolling() {
  stopPolling();
  polling.value = window.setInterval(async () => {
    try {
      progress.value = await api.scanProgress();
      idx.value = await api.indexStats();
      await syncDiscoveredExtensions();
      if (Date.now() - lastRootRefresh > 2500) {
        lastRootRefresh = Date.now();
        await refreshRootStats();
      }
      // The worker belongs to the backend and survives route changes. Rebuild
      // the local busy state when this view is mounted again mid-operation.
      const remoteBusy = !!progress.value?.running
        || !!idx.value?.running
        || (idx.value?.queued ?? 0) > 0;
      if (remoteBusy && !operationActive.value) {
        operationActive.value = true;
        store.scanBusy = true;
      }
      if (remoteBusy && (progress.value?.cancelled || idx.value?.cancelled)) {
        cancelPending.value = true;
      }
      if (operationActive.value && (
        (progress.value?.cancelled && !progress.value.running) ||
        (idx.value?.cancelled && !progress.value?.running && !idx.value.running && idx.value.queued === 0) ||
        (progress.value?.done && !progress.value.running
          && idx.value && !idx.value.running && idx.value.queued === 0)
      )) {
        operationActive.value = false;
        cancelPending.value = false;
        store.scanBusy = false;
        await refreshStats();
        await refreshRootStats();
      }
    } catch {
      /* ignore */
    }
  }, 500);
}

function stopPolling() {
  if (polling.value !== null) {
    window.clearInterval(polling.value);
    polling.value = null;
  }
}

async function buildIndex(what: "auto" | "text" | "embed") {
  try {
    if (!activeRoots.value.length) return toast(t("至少启用一个扫描目录"), "warn");
    await api.saveConfig({ scan: scanSettingsPatch() });
    cancelPending.value = false;
    cancelledIndexPct.value = null;
    defaultsAppliedForRun = false;
    activeOperationRoots.value = [...activeRoots.value];
    await api.scanStart(activeRoots.value, what);
    operationActive.value = true;
    startPolling();
    toast(`${t("已重新扫描目录，随后继续")}${what === "auto" ? t("全文和向量") : what === "text" ? t("正文") : t("向量")}${t("索引")}`, "ok");
  } catch (e) {
    toast(`${t("索引失败：")}${e}`, "error");
  }
}

const operationBusy = computed(() => operationActive.value || store.scanBusy || !!idx.value?.running || !!idx.value?.queued);

const selectedScanStats = computed(() => liveSelectedRoot.value || selectedRootStats.value);
const scanTotalFiles = computed(() => selectedScanStats.value?.discovered_files ?? selectedScanStats.value?.files ?? 0);
const scanAddedOrUpdated = computed(() => (selectedScanStats.value?.added ?? 0) + (selectedScanStats.value?.updated ?? 0));
const scanErrors = computed(() => selectedScanStats.value?.errors ?? 0);
const scanBytes = computed(() => selectedScanStats.value?.discovered_bytes ?? selectedScanStats.value?.bytes ?? 0);

function formatBytes(bytes: number): string {
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
  if (bytes >= 1024 ** 2) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024).toFixed(1)} KB`;
}

const indexPct = computed(() => {
  if (cancelledIndexPct.value !== null) return cancelledIndexPct.value;
  const s = idx.value;
  const partialCompleted = s && s.segment_total > 0
    ? Math.min(1, Math.max(0, s.segment_completed / s.segment_total))
    : 0;
  // A video reports segment progress before its file-level SQLite state is
  // committed. Keep that fraction visible even when the selected-root
  // snapshot is one polling cycle behind (small video-only directories used
  // to remain at 0% while their first windows were already searchable).
  const liveProgress = s && s.total > 0
    ? ((s.completed + partialCompleted) / s.total) * 100
    : partialCompleted * 100;
  // IndexStats follows the active queue even when several roots are included.
  // The per-root snapshot is checkpointed less often and can stay at 0% while
  // completed video windows/files are already available to search.
  const selectedRootInOperation = !selectedRoot.value
    || activeOperationRoots.value.length === 0
    || activeOperationRoots.value.some((root) => rootKey(root) === rootKey(selectedRoot.value!));
  if (operationBusy.value && selectedRootInOperation && s && s.total > 0) {
    return displayProgress(liveProgress);
  }
  if (partialCompleted > 0) return displayProgress(liveProgress);
  if (selectedRoot.value) return rootProgress(selectedRootStats.value);
  if (s && s.total) {
    return displayProgress(liveProgress);
  }
  return persistedIndexPct.value;
});

const indexElapsedLabel = computed(() => {
  const seconds = Math.max(0, Math.floor(idx.value?.elapsed_seconds ?? 0));
  const minutes = Math.floor(seconds / 60);
  const remainder = seconds % 60;
  return `${String(minutes).padStart(2, "0")}:${String(remainder).padStart(2, "0")}`;
});

const currentItemsLabel = computed(() => {
  // IndexStats reports the active model batch. During the short scan/extract
  // hand-off only ScanProgress has a current file, so include both sources
  // instead of leaving the label blank while work is already running.
  const names = [...new Set([
    ...(idx.value?.current_items || []),
    progress.value?.current || "",
  ].map((name) => name.trim()).filter(Boolean))];
  if (names.length) return names.join(", ");
  return idx.value?.current?.trim() || "";
});

onMounted(async () => {
  await loadCfg();
  await refreshRootStats();
  startPolling();
});
onBeforeUnmount(stopPolling);
</script>

<template>
  <div class="view">
    <div class="grid">
      <section class="card blk roots-card">
        <h3>{{ t("索引目录") }}</h3>
        <div class="roots">
          <div
            v-for="r in roots"
            :key="r"
            class="root"
            :class="{ selected: selectedRoot === r, disabled: rootEnabled[r] === false }"
            @click="selectRootAndScan(r)"
          >
            <button
              class="root-toggle"
              type="button"
              role="checkbox"
              :aria-checked="rootEnabled[r] !== false"
              :title="rootEnabled[r] === false ? t('点击启用此目录') : t('点击跳过此目录')"
              @click.stop="toggleRoot(r)"
            ><Check v-if="rootEnabled[r] !== false" :size="16" :stroke-width="3" /></button>
            <span class="mono root-name">{{ r }}</span>
            <button class="mini" @click.stop="removeRoot(r)">{{ t("移除") }}</button>
          </div>
          <div v-if="!roots.length" class="none">{{ t("还没有添加目录") }}</div>
        </div>
        <div class="row">
          <button class="primary" @click="pickDir">{{ t("+ 添加目录") }}</button>
          <button class="ghost" @click="saveRoots">{{ t("保存") }}</button>
        </div>
      </section>

      <section v-if="hasSelectedRoot" class="card blk types-card">
        <h3>{{ t("文件类型") }}</h3>
        <div class="type-groups">
          <div class="type-buttons">
            <button
              v-for="item in TYPES"
              :key="item.k"
              type="button"
              class="type-tag"
              :class="{ active: categorySelected(item.k), expanded: extensionGroupOpen === item.k }"
              :aria-expanded="extensionGroupOpen === item.k"
              @click="extensionGroupOpen = extensionGroupOpen === item.k ? '' : item.k"
            >{{ t(item.label) }}</button>
          </div>
          <div v-if="extensionGroupOpen" class="extension-row">
            <label v-for="extension in groupedExtensions[extensionGroupOpen] || []" :key="extension.ext" class="extension-chip" :class="{ selected: selectedExtensions.includes(extension.ext) }">
              <input type="checkbox" :checked="selectedExtensions.includes(extension.ext)" @change="toggleExtension(extension.ext)" />
              <span>{{ extension.ext ? `.${extension.ext}` : t("无扩展名") }}</span>
            </label>
            <span v-if="!(groupedExtensions[extensionGroupOpen] || []).length" class="empty-extension">{{ t("尚未扫描到此类型") }}</span>
          </div>
        </div>
        <div class="exclude-row">
          <input v-model="excludeDraft" class="exclude-input" :placeholder="t('输入要跳过的路径或文件')" @keydown.enter.prevent="addExcludedPath" />
          <button class="exclude-confirm" :class="{ ready: excludeDraft.trim() }" @click="addExcludedPath">{{ t("确定") }}</button>
          <button class="ghost" @click="openExclusions">{{ t("列表") }} ({{ excludedPaths.length }})</button>
        </div>
      </section>
    </div>

    <section v-if="hasSelectedRoot" class="card blk grow">
      <h3>{{ t("索引进度") }}</h3>
      <div class="scan-cards">
        <div class="scan-stat"><b>{{ scanTotalFiles.toLocaleString() }}</b><span>{{ t("文件") }}</span></div>
        <div class="scan-stat"><b>{{ scanAddedOrUpdated.toLocaleString() }}</b><span>{{ t("新增 / 更新") }}</span></div>
        <div class="scan-stat" :class="{ warn: scanErrors > 0 }"><b>{{ scanErrors.toLocaleString() }}</b><span>{{ t("扫描错误") }}</span></div>
        <div class="scan-stat"><b>{{ formatBytes(scanBytes) }}</b><span>{{ t("总体积") }}</span></div>
      </div>

      <div class="prog">
        <div class="pbar"><div class="pfill index-fill" :style="{ width: indexPct + '%' }"></div></div>
        <span class="muted mono">{{ indexPct }}%</span>
      </div>
      <div class="index-stats" v-if="idx">
        <div><b>{{ idx.text_done }}</b><span>{{ t("正文已抽取") }}</span></div>
        <div><b>{{ idx.emb_done }}</b><span>{{ t("向量已完成") }}</span></div>
        <div><b>{{ idx.text_fail + idx.emb_fail }}</b><span>{{ t("失败") }}</span></div>
        <div><b>{{ indexElapsedLabel }}</b><span>{{ t("已用时间") }}</span></div>
      </div>
      <div v-if="operationBusy" class="index-current muted mono" :title="currentItemsLabel || undefined" aria-live="polite">
        <template v-if="cancelPending">{{ t("正在停止") }}</template>
        <template v-else-if="currentItemsLabel">{{ t("正在处理：") }}{{ currentItemsLabel }}</template>
        <template v-else>{{ t("正在准备处理文件…") }}</template>
      </div>
      <div class="row">
        <button class="primary" :disabled="operationBusy" @click="buildIndex('auto')">{{ t("建立索引") }}</button>
        <button class="ghost" :disabled="!operationBusy || cancelPending" @click="cancelScan">{{ cancelPending ? t("正在停止") : t("取消") }}</button>
        <button class="ghost" :disabled="operationBusy" @click="buildIndex('text')">{{ t("仅补正文") }}</button>
        <button class="ghost" :disabled="operationBusy" @click="buildIndex('embed')">{{ t("仅补向量") }}</button>
      </div>
    </section>

    <div v-if="exclusionsOpen" class="modal-backdrop" role="presentation" @click.self="closeExclusions">
      <section class="exclude-modal" role="dialog" aria-modal="true" :aria-label="t('排除项列表')">
        <button class="modal-close" type="button" :title="t('关闭')" @click="closeExclusions">×</button>
        <h3>{{ t("排除项列表") }}</h3>
        <div class="modal-items">
          <div v-for="path in modalExcludedPaths" :key="path" class="exclude-item">
            <span class="mono" :title="path">{{ path }}</span>
            <button type="button" :title="t('删除')" @click="removeModalExcludedPath(path)">×</button>
          </div>
          <div v-if="!modalExcludedPaths.length" class="empty-extension">{{ t("没有排除项") }}</div>
        </div>
        <div class="modal-actions">
          <button class="primary" type="button" @click="confirmExclusions">{{ t("确定") }}</button>
        </div>
      </section>
    </div>
  </div>
</template>

<style scoped>
.view { display: flex; flex-direction: column; height: 100%; gap: 8px; min-height: 0; overflow: auto; }
.grid { display: grid; grid-template-columns: 1fr 1fr; gap: 8px; }
.blk { padding: 14px; }
.roots-card { grid-column: 1 / -1; }
.blk.grow { flex: 1; }
h3 { margin: 0 0 10px; font-size: 13px; color: #b7c4da; font-weight: 600; }
h3.mt { margin-top: 18px; }
.roots { display: flex; flex-direction: column; gap: 6px; }
.root {
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 7px;
  padding: 6px 10px; font-size: 12px;
}
.root.selected { outline: 2px solid #111; outline-offset: -2px; }
.root { cursor: pointer; }
.root.disabled { color: #777; background: #f2f2f2; }
.root-toggle { width: 22px; height: 22px; flex: none; display: grid; place-items: center; padding: 0; border: 0; background: transparent; color: #111; font-size: 0; line-height: 0; cursor: pointer; }
.root.disabled .root-toggle { color: #888; }
.root-name { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.mini { padding: 2px 8px; font-size: 11px; }
.none { color: var(--muted); font-size: 12px; padding: 8px 0; }
.row { display: flex; gap: 8px; margin-top: 12px; flex-wrap: wrap; }
.types-card { grid-column: 1 / -1; }
.types-card .row { display: none; }
.type-groups { display: flex; flex-direction: column; gap: 6px; }
.type-buttons { display: grid; grid-template-columns: repeat(6, minmax(0, 1fr)); gap: 6px; }
.type-tag { min-width: 0; padding: 7px 5px; border: 1px solid #111; border-radius: 0; background: #e8e8e8; color: #666; font-size: 13px; cursor: pointer; white-space: nowrap; }
.type-tag.active { background: #111; color: #fff; }
.type-tag.expanded { outline: 1px solid #111; outline-offset: -3px; }
.extension-row { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; padding: 8px 2px; border-top: 1px solid #111; background: #fff; }
.extension-chip { display: inline-flex; flex-direction: row; align-items: center; gap: 4px; color: #111; background: transparent; font-size: 12px; cursor: pointer; white-space: nowrap; }
.extension-chip input { appearance: none; display: inline-grid; flex: none; width: 12px; height: 12px; margin: 0; border: 1px solid #111; border-radius: 0; background: #fff; cursor: pointer; }
.extension-chip input:checked { background: #111; }
.extension-chip input:checked::after { content: "✓"; display: block; width: auto; height: auto; border: 0; transform: none; color: #fff; text-align: center; font-size: 11px; line-height: 1; font-weight: 700; }
.empty-extension { color: #666; font-size: 11px; }
.exclude-row { position: relative; display: flex; gap: 6px; margin-top: 10px; }
.exclude-input { flex: 1; min-width: 0; padding: 7px 9px; border: 1px solid #111; border-radius: 0; background: #fff; }
.exclude-confirm { padding: 7px 14px; border: 1px solid #111; border-radius: 2px; color: #111; background: #fff; cursor: pointer; transition: color .18s, background .18s; }
.exclude-confirm.ready { color: #fff; background: #111; }
.exclude-item { display: flex; align-items: center; gap: 7px; min-height: 32px; padding: 3px 6px; border-bottom: 1px solid #ddd; }
.exclude-item .mono { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 11px; }
.exclude-item button { width: 24px; height: 24px; padding: 0; border: 1px solid #111; color: #111; background: #fff; cursor: pointer; }
.modal-backdrop { position: fixed; inset: 0; z-index: 100; display: grid; place-items: center; padding: 18px; background: rgba(0, 0, 0, .28); backdrop-filter: blur(5px); }
.exclude-modal { position: relative; width: min(620px, 100%); max-height: min(620px, 90vh); display: flex; flex-direction: column; padding: 20px; border: 1px solid #111; border-radius: 8px; background: #fff; box-shadow: 0 18px 50px rgba(0, 0, 0, .25); }
.exclude-modal h3 { margin: 0 0 12px; color: #111; }
.modal-close { position: absolute; top: 9px; right: 10px; width: auto; height: auto; padding: 0 2px; border: 0; border-radius: 0; color: #111; background: transparent; font-size: 22px; line-height: 1; cursor: pointer; }
.modal-items { min-height: 80px; overflow: auto; border-top: 1px solid #ddd; }
.modal-actions { display: flex; justify-content: flex-end; margin-top: 14px; }
.type-tag:focus-visible, .root-toggle:focus-visible { outline: 2px solid #111; outline-offset: 2px; }
.root-detail { margin-top: 12px; border-top: 1px solid #111; padding-top: 10px; }
.root-detail-head { display: flex; justify-content: space-between; gap: 10px; font-size: 12px; }
.root-detail-head span { color: #666; }
.root-detail-grid { display: grid; grid-template-columns: max-content 1fr max-content 1fr; gap: 5px 14px; margin-top: 8px; font-size: 11px; }
.root-detail-grid span { color: #666; }
.root-detail-grid b { overflow-wrap: anywhere; font-weight: 600; }
.root-error { margin: 9px 0 0; color: #8c1d18; font-size: 11px; overflow-wrap: anywhere; }
.prog { display: flex; align-items: center; gap: 10px; margin-top: 14px; }
.pbar { flex: 1; height: 8px; background: #1b2333; border-radius: 5px; overflow: hidden; }
.pfill { height: 100%; background: linear-gradient(90deg, #4f8cff, #22c55e); transition: width .3s; }
.scan-cards { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 8px; }
.scan-stat { display: flex; min-width: 0; flex-direction: column; gap: 3px; padding: 9px 10px; border: 1px solid #111; background: #fff; }
.scan-stat b { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 17px; font-weight: 600; }
.scan-stat span { color: #555; font-size: 11px; }
.scan-stat.warn b { color: #8c1d18; }
.index-stats { display: flex; gap: 28px; margin-top: 10px; }
.index-stats div { display: flex; min-width: 70px; flex-direction: column; gap: 3px; }
.index-stats b { font-size: 18px; font-weight: 600; font-variant-numeric: tabular-nums; }
.index-stats span { color: #555; font-size: 11px; }
.index-current { margin-top: 10px; color: #666; font-size: 11px; line-height: 1.45; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.tiny { font-size: 11px; }
/* Flat white/black scan surface. */
.view { color: #111; }
.blk { border: 1px solid #111; border-radius: 2px; background: #fff; }
h3 { color: #111; }
.root { border-color: #111; border-radius: 2px; background: #fff; }
.root.disabled { background: #eee; }
.pbar { background: #e9e9e9; border: 1px solid #111; border-radius: 0; }
.pfill { background: #111; }
.index-fill { background: #555; }
.pfill.scanning { animation: scan-progress 1.35s ease-in-out infinite alternate; }
@keyframes scan-progress { from { transform: translateX(0); } to { transform: translateX(185%); } }
@media (max-width: 720px) {
  .type-buttons { grid-template-columns: repeat(3, minmax(0, 1fr)); }
  .scan-cards { grid-template-columns: repeat(2, minmax(0, 1fr)); }
}
</style>
