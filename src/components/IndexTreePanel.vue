<script setup lang="ts">
// 右上角「索引目录结构」面板：从顶栏的索引统计按钮向下展开。
// 数据全部来自索引库（tree_browse / tree_search），不触发任何磁盘扫描；
// 面板收起时不销毁已加载的层级，再次展开直接复用缓存。
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from "vue";
import {
  ChevronRight,
  CircleAlert,
  ExternalLink,
  File,
  FileArchive,
  FileBox,
  FileCode,
  FileImage,
  FileText,
  Film,
  Folder,
  FolderOpen,
  FolderTree,
  LoaderCircle,
  Music,
  RefreshCw,
  Search,
  X,
} from "lucide-vue-next";
import { api, type TreeEntry, type TreeLevel } from "../api";
import { humanSize, toast } from "../store";
import { t } from "../i18n";
import GalaxyCanvas from "../galaxy/GalaxyCanvas.vue";

const props = defineProps<{ open: boolean }>();
const emit = defineEmits<{ (e: "close"): void; (e: "open-full"): void }>();

/** REQ-027/028：面板内「目录树 / 星系图」双 Tab。 */
const tab = ref<"tree" | "graph">("tree");
/** 星系图 Tab 的根目录（跟随面板里选中的目录）。 */
const graphRoot = computed(() => {
  const sel = selected.value;
  if (!sel) return "";
  return sel.endsWith("\\") ? sel : parentOf(sel);
});

const panelRef = ref<HTMLElement | null>(null);
const bodyRef = ref<HTMLElement | null>(null);

/** 已加载的层级：key 为归一化路径，根层是空串。 */
const levels = reactive<Record<string, TreeLevel>>({});
const expanded = reactive<Set<string>>(new Set());
const loading = reactive<Set<string>>(new Set());
const errors = reactive<Record<string, string>>({});
const selected = ref("");
/** 底栏统计跟随的目录（选中文件时停留在父目录）。 */
const focus = ref("");
const refreshing = ref(false);

const query = ref("");
const searching = ref(false);
const results = ref<TreeEntry[]>([]);

const menu = reactive<{ open: boolean; x: number; y: number; entry: TreeEntry | null }>({
  open: false,
  x: 0,
  y: 0,
  entry: null,
});

const CODE_EXT = new Set([
  "rs", "js", "mjs", "cjs", "ts", "tsx", "jsx", "vue", "py", "c", "h", "cc", "cpp", "hpp",
  "cs", "java", "go", "rb", "php", "sh", "ps1", "bat", "cmd", "json", "toml", "yaml", "yml",
  "xml", "html", "css", "scss", "less", "sql", "lua", "kt", "swift", "gradle", "cmake",
]);

/** 路径归一化：去掉尾部反斜杠并转小写，作为缓存键。 */
function key(path: string): string {
  const p = (path || "").replace(/\\+$/, "");
  return p.toLowerCase();
}

/** 取父目录；盘符根（D:\）的父目录是空串。 */
function parentOf(path: string): string {
  const p = (path || "").replace(/\\+$/, "");
  const i = p.lastIndexOf("\\");
  if (i < 0) return "";
  const head = p.slice(0, i);
  if (!head) return "";
  if (head.length === 2 && head[1] === ":") return head + "\\";
  return head;
}

async function ensureLevel(path: string, force = false): Promise<TreeLevel | null> {
  const k = key(path);
  if (!force && levels[k]) return levels[k];
  if (loading.has(k)) return null;
  loading.add(k);
  try {
    const lv = await api.treeBrowse(path || undefined);
    levels[k] = lv;
    delete errors[k];
    return lv;
  } catch (e) {
    errors[k] = String(e);
    return null;
  } finally {
    loading.delete(k);
  }
}

async function toggle(entry: TreeEntry) {
  if (entry.is_file) {
    selected.value = entry.path;
    focus.value = parentOf(entry.path);
    return;
  }
  selected.value = entry.path;
  focus.value = entry.path;
  const k = key(entry.path);
  if (expanded.has(k)) {
    expanded.delete(k);
    return;
  }
  expanded.add(k);
  if (!levels[k]) await ensureLevel(entry.path);
}

type Row =
  | { kind: "node"; entry: TreeEntry; depth: number }
  | { kind: "more"; depth: number; count: number };

const rows = computed<Row[]>(() => {
  const out: Row[] = [];
  const walk = (path: string, depth: number) => {
    const lv = levels[key(path)];
    if (!lv) return;
    for (const entry of lv.entries) {
      out.push({ kind: "node", entry, depth });
      if (!entry.is_file && expanded.has(key(entry.path))) walk(entry.path, depth + 1);
    }
    if (lv.hidden_files > 0) out.push({ kind: "more", depth, count: lv.hidden_files });
  };
  walk("", 0);
  return out;
});

function highlight(name: string, q: string): [string, string, string] {
  const s = q.trim();
  if (!s) return [name, "", ""];
  const i = name.toLowerCase().indexOf(s.toLowerCase());
  if (i < 0) return [name, "", ""];
  return [name.slice(0, i), name.slice(i, i + s.length), name.slice(i + s.length)];
}

const resultRows = computed(() =>
  results.value.map((entry) => {
    const [pre, hit, post] = highlight(entry.name, query.value);
    return { entry, pre, hit, post };
  }),
);

const footerText = computed(() => {
  const roots = levels[""];
  if (query.value.trim()) {
    return `${t("匹配到")} ${results.value.length.toLocaleString()} ${t("个文件夹")}`;
  }
  if (!focus.value) {
    if (!roots) return t("正在读取索引结构…");
    return `${roots.entries.length} ${t("个索引根")} · ${roots.total_files.toLocaleString()} ${t("个文件")} · ${humanSize(roots.total_bytes)}`;
  }
  const lv = levels[key(focus.value)];
  if (!lv) return t("正在读取索引结构…");
  return `${t("当前目录包含")} ${lv.dirs.toLocaleString()} ${t("个子文件夹")}，${lv.files.toLocaleString()} ${t("个文件")} · ${humanSize(lv.total_bytes)}`;
});

function iconFor(entry: TreeEntry) {
  if (!entry.is_file) return expanded.has(key(entry.path)) ? FolderOpen : Folder;
  if (entry.ext && CODE_EXT.has(entry.ext.toLowerCase())) return FileCode;
  switch (entry.kind) {
    case "image":
      return FileImage;
    case "video":
      return Film;
    case "audio":
      return Music;
    case "archive":
      return FileArchive;
    case "document":
    case "text":
      return FileText;
    case "app":
      return FileBox;
    default:
      return File;
  }
}

function dotClass(entry: TreeEntry): string {
  if (entry.error) return "err";
  // 目录的 indexed/pending 只覆盖直属文件，用它代表整棵树会误导，
  // 所以状态点只标在文件行上（docx 也只要求「文件名右侧」）。
  if (!entry.is_file) return "none";
  return entry.indexed > 0 ? "ok" : "idle";
}

function rowTitle(entry: TreeEntry): string {
  if (entry.error) return `${entry.path}\n${entry.error}`;
  if (entry.is_file) return `${entry.path}\n${humanSize(entry.size)}`;
  return `${entry.path}\n${entry.total_files.toLocaleString()} ${t("个文件")} · ${humanSize(entry.total_bytes)}`;
}

async function refresh() {
  if (refreshing.value) return;
  refreshing.value = true;
  try {
    for (const k of Object.keys(levels)) delete levels[k];
    for (const k of Object.keys(errors)) delete errors[k];
    expanded.clear();
    selected.value = "";
    focus.value = "";
    await ensureLevel("");
    toast(t("索引结构已刷新"), "ok", 2200);
  } finally {
    refreshing.value = false;
  }
}

/** 从搜索结果跳回树：逐级展开祖先目录并滚动到目标。 */
async function reveal(entry: TreeEntry) {
  const chain: string[] = [];
  let cur = entry.is_file ? parentOf(entry.path) : entry.path;
  while (cur) {
    chain.unshift(cur);
    cur = parentOf(cur);
  }
  await ensureLevel("");
  for (const p of chain) {
    const lv = await ensureLevel(p);
    if (!lv) break;
    expanded.add(key(p));
  }
  query.value = "";
  results.value = [];
  selected.value = entry.path;
  focus.value = entry.is_file ? parentOf(entry.path) : entry.path;
  await nextTick();
  scrollTo(entry.path);
}

function scrollTo(path: string) {
  const i = rows.value.findIndex((r) => r.kind === "node" && r.entry.path === path);
  if (i < 0) return;
  const el = bodyRef.value?.querySelector<HTMLElement>(`[data-row="${i}"]`);
  el?.scrollIntoView({ block: "center" });
}

async function openCurrent() {
  const p = focus.value || "";
  if (!p) {
    toast(t("先选中一个目录"), "warn", 2400);
    return;
  }
  try {
    await api.openPath(p);
  } catch (e) {
    toast(`${t("打开失败：")}${e}`, "error");
  }
}

// ---------------------------------------------------------------- 搜索

let searchTimer: number | undefined;

watch(query, (raw) => {
  if (searchTimer) window.clearTimeout(searchTimer);
  const q = raw.trim();
  if (!q) {
    results.value = [];
    searching.value = false;
    return;
  }
  searching.value = true;
  searchTimer = window.setTimeout(async () => {
    try {
      results.value = await api.treeSearch(q, 200);
    } catch {
      results.value = [];
    } finally {
      searching.value = false;
    }
  }, 220);
});

// ---------------------------------------------------------------- 右键菜单

function openMenu(ev: MouseEvent, entry: TreeEntry) {
  ev.preventDefault();
  ev.stopPropagation();
  menu.entry = entry;
  menu.x = Math.min(ev.clientX, Math.max(8, window.innerWidth - 196));
  menu.y = Math.min(ev.clientY, Math.max(8, window.innerHeight - 168));
  menu.open = true;
}

function closeMenu() {
  menu.open = false;
}

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      document.body.removeChild(ta);
      return ok;
    } catch {
      return false;
    }
  }
}

async function runMenu(action: "reveal" | "copy-path" | "copy-name" | "workspace") {
  const entry = menu.entry;
  menu.open = false;
  if (!entry) return;
  try {
    if (action === "reveal") {
      if (entry.is_file) await api.revealPath(entry.path);
      else await api.openPath(entry.path);
      return;
    }
    if (action === "copy-path") {
      const ok = await copyText(entry.path);
      toast(ok ? t("已复制完整路径") : t("复制失败"), ok ? "ok" : "error", 2200);
      return;
    }
    if (action === "copy-name") {
      const ok = await copyText(entry.name);
      toast(ok ? t("已复制文件名") : t("复制失败"), ok ? "ok" : "error", 2200);
      return;
    }
    const dir = entry.is_file ? parentOf(entry.path) : entry.path;
    if (!dir) {
      toast(t("根目录不能加入工作区"), "warn", 2600);
      return;
    }
    await api.workspaceAdd(dir);
    toast(`${t("已加入工作区：")}${dir}`, "ok", 3200);
  } catch (e) {
    toast(`${t("操作失败：")}${e}`, "error");
  }
}

// ---------------------------------------------------------------- 打开 / 关闭

watch(
  () => props.open,
  (open) => {
    if (open) {
      if (!levels[""]) void ensureLevel("");
    } else {
      menu.open = false;
      query.value = "";
      results.value = [];
    }
  },
  { immediate: true },
);

function onDocPointerDown(ev: MouseEvent) {
  if (!props.open) return;
  const el = ev.target as HTMLElement | null;
  if (!el) return;
  // 右键菜单单独处理：点菜单外面只收菜单，面板保持展开。
  if (menu.open && !el.closest(".tp-menu")) menu.open = false;
  if (panelRef.value?.contains(el)) return;
  if (el.closest(".index-wrap")) return;
  if (el.closest(".tp-menu")) return;
  emit("close");
}

function onDocKeyDown(ev: KeyboardEvent) {
  if (!props.open) return;
  if (ev.key === "Escape") {
    if (menu.open) menu.open = false;
    else emit("close");
  }
}

onMounted(() => {
  document.addEventListener("mousedown", onDocPointerDown, true);
  document.addEventListener("keydown", onDocKeyDown, true);
});

onBeforeUnmount(() => {
  document.removeEventListener("mousedown", onDocPointerDown, true);
  document.removeEventListener("keydown", onDocKeyDown, true);
  if (searchTimer) window.clearTimeout(searchTimer);
});
</script>

<template>
  <Transition name="tp-drop">
    <div v-if="open" ref="panelRef" class="tree-panel" @contextmenu.prevent>
      <div class="tp-head">
        <div class="tp-title">
          <FolderTree :size="16" aria-hidden="true" />
          <span>{{ t("索引目录结构") }}</span>
        </div>
        <div class="tp-tools">
          <button class="tp-icon" :title="t('刷新')" :aria-label="t('刷新')" @click="refresh">
            <RefreshCw :size="14" :class="{ spin: refreshing }" aria-hidden="true" />
          </button>
          <button
            class="tp-icon"
            :title="t('在资源管理器中打开')"
            :aria-label="t('在资源管理器中打开')"
            @click="openCurrent"
          >
            <ExternalLink :size="14" aria-hidden="true" />
          </button>
        </div>
      </div>

      <div class="tp-tabs" role="tablist">
        <button
          class="tp-tab"
          :class="{ active: tab === 'tree' }"
          role="tab"
          :aria-selected="tab === 'tree'"
          @click="tab = 'tree'"
        >{{ t("目录树") }}</button>
        <button
          class="tp-tab"
          :class="{ active: tab === 'graph' }"
          role="tab"
          :aria-selected="tab === 'graph'"
          @click="tab = 'graph'"
        >{{ t("星系图") }}</button>
      </div>

      <div v-if="tab === 'tree'" class="tp-search">
        <Search :size="13" class="tp-search-ico" aria-hidden="true" />
        <input v-model="query" :placeholder="t('搜索文件夹…')" spellcheck="false" autocomplete="off" />
        <button v-if="query" class="tp-clear" :title="t('清空')" @click="query = ''">
          <X :size="12" aria-hidden="true" />
        </button>
      </div>

      <div v-if="tab === 'graph'" class="tp-graph">
        <GalaxyCanvas compact :root="graphRoot" :depth="2" :max-nodes="320" :file-limit="5" />
      </div>

      <div v-show="tab === 'tree'" ref="bodyRef" class="tp-body">
        <template v-if="query.trim()">
          <div v-if="searching" class="tp-empty">
            <LoaderCircle :size="13" class="spin" aria-hidden="true" />
            <span>{{ t("搜索中…") }}</span>
          </div>
          <div v-else-if="!resultRows.length" class="tp-empty">{{ t("没有匹配的文件夹") }}</div>
          <button
            v-for="r in resultRows"
            :key="r.entry.path"
            class="tp-row"
            :title="rowTitle(r.entry)"
            @click="reveal(r.entry)"
          >
            <span class="tp-caret"></span>
            <Folder :size="16" class="tp-ico folder" aria-hidden="true" />
            <span class="tp-name">{{ r.pre }}<mark v-if="r.hit">{{ r.hit }}</mark>{{ r.post }}</span>
            <span class="tp-meta">{{ r.entry.total_files.toLocaleString() }}</span>
          </button>
        </template>

        <template v-else>
          <div v-if="!levels[''] && loading.has('')" class="tp-empty">
            <LoaderCircle :size="13" class="spin" aria-hidden="true" />
            <span>{{ t("读取索引结构…") }}</span>
          </div>
          <div v-else-if="errors['']" class="tp-empty err">{{ errors[""] }}</div>
          <div v-else-if="!rows.length" class="tp-empty">{{ t("索引里还没有任何目录") }}</div>

          <template v-for="(row, i) in rows" :key="row.kind === 'node' ? row.entry.path : 'more-' + i">
            <div
              v-if="row.kind === 'more'"
              class="tp-more"
              :style="{ paddingLeft: 8 + row.depth * 16 + 'px' }"
            >
              {{ t("另有") }} {{ row.count.toLocaleString() }} {{ t("个文件未显示") }}
            </div>
            <div
              v-else
              class="tp-row"
              :class="{ sel: selected === row.entry.path }"
              :data-row="i"
              :style="{ paddingLeft: 8 + row.depth * 16 + 'px' }"
              :title="rowTitle(row.entry)"
              @click="toggle(row.entry)"
              @contextmenu="openMenu($event, row.entry)"
            >
              <span class="tp-caret" :class="{ leaf: row.entry.is_file }">
                <LoaderCircle
                  v-if="loading.has(key(row.entry.path))"
                  :size="12"
                  class="spin"
                  aria-hidden="true"
                />
                <ChevronRight
                  v-else
                  :size="13"
                  :class="{ open: expanded.has(key(row.entry.path)) }"
                  aria-hidden="true"
                />
              </span>
              <component
                :is="iconFor(row.entry)"
                :size="16"
                class="tp-ico"
                :class="{ folder: !row.entry.is_file }"
                aria-hidden="true"
              />
              <span class="tp-name">{{ row.entry.name }}</span>
              <CircleAlert
                v-if="row.entry.error"
                :size="13"
                class="tp-alert"
                :title="row.entry.error"
                aria-hidden="true"
              />
              <span v-else-if="!row.entry.is_file" class="tp-meta">
                {{ row.entry.total_files.toLocaleString() }}
              </span>
              <span v-else class="tp-meta">{{ humanSize(row.entry.size) }}</span>
              <span v-if="row.entry.is_file" class="tp-dot" :class="dotClass(row.entry)"></span>
            </div>
          </template>
        </template>
      </div>

      <div class="tp-foot">
        <span class="tp-stat">{{ footerText }}</span>
        <button class="tp-link" @click="emit('open-full')">{{ t("完整视图") }} →</button>
      </div>
    </div>
  </Transition>

  <Teleport to="body">
    <div
      v-if="menu.open && menu.entry"
      class="tp-menu"
      :style="{ left: menu.x + 'px', top: menu.y + 'px' }"
      @mousedown.stop
      @contextmenu.prevent
    >
      <button @click="runMenu('reveal')">{{ t("打开文件位置") }}</button>
      <button @click="runMenu('copy-path')">{{ t("复制完整路径") }}</button>
      <button @click="runMenu('copy-name')">{{ t("复制文件名") }}</button>
      <div class="tp-menu-sep"></div>
      <button @click="runMenu('workspace')">{{ t("加入工作区") }}</button>
    </div>
  </Teleport>
</template>

<style scoped>
.tp-tabs {
  display: flex;
  gap: 2px;
  padding: 0 8px;
  border-bottom: 1px solid #111;
}
.tp-tab {
  background: transparent;
  border: 0;
  border-bottom: 2px solid transparent;
  color: #555;
  font-size: 12px;
  padding: 6px 10px;
  cursor: pointer;
}
.tp-tab:hover { color: #111; }
.tp-tab.active { color: #111; font-weight: 600; border-bottom-color: #111; }
.tp-graph { flex: 1; min-height: 0; display: flex; padding: 6px; }
.tp-graph > * { flex: 1; min-height: 240px; }
.tree-panel {
  --tp-bg: #ffffff;
  --tp-text: #333333;
  --tp-muted: #999999;
  --tp-line: #ececec;
  --tp-border: #cccccc;
  --tp-hover: #f7f7f7;
  --tp-sel: #f5f5f5;
  --tp-hl: #e6f7ff;
  position: absolute;
  top: calc(100% + 8px);
  right: 0;
  z-index: 120;
  width: 400px;
  max-width: calc(100vw - 24px);
  min-height: 268px;
  max-height: 80vh;
  display: flex;
  flex-direction: column;
  background: var(--tp-bg);
  color: var(--tp-text);
  border: 1px solid var(--tp-border);
  border-radius: 5px;
  box-shadow: 0 10px 30px rgba(0, 0, 0, 0.13), 0 2px 6px rgba(0, 0, 0, 0.06);
  font-family: Inter, Roboto, "Segoe UI", "Microsoft YaHei", system-ui, sans-serif;
  font-size: 12.5px;
  overflow: hidden;
}

/* 顶栏 */
.tp-head {
  flex: none;
  height: 38px;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 8px 0 12px;
  border-bottom: 1px solid var(--tp-line);
}
.tp-title {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  font-size: 12.5px;
  font-weight: 500;
  color: var(--tp-text);
  letter-spacing: 0.2px;
}
.tp-title svg { color: #8c8c8c; }
.tp-tools { margin-left: auto; display: inline-flex; align-items: center; gap: 2px; }
.tp-icon {
  width: 26px;
  height: 26px;
  display: inline-grid;
  place-items: center;
  padding: 0;
  border: 1px solid transparent;
  border-radius: 3px;
  background: transparent;
  color: #8c8c8c;
  cursor: pointer;
}
.tp-icon:hover { background: var(--tp-hover); border-color: var(--tp-line); color: var(--tp-text); }

/* 搜索 */
.tp-search {
  flex: none;
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 7px 10px 6px;
  padding: 0 7px;
  height: 27px;
  border: 1px solid #dedede;
  border-radius: 3px;
  background: #fafafa;
}
.tp-search:focus-within { border-color: #b9b9b9; background: #fff; }
.tp-search-ico { color: #a8a8a8; flex: none; }
.tp-search input {
  flex: 1;
  min-width: 0;
  height: 100%;
  padding: 0;
  border: 0;
  outline: 0;
  background: transparent;
  color: var(--tp-text);
  font-size: 12px;
}
.tp-search input::placeholder { color: #b3b3b3; }
.tp-clear {
  width: 18px;
  height: 18px;
  display: inline-grid;
  place-items: center;
  padding: 0;
  border: 0;
  border-radius: 50%;
  background: transparent;
  color: #a0a0a0;
  cursor: pointer;
}
.tp-clear:hover { background: #ececec; color: #555; }

/* 树主体 */
.tp-body {
  flex: 1;
  min-height: 120px;
  max-height: 56vh;
  overflow: auto;
  overscroll-behavior: contain;
  padding: 2px 0 6px;
}
.tp-body::-webkit-scrollbar { width: 8px; }
.tp-body::-webkit-scrollbar-thumb { background: #d8d8d8; border-radius: 4px; }
.tp-body::-webkit-scrollbar-thumb:hover { background: #bfbfbf; }

.tp-row {
  width: 100%;
  height: 28px;
  display: flex;
  align-items: center;
  gap: 5px;
  padding: 0 10px 0 8px;
  border: 0;
  border-radius: 0;
  background: transparent;
  color: var(--tp-text);
  font-size: 12.5px;
  text-align: left;
  cursor: default;
  user-select: none;
  white-space: nowrap;
}
.tp-row:hover { background: var(--tp-hover); border: 0; }
.tp-row.sel { background: var(--tp-sel); }

.tp-caret {
  width: 24px;
  height: 24px;
  flex: none;
  display: inline-grid;
  place-items: center;
  color: #a6a6a6;
}
.tp-caret.leaf { visibility: hidden; }
.tp-caret svg { transition: transform 0.12s ease; }
.tp-caret svg.open { transform: rotate(90deg); }

.tp-ico { flex: none; color: #a8a8a8; }
.tp-ico.folder { color: #8c8c8c; fill: currentColor; fill-opacity: 0.1; }

.tp-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.tp-name mark {
  background: var(--tp-hl);
  color: #333;
  font-weight: 600;
  padding: 0;
  border-radius: 2px;
}
.tp-meta {
  flex: none;
  color: var(--tp-muted);
  font-size: 11px;
  font-variant-numeric: tabular-nums;
  padding-left: 6px;
}
.tp-alert { flex: none; color: #d9534f; }

.tp-dot {
  flex: none;
  width: 6px;
  height: 6px;
  margin-left: 2px;
  border-radius: 50%;
  background: #d0d0d0;
}
.tp-dot.none { display: none; }
.tp-dot.ok { background: #3fb950; }
.tp-dot.part { background: #9ad3a4; }
.tp-dot.idle { background: #d8d8d8; }
.tp-dot.err { background: #d9534f; }

.tp-more {
  height: 26px;
  display: flex;
  align-items: center;
  color: #a3a3a3;
  font-size: 11.5px;
  font-style: italic;
}

.tp-empty {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 7px;
  padding: 30px 16px;
  color: #a3a3a3;
  font-size: 12px;
}
.tp-empty.err { color: #c0504d; }

/* 底栏 */
.tp-foot {
  flex: none;
  height: 31px;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 10px 0 12px;
  border-top: 1px solid var(--tp-line);
  background: #fcfcfc;
}
.tp-stat {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: #8f8f8f;
  font-size: 11px;
  font-variant-numeric: tabular-nums;
}
.tp-link {
  flex: none;
  padding: 2px 4px;
  border: 0;
  background: transparent;
  color: #7d7d7d;
  font-size: 11px;
  cursor: pointer;
}
.tp-link:hover { color: #333; background: transparent; border: 0; text-decoration: underline; }

.spin { animation: tp-spin 0.9s linear infinite; }
@keyframes tp-spin { to { transform: rotate(360deg); } }

/* 展开动画 */
.tp-drop-enter-active,
.tp-drop-leave-active { transition: opacity 0.14s ease, transform 0.14s ease; }
.tp-drop-enter-from,
.tp-drop-leave-to { opacity: 0; transform: translateY(-6px); }

/* 右键菜单（Teleport 到 body，仍带 scoped 属性） */
.tp-menu {
  position: fixed;
  z-index: 300;
  min-width: 168px;
  padding: 4px;
  background: #fff;
  color: #333;
  border: 1px solid #ccc;
  border-radius: 4px;
  box-shadow: 0 6px 20px rgba(0, 0, 0, 0.14);
  font-size: 12.5px;
}
.tp-menu button {
  display: block;
  width: 100%;
  padding: 6px 10px;
  border: 0;
  border-radius: 3px;
  background: transparent;
  color: #333;
  font-size: 12.5px;
  text-align: left;
  cursor: pointer;
}
.tp-menu button:hover { background: #f2f2f2; border: 0; }
.tp-menu-sep { height: 1px; margin: 4px 6px; background: #ececec; }

/* 暗色主题：沿用项目变量，避免在暗色界面里出现刺眼白块 */
html[data-theme="dark"] .tree-panel {
  --tp-bg: #171a18;
  --tp-text: #e9ede9;
  --tp-muted: #8f9a92;
  --tp-line: #2c322e;
  --tp-border: #3d4440;
  --tp-hover: #1f2422;
  --tp-sel: #242a27;
  --tp-hl: #1d3a4a;
  box-shadow: 0 10px 30px rgba(0, 0, 0, 0.5);
}
html[data-theme="dark"] .tp-search { background: #1e2220; border-color: #333a36; }
html[data-theme="dark"] .tp-search:focus-within { background: #1e2220; border-color: #4a544e; }
html[data-theme="dark"] .tp-search input::placeholder { color: #6c766f; }
html[data-theme="dark"] .tp-foot { background: #141715; }
html[data-theme="dark"] .tp-name mark { color: #e9ede9; }
html[data-theme="dark"] .tp-menu { background: #1b1f1d; border-color: #3d4440; color: #e9ede9; }
html[data-theme="dark"] .tp-menu button { color: #e9ede9; }
html[data-theme="dark"] .tp-menu button:hover { background: #262c28; }
html[data-theme="dark"] .tp-menu-sep { background: #2c322e; }

@media (prefers-reduced-motion: reduce) {
  .tp-drop-enter-active,
  .tp-drop-leave-active { transition: none; }
  .spin { animation: none; }
}
</style>
