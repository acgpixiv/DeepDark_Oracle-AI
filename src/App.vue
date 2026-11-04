<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { Database, Network, Search, Settings2 } from "lucide-vue-next";
import { api, type ScanProgress } from "./api";
import { refreshInfo, refreshStats, store, toast } from "./store";
import { applyAppearance } from "./theme";
import { langState, t } from "./i18n";
import SearchView from "./views/SearchView.vue";
import TreeView from "./views/TreeView.vue";
import GalaxyView from "./views/GalaxyView.vue";
import SettingsDrawer from "./components/SettingsDrawer.vue";
import IndexTreePanel from "./components/IndexTreePanel.vue";

type Page = "home" | "tree" | "graph" | "settings";
const page = ref<Page>("home");
/** 星系图谱的根目录（从目录树点进来时带路径，否则空串=所有索引根）。 */
const graphRoot = ref("");
const settingsTab = ref("general");
/** 右上角「索引目录结构」下拉面板是否展开。 */
const treePanelOpen = ref(false);
const thumbDir = ref("");
const notifyChanges = ref(true);
let statsTimer: number | undefined;
let scanTimer: number | undefined;
let lastScanRunning = false;
let lastScan: ScanProgress | null = null;

/** 面板底部的「完整视图」：收起面板并切到树状图页面。 */
function openFullTree() {
  treePanelOpen.value = false;
  page.value = "tree";
}

function showSettings(tab = "general") {
  settingsTab.value = tab;
  page.value = "settings";
}

/** 打开全屏星系图谱（REQ-002：快捷键 Ctrl+G / 顶栏图标）。 */
function openGalaxy(root = "") {
  treePanelOpen.value = false;
  graphRoot.value = root;
  page.value = "graph";
}

/** 全局快捷键：Ctrl+G 切星系图谱，Ctrl+F 回搜索。 */
function onGlobalKey(e: KeyboardEvent) {
  if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
  const key = e.key.toLowerCase();
  if (key === "g") {
    e.preventDefault();
    page.value === "graph" ? (page.value = "home") : openGalaxy();
  } else if (key === "f" && page.value === "graph") {
    e.preventDefault();
    page.value = "home";
  }
}

/** 读取配置并应用外观（主题 / 字号 / 语言）。 */
async function syncAppearance() {
  try {
    const cfg = await api.getConfig();
    applyAppearance(cfg.ui);
    notifyChanges.value = cfg.ui?.notify_file_changes !== false;
  } catch {
    /* 配置读不到时用默认外观 */
  }
}

/** 轮询扫描进度：从「运行中」变成「已结束」时在底部给一条轻量提示。 */
async function pollScan() {
  try {
    const p = await api.scanProgress();
    if (lastScanRunning && !p.running && lastScan) {
      const changed = (p.added || 0) + (p.updated || 0) + (p.missing || 0);
      if (changed > 0 && notifyChanges.value) {
        const parts: string[] = [];
        if (p.added) parts.push(`${t("新增")} ${p.added}`);
        if (p.updated) parts.push(`${t("更新")} ${p.updated}`);
        if (p.missing) parts.push(`${t("移除")} ${p.missing}`);
        toast(`${t("文件变化：")}${parts.join(" · ")}`, "info", 6000);
      }
      await refreshStats();
    }
    lastScanRunning = p.running;
    lastScan = p;
  } catch {
    /* 后端未就绪时忽略 */
  }
}

onMounted(async () => {
  await Promise.all([refreshInfo(), refreshStats(), syncAppearance()]);
  try {
    thumbDir.value = `${await api.dataDir()}\\thumbs`;
  } catch {}
  statsTimer = window.setInterval(refreshStats, 5000);
  scanTimer = window.setInterval(pollScan, 3000);
  window.addEventListener("keydown", onGlobalKey);
  void pollScan();
});

onBeforeUnmount(() => {
  if (statsTimer) window.clearInterval(statsTimer);
  if (scanTimer) window.clearInterval(scanTimer);
  window.removeEventListener("keydown", onGlobalKey);
});

// 从设置页返回时重新同步一次外观（用户可能刚改过主题）
watch(page, (next) => {
  if (next === "home") void syncAppearance();
});
</script>

<template>
  <div class="app-shell">
    <header class="topbar">
      <button
        v-if="page === 'settings'"
        class="search-link"
        :aria-label="t('文件搜索')"
        @click="page = 'home'"
      >
        <Search :size="18" aria-hidden="true" />
        <span>{{ t("文件搜索") }}</span>
      </button>
      <div class="index-wrap">
        <button
          class="index-link"
          :class="{ current: page === 'tree', open: treePanelOpen }"
          :title="t('查看索引目录结构')"
          :aria-expanded="treePanelOpen"
          @click="treePanelOpen = !treePanelOpen"
        >
          <Database :size="15" aria-hidden="true" />
          <span>
            {{ (store.stats?.files ?? 0).toLocaleString() }} {{ langState.current === "en" ? "files" : "个文件" }}
            <span class="index-divider">·</span>
            {{ store.stats?.bytes_label ?? "0 B" }}
          </span>
        </button>
        <IndexTreePanel
          :open="treePanelOpen"
          @close="treePanelOpen = false"
          @open-full="openFullTree"
        />
      </div>
      <button
        class="icon-button graph-button"
        :class="{ current: page === 'graph' }"
        :title="t('星系图谱（Ctrl+G）')"
        :aria-label="t('星系图谱')"
        @click="page === 'graph' ? (page = 'home') : openGalaxy()"
      >
        <Network :size="19" aria-hidden="true" />
      </button>
      <button class="icon-button settings-button" :title="t('设置')" :aria-label="t('设置')" @click="page === 'settings' ? (page = 'home') : showSettings()">
        <Settings2 :size="19" aria-hidden="true" />
      </button>
    </header>

    <main class="app-content">
      <Transition name="page" mode="out-in">
        <SearchView v-if="page === 'home'" key="home" :thumb-dir="thumbDir" />
        <TreeView v-else-if="page === 'tree'" key="tree" />
        <GalaxyView v-else-if="page === 'graph'" key="graph" :root="graphRoot" @back="page = 'home'" />
        <SettingsDrawer
          v-else
          key="settings"
          :open="true"
          :tab="settingsTab"
          @tab="settingsTab = $event"
        />
      </Transition>
    </main>

    <TransitionGroup tag="div" name="toast" class="toasts" aria-live="polite">
      <div v-for="item in store.toasts" :key="item.id" class="toast" :class="item.level">
        {{ item.text }}
      </div>
    </TransitionGroup>
  </div>
</template>

<style scoped>
.app-shell { height: 100%; display: flex; flex-direction: column; overflow: hidden; }
.topbar {
  height: 54px;
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 0 22px;
  border-bottom: 1px solid var(--border);
  background: var(--bg);
  flex: none;
  transition: background .18s ease, border-color .18s ease;
}
.search-link {
  width: 36px;
  height: 36px;
  flex: none;
  display: inline-flex;
  align-items: center;
  justify-content: flex-start;
  gap: 8px;
  padding: 0 8px;
  overflow: hidden;
  border: 1px solid transparent;
  background: transparent;
  color: var(--text);
  white-space: nowrap;
  transition: width .16s ease, border-color .16s ease, background .16s ease;
}
.search-link :deep(svg) { width: 18px; height: 18px; flex: none; }
.search-link span { width: 0; overflow: hidden; opacity: 0; transition: width .16s ease, opacity .16s ease; }
.search-link:hover, .search-link:focus-visible { width: 112px; border-color: var(--border); background: var(--panel); }
.search-link:hover span, .search-link:focus-visible span { width: 56px; opacity: 1; }
.index-wrap {
  position: relative;
  margin-left: auto;
  display: flex;
  align-items: center;
}
.index-link {
  min-width: 210px;
  display: inline-flex;
  justify-content: center;
  align-items: center;
  gap: 9px;
  padding: 7px 10px;
  border: 1px solid transparent;
  background: transparent;
  color: var(--muted);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  transition: background .16s ease, border-color .16s ease, color .16s ease;
}
.index-link:hover, .index-link.current, .index-link.open { background: var(--panel); border-color: var(--border); color: var(--text); }
.index-divider { padding: 0 3px; color: #59615d; }
.index-link :deep(svg:last-child) { color: #707a73; }
.icon-button {
  width: 36px;
  height: 36px;
  display: inline-grid;
  place-items: center;
  padding: 0;
  border: 1px solid transparent;
  background: transparent;
  color: var(--muted);
  transition: background .16s ease, border-color .16s ease, color .16s ease, transform .1s ease;
}
.icon-button:hover { background: var(--panel); border-color: var(--border); color: var(--text); }
.icon-button:active { transform: scale(.94); }
.settings-button { margin-left: 4px; }
.graph-button { margin-left: 2px; }
.graph-button.current { background: var(--panel); border-color: var(--border); color: var(--text); }
.app-content { flex: 1; min-height: 0; min-width: 0; overflow: hidden; }

/* 页面切换：淡入 + 轻微上移，避免整块界面硬切。 */
.page-enter-active, .page-leave-active { transition: opacity .18s ease, transform .18s ease; }
.page-enter-from { opacity: 0; transform: translateY(6px); }
.page-leave-to { opacity: 0; transform: translateY(-4px); }

.toasts { position: fixed; right: 18px; bottom: 18px; z-index: 100; display: flex; flex-direction: column; align-items: flex-end; gap: 8px; pointer-events: none; }
.toast { max-width: min(420px, calc(100vw - 36px)); padding: 9px 12px; border: 1px solid var(--border); border-radius: 4px; background: var(--panel); color: var(--text); box-shadow: 0 5px 18px rgba(0, 0, 0, .12); font-size: 12px; }
.toast.ok { border-color: #85b894; background: #f1faf3; color: #245a32; }
.toast.warn { border-color: #cfad6b; background: #fffaf0; color: #715414; }
.toast.error { border-color: #d58b86; background: #fff5f4; color: #7b2b27; }
.toast-enter-active, .toast-leave-active { transition: opacity .2s ease, transform .2s ease; }
.toast-enter-from, .toast-leave-to { opacity: 0; transform: translateX(18px); }
.toast-move { transition: transform .2s ease; }

@media (max-width: 560px) {
  .topbar { padding: 0 12px; }
  .index-wrap { margin-left: auto; }
  .index-link { min-width: 0; gap: 6px; padding-inline: 7px; }
  .index-link span { max-width: 175px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
}
@media (prefers-reduced-motion: reduce) {
  .page-enter-active, .page-leave-active, .toast-enter-active, .toast-leave-active { transition: none; }
}
</style>
