<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
import { ArrowDownUp, Check, ChevronDown, ChevronRight, Download, FolderOpen, LayoutGrid, Network, Search, Sparkles, X } from "lucide-vue-next";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api, type Hit, type MindMapOut, type SearchQuery } from "../api";
import { langState, t } from "../i18n";
import { toast } from "../store";
import ResultGallery from "../components/ResultGallery.vue";
import GalaxyCanvas from "../galaxy/GalaxyCanvas.vue";
import MindMap from "../components/MindMap.vue";

const props = defineProps<{ thumbDir: string }>();

const text = ref("");
// Hybrid is the default: lexical matches stay instant, while semantic search
// is loaded lazily on the first query that needs it.
const semantic = ref(true);
const fulltext = ref(true);
const pinyin = ref(true);
const fuzzy = ref(true);
const visual = ref(true);
const workspaceMode = ref(true);
const debounceMs = ref(260);
const RESULT_PAGE_SIZE = 500;
const pageSize = ref(RESULT_PAGE_SIZE);
const kind = ref("all");
const sort = ref("relevance");
const desc = ref(true);
const hits = shallowRef<Hit[]>([]);
const allHits: Hit[] = [];
const hitBuckets = new Map<string, Hit[]>();
const loadedResultsRevision = ref(0);
const loading = ref(false);
const searched = ref(false);
const selected = ref<Hit | null>(null);
const similarMode = ref(false);
const similarSource = ref<Hit | null>(null);
const selectedIds = ref<Set<number>>(new Set());
const selectAllActive = ref(false);
const selectAllKind = ref("all");
const loadingMore = ref(false);
const renderLimit = ref(RESULT_PAGE_SIZE);
const typedPrompt = ref("");
const scopeOpen = ref(false);
const scopeValue = ref("all");
const scopeConfig = ref<Record<string, any>>({});
const driveRoots = ref<string[]>([]);

// ---- 视图切换（REQ-065 混合视图 / REQ-053~061 思维模型图）
type ViewMode = "list" | "galaxy" | "mind";
const viewMode = ref<ViewMode>("list");
/** 思维模型图的模型选择：八种之一。 */
const MIND_MODELS = [
  { v: "definition", label: "界定图" },
  { v: "description", label: "描述图" },
  { v: "comparison", label: "比较图" },
  { v: "classification", label: "分类图" },
  { v: "decomposition", label: "拆解图" },
  { v: "sequence", label: "顺序图" },
  { v: "causality", label: "因果图" },
  { v: "analogy", label: "类比图" },
] as const;
const mindModel = ref<string>("definition");
const mindData = ref<MindMapOut | null>(null);
const mindLoading = ref(false);
const mindError = ref("");

/** 当前结果里的文件路径（最多 600 条，够画图了）。 */
const mindPaths = computed(() => hits.value.slice(0, 600).map((h) => h.path));

async function loadMind() {
  mindLoading.value = true;
  mindError.value = "";
  try {
    mindData.value = await api.mindMapData(mindModel.value, mindPaths.value, text.value);
  } catch (e) {
    mindError.value = String(e);
    mindData.value = null;
  } finally {
    mindLoading.value = false;
  }
}

function setView(v: ViewMode) {
  viewMode.value = v;
  if (v === "mind") void loadMind();
}

watch([mindModel, () => hits.value.length], () => {
  if (viewMode.value === "mind") void loadMind();
});
type ScopeOption = { value: string; label: string; path?: string; mode: "all" | "indexed" | "path" };
const driveOptions = computed<ScopeOption[]>(() => {
  const drives = new Map<string, string>();
  for (const root of driveRoots.value) {
    const match = root.match(/^([a-z]:)(?:[\\/]|$)/i);
    if (match) drives.set(match[1].toLowerCase(), `${match[1].toUpperCase()}\\`);
  }
  return [...drives.values()].sort().map((path) => ({ value: `drive:${path}`, label: path, path, mode: "path" }));
});
const recentScopeOptions = computed<ScopeOption[]>(() => ((scopeConfig.value.recent_paths as string[] | undefined) ?? []).map((path) => ({ value: `recent:${path}`, label: path, path, mode: "path" })));
const pinnedScopeOptions = computed<ScopeOption[]>(() => ((scopeConfig.value.pinned_paths as string[] | undefined) ?? []).map((path) => ({ value: `pinned:${path}`, label: path, path, mode: "path" })));
const scopeOptions = computed<ScopeOption[]>(() => [
  { value: "all", label: t("全盘"), mode: "all" },
  { value: "indexed", label: t("仅索引"), mode: "indexed" },
  ...driveOptions.value,
  ...recentScopeOptions.value,
  ...pinnedScopeOptions.value.filter((item) => !recentScopeOptions.value.some((recent) => recent.path?.toLowerCase() === item.path?.toLowerCase())),
  ...(scopeConfig.value.initial_path && ![...recentScopeOptions.value, ...pinnedScopeOptions.value, ...driveOptions.value].some((item) => item.path?.toLowerCase() === String(scopeConfig.value.initial_path).toLowerCase())
    ? [{ value: "initial:" + scopeConfig.value.initial_path, label: String(scopeConfig.value.initial_path), path: String(scopeConfig.value.initial_path), mode: "path" as const }]
    : []),
]);
const activeScope = computed(() => scopeOptions.value.find((item) => item.value === scopeValue.value) ?? scopeOptions.value[0]);

// 界面文案在模板里用 t() 翻译，这里保留中文原文作为词条 key。
const KINDS = [
  { v: "all", label: "全部" },
  { v: "image", label: "图片" },
  { v: "video", label: "视频" },
  { v: "audio", label: "音频" },
  { v: "document", label: "文档" },
  { v: "archive", label: "压缩包" },
];
const FILTER_KINDS = [
  ...KINDS,
  { v: "other", label: "其他" },
];
const SORTS = [
  { v: "relevance", label: "相关度" },
  { v: "mtime", label: "修改时间" },
  { v: "size", label: "大小" },
  { v: "name", label: "名称" },
  { v: "kind", label: "类型" },
];

let searchTimer: number | undefined;
let typeTimer: number | undefined;
let searchGeneration = 0;
let loadingMoreGeneration = 0;
let disposed = false;
let promptIndex = 0;
let deleting = false;
// 打字机文案跟随界面语言：词条走 i18n，切换语言时重置动画。
const prompts = computed(() => [
  t("今天想要搜索什么？"),
  t("找一份文件，或描述一张图片"),
  t("用一句话找到硬盘里的内容"),
]);

function typePrompt() {
  const target = prompts.value[promptIndex] ?? "";
  if (!deleting) {
    typedPrompt.value = target.slice(0, typedPrompt.value.length + 1);
    if (typedPrompt.value === target) {
      deleting = true;
      typeTimer = window.setTimeout(typePrompt, 1900);
      return;
    }
  } else {
    typedPrompt.value = target.slice(0, Math.max(0, typedPrompt.value.length - 1));
    if (!typedPrompt.value) {
      deleting = false;
      promptIndex = (promptIndex + 1) % prompts.value.length;
    }
  }
  typeTimer = window.setTimeout(typePrompt, deleting ? 45 : 85);
}

function buildQuery(): SearchQuery {
  const scope = activeScope.value;
  return {
    text: text.value,
    // Type pills filter the fetched pages locally; changing a type does not
    // issue another model/database query.
    kinds: [],
    exts: [],
    roots: [],
    categories: [],
    limit: pageSize.value,
    sort: sort.value,
    desc: desc.value,
    use_fts: fulltext.value,
    use_pinyin: pinyin.value,
    use_fuzzy: fuzzy.value,
    use_semantic: semantic.value,
    cross_modal: visual.value,
    workspace: workspaceMode.value && scope?.mode !== "indexed",
    scope: scope?.mode ?? "all",
    scope_path: scope?.mode === "path" ? scope.path ?? null : null,
  };
}

async function persistScope(next: string, path?: string) {
  try {
    const state = await api.workspaceSetActive(path ? "path" : next, path);
    scopeConfig.value = { ...scopeConfig.value, ...state };
    scopeValue.value = path ? "recent:" + path : next;
    scopeOpen.value = false;
  } catch (error) {
    toast(`${t("保存搜索范围失败：")}${error}`, "error");
  }
  if (searched.value && text.value.trim()) schedule();
}

async function addRecentScope(path: string) {
  const normalized = path.replace(/[\\/]+$/, "") || path;
  const recent = (scopeConfig.value.recent_paths as string[] | undefined) ?? [];
  scopeConfig.value.recent_paths = [normalized, ...recent.filter((item) => item.toLowerCase() !== normalized.toLowerCase())].slice(0, 3);
  try { await api.saveConfig({ workspace: { recent_paths: scopeConfig.value.recent_paths } }); } catch {}
  await persistScope(`recent:${normalized}`, normalized);
}

async function chooseSearchPath() {
  try {
    const picked = await openDialog({ directory: true, multiple: false, title: t("选择搜索范围") });
    if (typeof picked === "string" && picked) await addRecentScope(picked);
  } catch (error) { toast(`${t("选择目录失败：")}${error}`, "error"); }
}

async function chooseScope(option: ScopeOption) {
  await persistScope(option.value, option.path);
}

function matchesKind(hit: Hit, selectedKind: string): boolean {
  if (selectedKind === "all") return true;
  if (selectedKind === "document") return hit.kind === "document" || hit.kind === "text";
  if (selectedKind === "other") return hit.kind === "other" || hit.kind === "app";
  return hit.kind === selectedKind;
}

function resetResults() {
  allHits.length = 0;
  hitBuckets.clear();
  for (const item of FILTER_KINDS) hitBuckets.set(item.v, []);
  hits.value = [];
  loadedResultsRevision.value++;
}

function syncVisibleHits() {
  hits.value = (hitBuckets.get(kind.value) ?? []).slice(0, renderLimit.value);
}

function appendResults(results: Hit[]) {
  if (!results.length) return;
  const nextSelected = selectAllActive.value ? new Set(selectedIds.value) : null;
  for (const hit of results) {
    allHits.push(hit);
    for (const item of FILTER_KINDS) {
      if (matchesKind(hit, item.v)) hitBuckets.get(item.v)?.push(hit);
    }
    if (nextSelected && matchesKind(hit, selectAllKind.value)) nextSelected.add(hit.id);
  }
  if (nextSelected && nextSelected.size !== selectedIds.value.size) selectedIds.value = nextSelected;
  loadedResultsRevision.value++;
  const activeBucket = hitBuckets.get(kind.value) ?? [];
  if (activeBucket.length > hits.value.length && hits.value.length < renderLimit.value) syncVisibleHits();
}

const displayHits = computed(() => hits.value);
const visibleSelectedCount = computed(() => displayHits.value.reduce((count, hit) => count + (selectedIds.value.has(hit.id) ? 1 : 0), 0));
const renderedHits = computed(() => displayHits.value.slice(0, renderLimit.value));
const hasMore = ref(false);

async function run(loadMore = false) {
  if (similarMode.value) return runSimilar();
  const generation = ++searchGeneration;
  loadingMoreGeneration = generation;
  loadingMore.value = false;
  hasMore.value = false;
  renderLimit.value = RESULT_PAGE_SIZE;
  if (!loadMore) {
    selectedIds.value = new Set();
    selectAllActive.value = false;
    resetResults();
  }
  searched.value = true;
  if (!loadMore) loading.value = true;
  try {
    if (!text.value.trim()) {
      resetResults();
      hasMore.value = false;
      return;
    }
    const q = buildQuery();
    q.limit = pageSize.value;
    q.offset = loadMore ? allHits.length : 0;
    const res = semantic.value ? await api.searchSemantic(q) : await api.search(q);
    if (generation !== searchGeneration || disposed) return;
    appendResults(res);
    hasMore.value = res.length === pageSize.value;
    if (!loadMore && hasMore.value) void loadRemaining(generation);
  } catch (e) {
    if (!loadMore) resetResults();
    if (generation !== searchGeneration || disposed) return;
    toast(`${t("搜索失败：")}${e}`, "error");
  } finally {
    if (!loadMore && generation === searchGeneration) loading.value = false;
  }
}

async function loadRemaining(generation: number) {
  if (loadingMore.value && loadingMoreGeneration === generation) return;
  loadingMoreGeneration = generation;
  loadingMore.value = true;
  try {
    while (hasMore.value && generation === searchGeneration && !disposed) {
      const q = buildQuery();
      q.limit = pageSize.value;
      q.offset = allHits.length;
      const res = semantic.value ? await api.searchSemantic(q) : await api.search(q);
      if (generation !== searchGeneration || disposed) return;
      if (!res.length) {
        hasMore.value = false;
        break;
      }
      appendResults(res);
      hasMore.value = res.length === pageSize.value;
      await new Promise<void>((resolve) => window.setTimeout(resolve, 0));
    }
  } catch (e) {
    if (generation === searchGeneration && !disposed) toast(`Loading more results failed: ${e}`, "error");
  } finally {
    if (loadingMoreGeneration === generation) loadingMore.value = false;
  }
}

async function runSimilar() {
  if (!similarSource.value) return;
  const generation = ++searchGeneration;
  loadingMoreGeneration = generation;
  loadingMore.value = false;
  renderLimit.value = RESULT_PAGE_SIZE;
  resetResults();
  searched.value = true;
  loading.value = true;
  try {
    appendResults(await api.similar(similarSource.value.id, undefined, RESULT_PAGE_SIZE));
    hasMore.value = false;
  } catch (e) {
    toast(`${t("相似检索失败：")}${e}`, "error");
  } finally {
    loading.value = false;
  }
}

function schedule() {
  window.clearTimeout(searchTimer);
  searchGeneration++;
  loadingMoreGeneration = searchGeneration;
  loadingMore.value = false;
  hasMore.value = false;
  loading.value = false;
  if (!text.value.trim() && !similarMode.value) {
    searched.value = false;
    resetResults();
    selected.value = null;
    selectedIds.value = new Set();
    return;
  }
  searched.value = true;
  searchTimer = window.setTimeout(run, debounceMs.value);
}

function runNow() {
  window.clearTimeout(searchTimer);
  void run();
}

function revealMoreResults() {
  renderLimit.value += RESULT_PAGE_SIZE;
  syncVisibleHits();
}

function enterSimilar(hit: Hit) {
  similarSource.value = hit;
  similarMode.value = true;
  selected.value = hit;
}

function exitSimilar() {
  similarMode.value = false;
  similarSource.value = null;
}

async function open(hit: Hit) {
  try { await api.openIndexedPath(hit.id, hit.path); } catch (e) { toast(`${t("打开失败：")}${e}`, "error"); }
}

async function reveal(hit: Hit) {
  try { await api.revealPath(hit.path); } catch (e) { toast(`${t("定位失败：")}${e}`, "error"); }
}

async function rename(hit: Hit) {
  const current = hit.name;
  const name = window.prompt(t("输入新的文件名"), current);
  if (!name || name === current) return;
  try {
    await api.renamePath(hit.path, name);
    toast(t("文件已重命名，请重新扫描目录以更新索引"), "ok");
    await run();
  } catch (e) {
    toast(`${t("重命名失败：")}${e}`, "error");
  }
}

function toggleSelected(id: number) {
  const next = new Set(selectedIds.value);
  if (next.has(id)) next.delete(id); else next.add(id);
  selectedIds.value = next;
}

function toggleAll() {
  const visibleIds = new Set((hitBuckets.get(kind.value) ?? []).map((hit) => hit.id));
  const allVisibleSelected = displayHits.value.length > 0 && visibleSelectedCount.value === displayHits.value.length;
  const next = new Set(selectedIds.value);
  for (const id of visibleIds) {
    if (allVisibleSelected) next.delete(id); else next.add(id);
  }
  selectAllActive.value = !allVisibleSelected;
  selectAllKind.value = kind.value;
  selectedIds.value = next;
}

async function doExport() {
  while (loadingMore.value) await new Promise<void>((resolve) => window.setTimeout(resolve, 25));
  const paths = allHits.filter((hit) => matchesKind(hit, kind.value) && selectedIds.value.has(hit.id)).map((hit) => hit.path);
  if (!paths.length) return toast(t("请先勾选文件"), "warn");
  try {
    const out = `${await api.dataDir()}\\export-${Date.now()}.csv`;
    const count = await api.opExport(paths, out, "csv");
    toast(`${t("已导出")} ${count} ${t("条到")} ${out}`, "ok");
  } catch (e) {
    toast(`${t("导出失败：")}${e}`, "error");
  }
}

watch([text, semantic, fulltext, pinyin, fuzzy, visual, workspaceMode, sort, desc], schedule);
watch(kind, () => { renderLimit.value = RESULT_PAGE_SIZE; syncVisibleHits(); });
watch(similarMode, (value) => { if (value) runSimilar(); else if (searched.value) run(); });

onMounted(async () => {
  typePrompt();
  try {
    const [config, drives, workspace] = await Promise.all([api.getConfig(), api.availableDrives(), api.workspaceState()]);
    scopeConfig.value = { ...(config.workspace ?? {}), ...workspace };
    driveRoots.value = drives;
    const launchDefault = scopeConfig.value.startup_scope ?? "remember";
    const savedScope = scopeConfig.value.last_scope === "path"
      ? String(scopeConfig.value.last_path ?? "")
      : String(scopeConfig.value.last_scope ?? "indexed");
    const remembered = savedScope === "all" || savedScope === "indexed"
      ? savedScope
      : savedScope ? `recent:${savedScope}` : "indexed";
    if (launchDefault === "remember") scopeValue.value = remembered;
    else if (launchDefault === "fixed" && scopeConfig.value.startup_path) scopeValue.value = `pinned:${scopeConfig.value.startup_path}`;
    else scopeValue.value = launchDefault;
    if (!scopeOptions.value.some((item) => item.value === scopeValue.value)) scopeValue.value = "indexed";
    const initialScope = String(workspace.initial_scope ?? "indexed");
    scopeValue.value = initialScope === "path"
      ? scopeOptions.value.find((item) => item.path?.toLowerCase() === String(workspace.initial_path).toLowerCase())?.value ?? "indexed"
      : initialScope;
    semantic.value = config.search.use_semantic !== false;
    fulltext.value = config.search.use_fulltext !== false;
    pinyin.value = config.search.use_pinyin !== false;
    fuzzy.value = config.search.use_fuzzy !== false;
    visual.value = config.model.clip_enabled !== false || config.model.clap_enabled !== false;
    // 默认搜索模式（通用配置）：fulltext / semantic / hybrid
    const mode = config.ui?.default_search_mode;
    if (mode === "fulltext") { semantic.value = false; fulltext.value = true; }
    else if (mode === "semantic") { semantic.value = true; fulltext.value = false; }
    else if (mode === "hybrid") { semantic.value = true; fulltext.value = true; }
    workspaceMode.value = Boolean(config.workspace?.enabled);
    // Migrate the old 1,000-result default. The backend scans up to 50,000
    // candidates, so normal searches no longer stop at the old ceiling.
    pageSize.value = RESULT_PAGE_SIZE;
    debounceMs.value = Math.min(2000, Math.max(0, Number(config.search.debounce_ms ?? 200)));
  } catch {
    scopeValue.value = "indexed";
  }
});

// 切换界面语言时重启打字机动画，让首屏文案立刻变成对应语言。
watch(
  () => langState.current,
  () => {
    if (searched.value) return;
    window.clearTimeout(typeTimer);
    promptIndex = 0;
    deleting = false;
    typedPrompt.value = "";
    typePrompt();
  }
);

onBeforeUnmount(() => {
  disposed = true;
  searchGeneration++;
  window.clearTimeout(searchTimer);
  window.clearTimeout(typeTimer);
});

const resultLabel = computed(() => {
  void loadedResultsRevision.value;
  if (similarMode.value) {
    return `${t("相似于")} ${similarSource.value?.name || t("当前文件")}`;
  }
  return `${(hitBuckets.get(kind.value)?.length ?? 0).toLocaleString()} ${t("个结果")}`;
});

defineExpose({ run, enterSimilar });
</script>

<template>
  <div class="search-page" :class="{ searched }">
    <div v-if="!searched" class="triangle-field" aria-hidden="true">
      <span class="triangle triangle-a"></span>
      <span class="triangle triangle-b"></span>
      <span class="triangle triangle-c"></span>
      <span class="triangle triangle-d"></span>
      <span class="triangle triangle-e"></span>
    </div>
    <section class="hero-search">
      <div v-if="!searched" class="hero-copy">
        <div class="brand-name">DeepDark_Oracle</div>
        <h1>{{ typedPrompt }}<span class="caret" aria-hidden="true"></span></h1>
      </div>
      <div class="query-wrap">
        <div class="scope-wrap">
          <button class="scope-select" type="button" :aria-expanded="scopeOpen" @click="scopeOpen = !scopeOpen">
            <span class="scope-caption">{{ t("工作区") }}</span>
            <span class="scope-path" :title="activeScope?.label">{{ activeScope?.label }}</span>
            <ChevronDown :size="16" aria-hidden="true" />
          </button>
          <div v-if="scopeOpen" class="scope-menu" @click.stop>
            <button v-for="option in scopeOptions" :key="option.value" type="button" class="scope-option" :class="{ selected: scopeValue === option.value }" @click="chooseScope(option)">
              <span>{{ option.label }}</span><Check v-if="scopeValue === option.value" :size="15" />
            </button>
            <button type="button" class="scope-add" @click="chooseSearchPath"><FolderOpen :size="15" />{{ t("选择路径…") }}</button>
          </div>
        </div>
        <div class="query-shell" :class="{ focused: searched }">
          <Search :size="20" class="query-icon" aria-hidden="true" />
          <input
            v-model="text"
            autofocus
            :disabled="similarMode"
            :placeholder="similarMode ? t('相似检索中：按内容距离排序') : t('搜索文件名、正文、拼音或一句话描述…')"
            @keyup.enter="runNow"
          />
          <button v-if="text" class="query-action clear-action" :title="t('清空')" :aria-label="t('清空')" @click="text = ''">
            <X :size="17" />
          </button>
          <button class="query-action run-action" :title="t('搜索')" :aria-label="t('搜索')" @click="runNow">
            <ChevronRight :size="20" />
          </button>
        </div>
      </div>
    </section>

    <template v-if="searched">
      <div class="toolbar">
        <div class="filter-group">
          <button
            v-for="item in FILTER_KINDS"
            :key="item.v"
            class="filter-pill"
            :class="{ active: kind === item.v }"
            @click="kind = item.v"
          >{{ t(item.label) }}</button>
        </div>
        <div class="toolbar-spacer"></div>
        <button class="tool-button" :class="{ active: fulltext }" :title="t('全文检索')" @click="fulltext = !fulltext">{{ t("全文") }}</button>
        <button class="tool-button" :class="{ active: pinyin }" :title="t('拼音检索')" @click="pinyin = !pinyin">{{ t("拼音") }}</button>
        <button class="tool-button" :class="{ active: semantic }" :title="t('语义检索')" @click="semantic = !semantic">{{ t("语义") }}</button>
        <button class="tool-button" :class="{ active: fuzzy }" :title="t('模糊匹配')" @click="fuzzy = !fuzzy">{{ t("模糊") }}</button>
        <button class="tool-button" :class="{ active: visual }" :title="t('跨模态检索')" @click="visual = !visual">{{ t("找图声") }}</button>
        <button class="tool-button" :class="{ active: workspaceMode }" :title="t('同时检索工作区目录中尚未入库的文件')" @click="workspaceMode = !workspaceMode">{{ t("工作区") }}</button>
        <select v-model="sort" class="sort-select" :disabled="similarMode" :aria-label="t('排序方式')">
          <option v-for="item in SORTS" :key="item.v" :value="item.v">{{ t(item.label) }}</option>
        </select>
        <button class="icon-button compact" :title="desc ? t('降序') : t('升序')" :aria-label="desc ? t('降序') : t('升序')" @click="desc = !desc">
          <ArrowDownUp :size="16" />
        </button>
        <button v-if="similarMode" class="tool-button" @click="exitSimilar">{{ t("退出相似") }}</button>
      </div>

      <div class="result-meta">
        <span class="meta-primary">{{ resultLabel }}</span>
        <span v-if="selectedIds.size" class="muted">{{ t("已选") }} {{ selectedIds.size }}</span>
        <span class="toolbar-spacer"></span>
        <div class="view-switch" role="tablist" :aria-label="t('视图')">
          <button
            class="vs-btn"
            :class="{ active: viewMode === 'list' }"
            :title="t('列表视图')"
            @click="setView('list')"
          ><LayoutGrid :size="13" />{{ t("列表") }}</button>
          <button
            class="vs-btn"
            :class="{ active: viewMode === 'galaxy' }"
            :title="t('星系图谱视图')"
            @click="setView('galaxy')"
          ><Network :size="13" />{{ t("图谱") }}</button>
          <button
            class="vs-btn"
            :class="{ active: viewMode === 'mind' }"
            :title="t('思维模型图')"
            @click="setView('mind')"
          ><Sparkles :size="13" />{{ t("思维图") }}</button>
        </div>
        <select
          v-if="viewMode === 'mind'"
          v-model="mindModel"
          class="sort-select"
          :aria-label="t('思维模型')"
        >
          <option v-for="m in MIND_MODELS" :key="m.v" :value="m.v">{{ t(m.label) }}</option>
        </select>
          <button class="meta-button" @click="toggleAll">
          <Check :size="14" /> {{ visibleSelectedCount === displayHits.length && displayHits.length ? t("取消全选") : t("全选") }}
        </button>
        <button class="meta-button" @click="doExport">
          <Download :size="14" /> {{ t("导出") }}
        </button>
      </div>

      <div class="results-layout" :class="{ 'graph-mode': viewMode !== 'list' }">
        <section v-if="viewMode === 'list'" class="gallery-panel">
          <div v-if="similarMode" class="similar-note">{{ t("相似于") }}：{{ similarSource?.name }}</div>
          <ResultGallery
            :hits="renderedHits"
            :selected-id="selected?.id ?? null"
            :selected-ids="selectedIds"
            :thumb-dir="props.thumbDir"
            :loading="loading && hits.length === 0"
            :query="text"
            @select="selected = $event"
            @open="open"
            @toggle="toggleSelected"
            @reveal="reveal"
            @rename="rename"
            @reach-end="revealMoreResults"
          />
        </section>

        <section v-else-if="viewMode === 'galaxy'" class="graph-panel">
          <GalaxyCanvas
            :depth="3"
            :file-limit="60"
            :max-nodes="1400"
            @open-path="(p) => api.openPath(p).catch((e) => toast(String(e), 'error'))"
          />
        </section>

        <section v-else class="graph-panel">
          <div v-if="mindLoading" class="graph-mask">{{ t("正在整理思维模型图…") }}</div>
          <div v-else-if="mindError" class="graph-mask">{{ mindError }}</div>
          <MindMap
            v-else-if="mindData"
            :model="mindData.model"
            :groups="mindData.groups"
            :title="mindData.title"
            :subtitle="mindData.subtitle"
            @open="(p) => api.openPath(p).catch((e) => toast(String(e), 'error'))"
            @reveal="(p) => api.revealPath(p).catch((e) => toast(String(e), 'error'))"
          />
        </section>
      </div>
    </template>
  </div>
</template>

<style scoped>
.search-page { position: relative; height: 100%; display: flex; flex-direction: column; min-height: 0; overflow: hidden; background: var(--bg); }
.triangle-field { position: absolute; z-index: 0; inset: 0 0 50%; overflow: hidden; pointer-events: none; perspective: 900px; }
.triangle-field::after { content: ""; position: absolute; inset: 0; background: linear-gradient(to bottom, transparent 28%, var(--bg) 100%); }
.triangle { position: absolute; display: block; width: 0; height: 0; border-left: 90px solid transparent; border-right: 90px solid transparent; border-bottom: 170px solid rgba(17, 17, 17, .055); transform-style: preserve-3d; filter: drop-shadow(0 12px 14px rgba(0, 0, 0, .06)); animation: triangle-rise var(--duration) linear var(--delay) infinite; }
.triangle-a { left: 10%; top: 58%; --duration: 10s; --delay: -2s; --depth: 45px; --rx-start: 24deg; --rx-end: 110deg; --ry-start: -35deg; --ry-end: 30deg; --rz-start: -18deg; --rz-end: 22deg; }
.triangle-b { left: 29%; top: 67%; border-left-width: 130px; border-right-width: 130px; border-bottom-width: 220px; border-bottom-color: rgba(17, 17, 17, .035); --duration: 13s; --delay: -9s; --depth: -90px; --rx-start: -22deg; --rx-end: 70deg; --ry-start: 35deg; --ry-end: -60deg; --rz-start: 22deg; --rz-end: -28deg; }
.triangle-c { right: 24%; top: 54%; border-left-width: 100px; border-right-width: 100px; border-bottom-width: 185px; border-bottom-color: rgba(17, 17, 17, .045); --duration: 11s; --delay: -5s; --depth: 110px; --rx-start: 30deg; --rx-end: -65deg; --ry-start: -22deg; --ry-end: 58deg; --rz-start: 11deg; --rz-end: -20deg; }
.triangle-d { right: 7%; top: 72%; border-left-width: 150px; border-right-width: 150px; border-bottom-width: 250px; border-bottom-color: rgba(17, 17, 17, .03); --duration: 15s; --delay: -12s; --depth: -45px; --rx-start: -28deg; --rx-end: 55deg; --ry-start: -40deg; --ry-end: 72deg; --rz-start: -27deg; --rz-end: 14deg; }
.triangle-e { left: 51%; top: 83%; border-left-width: 70px; border-right-width: 70px; border-bottom-width: 130px; border-bottom-color: rgba(17, 17, 17, .04); --duration: 9s; --delay: -6s; --depth: 20px; --rx-start: 15deg; --rx-end: -80deg; --ry-start: 48deg; --ry-end: -34deg; --rz-start: 4deg; --rz-end: 35deg; }
@keyframes triangle-rise {
  0% { opacity: 0; transform: translate3d(0, 18vh, var(--depth)) rotateX(var(--rx-start)) rotateY(var(--ry-start)) rotateZ(var(--rz-start)) scale(.72); }
  18% { opacity: .13; }
  57% { opacity: .09; }
  78%, 100% { opacity: 0; transform: translate3d(0, -22vh, var(--depth)) rotateX(var(--rx-end)) rotateY(var(--ry-end)) rotateZ(var(--rz-end)) scale(1.12); }
}
@media (prefers-reduced-motion: reduce) { .triangle { animation-duration: 1ms; animation-iteration-count: 1; } .toolbar, .result-meta { animation: none; } .query-shell, .hero-search { transition: none; } }
.hero-search, .toolbar, .result-meta, .results-layout { position: relative; z-index: 1; }
.toolbar, .result-meta { animation: fade-down .22s ease both; }
@keyframes fade-down { from { opacity: 0; transform: translateY(-4px); } to { opacity: 1; transform: translateY(0); } }
.hero-search { flex: 1; min-height: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 34px; padding: 24px; transition: padding .26s ease, gap .26s ease; }
.searched .hero-search { flex: none; align-items: stretch; gap: 0; padding: 26px clamp(16px, 8vw, 140px) 15px; }
.hero-copy { text-align: center; }
.hero-kicker { display: none; }
.brand-name { margin-bottom: 18px; color: #111; font: 600 16px/1 Consolas, "Segoe UI", sans-serif; letter-spacing: 0; }
h1 { min-height: 1.4em; margin: 0; color: #111; font-size: 42px; font-weight: 450; letter-spacing: 0; }
.caret { display: inline-block; width: 2px; height: .9em; margin-left: 4px; vertical-align: -.08em; background: var(--accent); animation: blink 1s steps(1) infinite; }
@keyframes blink { 50% { opacity: 0; } }
.query-wrap { position: relative; width: min(680px, 100%); display: flex; flex-direction: column; gap: 8px; }
.searched .query-wrap { width: 100%; }
.scope-wrap { position: relative; z-index: 4; width: 100%; }
.scope-select { width: 100%; height: 32px; display: flex; align-items: center; gap: 12px; padding: 0 18px; border: 1px solid rgba(17,17,17,.14); border-radius: 18px 18px 0 0; background: linear-gradient(180deg, rgba(17,17,17,.075), rgba(255,255,255,.72)); color: #111; text-align: left; }
.scope-caption { flex: none; color: #555; font-size: 11px; }
.scope-path { flex: 1; min-width: 0; overflow: hidden; color: #111; font-size: 12px; text-overflow: ellipsis; white-space: nowrap; }
.scope-select :deep(svg) { flex: none; }
.scope-menu { position: absolute; top: 100%; left: 0; right: 0; z-index: 20; max-height: 320px; overflow: auto; padding: 5px; border: 1px solid #111; border-radius: 0 0 4px 4px; background: #fff; box-shadow: 0 12px 28px rgba(0,0,0,.16); }
.scope-option, .scope-add { width: 100%; min-height: 34px; display: flex; align-items: center; justify-content: space-between; gap: 10px; padding: 7px 10px; border: 0; background: transparent; color: #111; text-align: left; }
.scope-option span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.scope-option:hover, .scope-option.selected, .scope-add:hover { background: #f1f1f1; }
.scope-add { justify-content: flex-start; margin-top: 4px; border-top: 1px solid #ddd; }
.query-shell { width: 100%; height: 62px; display: flex; align-items: center; gap: 12px; padding: 0 12px 0 22px; border: 1px solid #111; border-radius: 32px; background: #fff; box-shadow: 0 12px 30px rgba(0,0,0,.08); transition: height .24s cubic-bezier(.2,.7,.3,1), border-radius .24s cubic-bezier(.2,.7,.3,1), padding .24s ease, border-color .15s, box-shadow .15s; }
.query-shell.focused { width: 100%; height: 50px; border-radius: 25px; background: #fff; box-shadow: none; }
.query-shell:focus-within { border-color: #111; box-shadow: 0 0 0 3px rgba(17, 17, 17, .08); }
.query-icon { flex: none; color: #111; }
.query-shell input { flex: 1; min-width: 0; height: 100%; padding: 0; border: 0; outline: 0; background: transparent; color: var(--text); font-size: 15px; }
.query-shell input::placeholder { color: #777; }
.query-action { width: 36px; height: 36px; display: grid; place-items: center; padding: 0; border: 0; border-radius: 50%; }
.query-action:hover { background: #f0f0f0; }
.clear-action { color: #666; }
.run-action { color: #fff; background: #111; }
.run-action:hover { background: #333; }
.toolbar { display: flex; align-items: center; gap: 7px; flex-wrap: wrap; padding: 0 clamp(16px, 8vw, 140px) 11px; }
.filter-group { display: flex; gap: 5px; overflow-x: auto; }
.filter-pill, .tool-button, .meta-button { padding: 5px 10px; border: 1px solid transparent; border-radius: 2px; background: transparent; color: #555; font-size: 11px; white-space: nowrap; transition: background .14s ease, color .14s ease, transform .1s ease; }
.filter-pill:active, .tool-button:active, .meta-button:active { transform: scale(.95); }
.filter-pill:hover, .tool-button:hover, .meta-button:hover { background: #f0f0f0; color: #111; }
.filter-pill.active, .tool-button.active { color: #fff; background: #111; border-color: #111; }
.toolbar-spacer { flex: 1; }
.sort-select { height: 29px; padding: 0 8px; border-radius: 5px; font-size: 11px; }
.compact { width: 30px; height: 29px; display: grid; place-items: center; padding: 0; border-radius: 2px; }
.result-meta { display: flex; align-items: center; gap: 10px; min-height: 34px; padding: 0 clamp(16px, 8vw, 140px) 10px; color: #555; font-size: 11px; }
.view-switch { display: inline-flex; border: 1px solid #111; border-radius: 2px; overflow: hidden; }
.vs-btn {
  display: inline-flex; align-items: center; gap: 4px;
  background: #fff; color: #111; border: 0;
  font-size: 11px; padding: 4px 9px; cursor: pointer;
  border-right: 1px solid #111;
}
.vs-btn:last-child { border-right: 0; }
.vs-btn:hover { background: #f1f1f1; }
.vs-btn.active { background: #111; color: #fff; }
.results-layout.graph-mode { padding-bottom: 12px; }
.graph-panel { position: relative; flex: 1; min-height: 320px; height: 100%; border: 1px solid #111; border-radius: 2px; overflow: hidden; }
.graph-mask {
  position: absolute; inset: 0; display: grid; place-items: center;
  font-size: 12px; color: #555; background: #0a0e1a;
}
.meta-primary { color: #111; }
.meta-button { display: inline-flex; align-items: center; gap: 5px; padding-inline: 7px; }
.results-layout { flex: 1; min-height: 0; display: grid; grid-template-columns: minmax(0, 1fr); gap: 12px; padding: 0 clamp(16px, 8vw, 140px) 18px; }
.gallery-panel { min-width: 0; min-height: 0; overflow: hidden; border-top: 1px solid #111; padding-top: 10px; }
.similar-note { margin: 0 0 8px; color: #444; font-size: 11px; }
@media (max-width: 860px) { .results-layout { padding-inline: 16px; } .searched .hero-search { padding-inline: 16px; } .toolbar, .result-meta { padding-inline: 16px; } }
@media (max-width: 640px) { h1 { font-size: 28px; }.results-layout { overflow: auto; } .toolbar { overflow-x: auto; flex-wrap: nowrap; } .filter-group { flex: none; } }
</style>
