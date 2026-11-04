<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { Cpu, Files, MonitorCog, Package, ScrollText, SlidersHorizontal, Wifi } from "lucide-vue-next";
import { api, type Activity, type AppConfig, type PartialConfig, type Rule, type ServerStatus, type WatchStatus } from "../api";
import { t } from "../i18n";
import { refreshStats, toast } from "../store";
import { applyAppearance } from "../theme";
import SettingsGeneralTab from "./SettingsGeneralTab.vue";
import SettingsIndexTab from "./SettingsIndexTab.vue";
import SettingsModelTab from "./SettingsModelTab.vue";
import SettingsLanTab from "./SettingsLanTab.vue";
import SettingsLogTab from "./SettingsLogTab.vue";
import SettingsExtensionTab from "./SettingsExtensionTab.vue";
import SettingsGpuTab from "./SettingsGpuTab.vue";

const props = defineProps<{ open: boolean; tab: string }>();
const emit = defineEmits<{ (event: "tab", tab: string): void }>();

const TABS = [
  { key: "general", label: "通用配置", icon: SlidersHorizontal },
  { key: "index", label: "文件索引", icon: Files },
  { key: "model", label: "模型配置", icon: Cpu },
  { key: "lan", label: "局域网配置", icon: Wifi },
  { key: "render", label: "图形与 GPU", icon: MonitorCog },
  { key: "extensions", label: "可选扩展", icon: Package },
  { key: "log", label: "日志", icon: ScrollText },
] as const;

const activeTab = computed(() => TABS.some((item) => item.key === props.tab) ? props.tab : "general");
const cfg = ref<AppConfig | null>(null);
const rules = ref<Rule[]>([]);
const logs = ref<Activity[]>([]);
const logKind = ref("");
const srv = ref<ServerStatus | null>(null);
const watcher = ref<WatchStatus | null>(null);
const dataDir = ref("");
const watchRootsText = ref("");
const excludeDirsText = ref("");
const loading = ref(false);
const clearConfirmOpen = ref(false);
const clearBusy = ref(false);

const newRule = ref<Rule>({
  id: 0,
  name: "",
  field: "name",
  op: "contains",
  value: "",
  category: "",
  priority: 10,
  enabled: true,
});

async function load() {
  loading.value = true;
  try {
    const [config, currentServer, currentWatcher, currentRules, currentLogs, dir] = await Promise.all([
      api.getConfig(),
      api.serverStatus(),
      api.watchStatus(),
      api.rulesList(),
      api.activityList(logKind.value || undefined, 300),
      api.dataDir(),
    ]);
    cfg.value = config;
    srv.value = currentServer;
    watcher.value = currentWatcher;
    rules.value = currentRules;
    logs.value = currentLogs;
    dataDir.value = dir;
    watchRootsText.value = (config.watch.roots || []).join("\n");
    excludeDirsText.value = (config.scan.exclude_dirs || []).join("\n");
  } catch (error) {
    toast(`${t("读取设置失败：")}${error}`, "error");
  } finally {
    loading.value = false;
  }
}

async function savePatch(patch: PartialConfig, message = "设置已保存") {
  try {
    await api.saveConfig(patch);
    cfg.value = await api.getConfig();
    toast(t(message), "ok");
    return true;
  } catch (error) {
    toast(`${t("保存失败：")}${error}`, "error");
    return false;
  }
}

async function saveFileConfig(): Promise<boolean> {
  if (!cfg.value) return false;
  return savePatch({
    scan: {
      max_size_mb: cfg.value.scan.max_size_mb,
      min_size_kb: cfg.value.scan.min_size_kb,
      exclude_dirs: excludeDirsText.value.split(/\r?\n|,/).map((value) => value.trim()).filter(Boolean),
      follow_links: cfg.value.scan.follow_links,
    },
    index: {
      max_text_chars: cfg.value.index.max_text_chars,
      image_max_side: cfg.value.index.image_max_side,
      image_token_budget: cfg.value.index.image_token_budget,
      pdf_image_token_budget: cfg.value.index.pdf_image_token_budget,
      video_token_budget: cfg.value.index.video_token_budget,
      vision_encoder: cfg.value.index.vision_encoder,
      audio_encoder: cfg.value.index.audio_encoder,
      video_frames: cfg.value.index.video_frames,
      video_frame_interval: cfg.value.index.video_frame_interval,
      video_max_frames: cfg.value.index.video_max_frames,
      video_dedup_distance: cfg.value.index.video_dedup_distance,
      video_index_audio: cfg.value.index.video_index_audio,
      audio_seconds: cfg.value.index.audio_seconds,
      batch_size: cfg.value.index.batch_size,
      auto_fulltext: cfg.value.index.auto_fulltext,
      auto_embed: cfg.value.index.auto_embed,
    },
    search: {
      page_size: cfg.value.search.page_size,
      use_pinyin: cfg.value.search.use_pinyin,
      use_fuzzy: cfg.value.search.use_fuzzy,
      use_semantic: cfg.value.search.use_semantic,
      use_fulltext: cfg.value.search.use_fulltext,
      semantic_weight: cfg.value.search.semantic_weight,
      semantic_floor: cfg.value.search.semantic_floor,
      debounce_ms: cfg.value.search.debounce_ms,
    },
  });
}

/** 通用配置：界面、启动行为和「工作区」目录。 */
async function saveGeneralConfig(): Promise<boolean> {
  if (!cfg.value) return false;
  const startupScope = String(cfg.value.workspace?.startup_scope ?? "remember");
  const startupPath = String(cfg.value.workspace?.startup_path ?? "");
  const ok = await savePatch({
    ui: {
      language: cfg.value.ui.language,
      theme: cfg.value.ui.theme,
      font_size: cfg.value.ui.font_size,
      remember_window: cfg.value.ui.remember_window,
      autostart: cfg.value.ui.autostart,
      auto_scan_on_start: cfg.value.ui.auto_scan_on_start,
      notify_file_changes: cfg.value.ui.notify_file_changes,
      default_search_mode: cfg.value.ui.default_search_mode,
      professional_mode: cfg.value.ui.professional_mode,
    },
    model: {
      parallel_mode: cfg.value.model.parallel_mode,
      parallel_threads: cfg.value.model.parallel_threads,
    },
    workspace: {
      dirs: (cfg.value.workspace?.dirs as string[]) ?? [],
      enabled: cfg.value.workspace?.enabled ?? true,
      live_extract: cfg.value.workspace?.live_extract ?? true,
      max_file_mb: cfg.value.workspace?.max_file_mb ?? 64,
      depth: cfg.value.workspace?.depth ?? 4,
      recent_paths: (cfg.value.workspace?.recent_paths as string[]) ?? [],
      pinned_paths: (cfg.value.workspace?.pinned_paths as string[]) ?? [],
      startup_scope: cfg.value.workspace?.startup_scope ?? "remember",
      startup_path: cfg.value.workspace?.startup_path ?? "",
      last_scope: cfg.value.workspace?.last_scope ?? "indexed",
      last_path: cfg.value.workspace?.last_path ?? "",
    },
  }, "通用设置已保存");
  if (ok) {
    try {
      const workspace = await api.workspaceSetStartup(startupScope, startupPath || undefined);
      if (cfg.value) cfg.value.workspace = { ...cfg.value.workspace, ...workspace };
    } catch {
      toast(t("保存启动搜索范围失败"), "error");
      return false;
    }
  }
  // 注意：必须传入 ui，否则 applyAppearance 会用默认值（system / zh）把刚保存的语言和主题覆盖掉。
  if (ok) await applyAppearance(cfg.value?.ui);
  return ok;
}

async function saveModelConfig(): Promise<boolean> {
  if (!cfg.value) return false;
  return savePatch({
    model: {
      embed_model: cfg.value.model.embed_model,
      chat_model: cfg.value.model.chat_model,
      n_threads: cfg.value.model.n_threads,
      gpu_device: cfg.value.model.gpu_device,
      context_size: cfg.value.model.context_size,
      auto_load: cfg.value.model.auto_load,
      parallel_mode: cfg.value.model.parallel_mode,
      parallel_threads: cfg.value.model.parallel_threads,
      clip_dir: cfg.value.model.clip_dir,
      clip_enabled: cfg.value.model.clip_enabled,
      clap_dir: cfg.value.model.clap_dir,
      clap_enabled: cfg.value.model.clap_enabled,
    },
  }, "模型设置已保存");
}

function watchRoots() {
  return watchRootsText.value.split(/\r?\n|,/).map((value) => value.trim()).filter(Boolean);
}

async function saveLanConfig(): Promise<boolean> {
  if (!cfg.value) return false;
  return savePatch({
    server: {
      enabled: cfg.value.server.enabled,
      bind: cfg.value.server.bind,
      port: cfg.value.server.port,
      token: cfg.value.server.token,
      allow_write: cfg.value.server.allow_write,
    },
    watch: {
      enabled: cfg.value.watch.enabled,
      auto_index: cfg.value.watch.auto_index,
      debounce_secs: cfg.value.watch.debounce_secs,
      roots: watchRoots(),
    },
  }, "局域网设置已保存");
}

async function toggleServer() {
  if (!(await saveLanConfig())) return;
  try {
    if (srv.value?.running) {
      await api.serverStop();
      srv.value = { running: false, url: "" };
      toast(t("局域网服务已停止"), "warn");
    } else {
      const url = await api.serverStart();
      srv.value = { running: true, url };
      toast(`${t("局域网服务已启动：")}${url}`, "ok");
    }
  } catch (error) {
    toast(`${t("服务操作失败：")}${error}`, "error");
  }
}

async function toggleWatcher() {
  if (!(await saveLanConfig())) return;
  try {
    if (watcher.value?.running) {
      await api.watchStop();
      toast(t("实时监控已停止"), "warn");
    } else {
      const count = await api.watchStart();
      toast(`${t("实时监控已开启，监视")} ${count} ${t("个目录")}`, "ok");
    }
    watcher.value = await api.watchStatus();
  } catch (error) {
    toast(`${t("监控操作失败：")}${error}`, "error");
  }
}

async function refreshLogs() {
  try {
    logs.value = await api.activityList(logKind.value || undefined, 300);
  } catch (error) {
    toast(`${t("读取日志失败：")}${error}`, "error");
  }
}

async function addRule() {
  if (!newRule.value.value.trim() || !newRule.value.category.trim()) {
    toast(t("请填写匹配内容与分类名"), "warn");
    return;
  }
  rules.value.push({ ...newRule.value, id: 0 });
  await saveRules();
  newRule.value.value = "";
  newRule.value.category = "";
}

async function saveRules() {
  try {
    await api.rulesSave(rules.value.map((rule, index) => ({ ...rule, priority: rule.priority ?? index })));
    toast(t("分类规则已保存"), "ok");
  } catch (error) {
    toast(`${t("保存规则失败：")}${error}`, "error");
  }
}

async function deleteRule(index: number) {
  rules.value.splice(index, 1);
  await saveRules();
}

async function applyRules() {
  try {
    const count = await api.rulesApply();
    toast(`${t("已重新分类")} ${count} ${t("个文件")}`, "ok");
    await refreshStats();
  } catch (error) {
    toast(`${t("应用规则失败：")}${error}`, "error");
  }
}

async function vacuum() {
  try {
    await api.dbVacuum();
    toast(t("数据库已压缩"), "ok");
    await refreshStats();
  } catch (error) {
    toast(`${t("压缩失败：")}${error}`, "error");
  }
}

async function clearAll() {
  if (clearBusy.value) return;
  clearConfirmOpen.value = true;
}

async function confirmClearAll() {
  if (clearBusy.value) return;
  clearBusy.value = true;
  try {
    await api.dbClear();
    clearConfirmOpen.value = false;
    toast(t("索引已清空"), "ok");
    await refreshStats();
  } catch (error) {
    toast(`${t("清空失败：")}${error}`, "error");
  } finally {
    clearBusy.value = false;
  }
}

watch(() => props.open, (open) => { if (open) load(); });
watch(() => props.tab, (tab) => { if (tab === "log") refreshLogs(); });
onMounted(load);
</script>

<template>
  <div class="settings-page">
    <aside class="settings-sidebar">
      <div class="settings-sidebar-head">
        <div>
          <div class="settings-title">{{ t("设置") }}</div>
          <div class="settings-subtitle">DeepDark_Oracle</div>
        </div>
      </div>
      <nav class="settings-nav" :aria-label="t('设置分类')">
        <button
          v-for="item in TABS"
          :key="item.key"
          class="settings-nav-item"
          :class="{ active: activeTab === item.key }"
          @click="emit('tab', item.key)"
        >
          <component :is="item.icon" :size="17" aria-hidden="true" />
          <span>{{ t(item.label) }}</span>
        </button>
      </nav>
    </aside>

    <section class="settings-main">
      <header class="settings-main-head">
        <div>
          <h1>{{ t(TABS.find((item) => item.key === activeTab)?.label ?? "设置") }}</h1>
          <p v-if="activeTab === 'general'">{{ t("界面语言与主题、启动行为，以及无需建索引的工作区目录。") }}</p>
          <p v-else-if="activeTab === 'index'">{{ t("扫描目录、建立全文与向量索引、管理分类规则。") }}</p>
          <p v-else-if="activeTab === 'model'">{{ t("管理嵌入模型、跨模态模型和 GPU 推理参数。") }}</p>
          <p v-else-if="activeTab === 'lan'">{{ t("配置局域网访问和文件变化监控。") }}</p>
          <p v-else-if="activeTab === 'render'">{{ t("图形渲染后端、画质档位与 GPU 兼容性诊断。") }}</p>
          <p v-else-if="activeTab === 'log'">{{ t("查看应用、扫描、索引和模型活动。") }}</p>
          <p v-else>{{ t("按需安装 ONNX Runtime 和 Python sidecar，扩展文件保存在项目目录。") }}</p>
        </div>
      </header>

      <div v-if="loading" class="settings-loading">{{ t("正在读取设置…") }}</div>
      <div v-else-if="cfg" class="settings-scroll">
        <SettingsGeneralTab
          v-if="activeTab === 'general'"
          :cfg="cfg"
          :save-general-config="saveGeneralConfig"
        />
        <SettingsIndexTab
          v-if="activeTab === 'index'"
          :cfg="cfg"
          :rules="rules"
          :new-rule="newRule"
          :exclude-dirs-text="excludeDirsText"
          :data-dir="dataDir"
          :save-file-config="saveFileConfig"
          :add-rule="addRule"
          :delete-rule="deleteRule"
          :apply-rules="applyRules"
          :save-rules="saveRules"
          :vacuum="vacuum"
          :clear-all="clearAll"
          @update-exclude-dirs="excludeDirsText = $event"
        />
        <SettingsModelTab
          v-if="activeTab === 'model'"
          :cfg="cfg"
          :save-model-config="saveModelConfig"
        />
        <SettingsLanTab
          v-if="activeTab === 'lan'"
          :cfg="cfg"
          :srv="srv"
          :watcher="watcher"
          :watch-roots-text="watchRootsText"
          :save-lan-config="saveLanConfig"
          :toggle-server="toggleServer"
          :toggle-watcher="toggleWatcher"
          @update-watch-roots="watchRootsText = $event"
        />
        <SettingsLogTab
          v-if="activeTab === 'log'"
          :logs="logs"
          :log-kind="logKind"
          :refresh-logs="refreshLogs"
          @update-log-kind="logKind = $event"
        />
        <SettingsGpuTab v-if="activeTab === 'render'" />
        <SettingsExtensionTab v-if="activeTab === 'extensions'" />
      </div>
    </section>

    <div v-if="clearConfirmOpen" class="clear-modal-backdrop" role="presentation" @click.self="clearConfirmOpen = false">
      <section class="clear-modal" role="dialog" aria-modal="true" :aria-label="t('确认清空索引')">
        <button class="clear-modal-close" type="button" :title="t('关闭')" @click="clearConfirmOpen = false">×</button>
        <h2>{{ t("确认清空索引") }}</h2>
        <p>{{ t("这会删除本地索引、正文和向量数据，但不会删除原始文件。清空后需要重新建立索引。") }}</p>
        <div class="clear-modal-actions">
          <button type="button" class="secondary-button" :disabled="clearBusy" @click="clearConfirmOpen = false">{{ t("取消") }}</button>
          <button type="button" class="clear-danger-button" :disabled="clearBusy" @click="confirmClearAll">{{ clearBusy ? t("正在清空…") : t("确认清空") }}</button>
        </div>
      </section>
    </div>
  </div>
</template>

<style scoped>
.settings-page { height: 100%; display: flex; min-height: 0; background: var(--bg); }
.settings-sidebar { width: 218px; flex: none; display: flex; flex-direction: column; border-right: 1px solid var(--border); background: #151a17; }
.settings-sidebar-head { display: flex; align-items: center; gap: 9px; min-height: 68px; padding: 12px 15px; border-bottom: 1px solid var(--border); }
.back-button { width: 30px; height: 30px; }
.settings-title { font-size: 15px; font-weight: 650; }
.settings-subtitle { color: #849087; font: 10px Consolas, monospace; margin-top: 2px; }
.settings-nav { display: flex; flex-direction: column; gap: 3px; padding: 14px 10px; }
.settings-nav-item { display: flex; align-items: center; gap: 10px; width: 100%; padding: 10px 12px; border: 1px solid transparent; border-radius: 6px; background: transparent; color: #909b93; text-align: left; font-size: 12px; }
.settings-nav-item:hover { color: var(--text); background: #202723; border-color: #2e3a32; }
.settings-nav-item.active { color: #d8e8dc; background: #294033; border-color: #45624e; }
.settings-sidebar-foot { display: none; margin-top: auto; padding: 14px; line-height: 1.5; }
.settings-main { flex: 1; min-width: 0; min-height: 0; display: flex; flex-direction: column; }
.settings-main-head { display: flex; justify-content: space-between; align-items: flex-start; gap: 18px; padding: 24px clamp(18px, 4vw, 58px) 18px; border-bottom: 1px solid var(--border); }
.settings-main-head h1 { min-height: 0; margin: 0; color: var(--text); font-size: 21px; font-weight: 600; }
.settings-main-head p { margin: 6px 0 0; color: #849087; font-size: 12px; }
.settings-scroll { flex: 1; min-height: 0; overflow: auto; padding: 18px clamp(18px, 4vw, 58px) 38px; }
.settings-loading { display: grid; place-items: center; flex: 1; color: var(--muted); font-size: 12px; }
.clear-modal-backdrop { position: fixed; inset: 0; z-index: 400; display: grid; place-items: center; padding: 18px; background: rgba(0,0,0,.35); backdrop-filter: blur(5px); }
.clear-modal { position: relative; width: min(470px, calc(100vw - 36px)); padding: 24px; border: 2px solid #b52525; border-radius: 4px; background: #fff; color: #111; box-shadow: 0 20px 55px rgba(0,0,0,.24); }
.clear-modal h2 { margin: 0 0 10px; font-size: 18px; color: #8f1d1d; }
.clear-modal p { margin: 0; color: #333; font-size: 12px; line-height: 1.7; }
.clear-modal-close { position: absolute; top: 8px; right: 10px; padding: 0 3px; border: 0; background: transparent; color: #8f1d1d; font-size: 22px; line-height: 1; }
.clear-modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 22px; }
.clear-danger-button { padding: 7px 14px; border: 1px solid #b52525; border-radius: 2px; background: #fff; color: #b52525; }
.clear-danger-button:hover { background: #fff0f0; }
.settings-scroll :deep(.embedded-view) { height: auto; min-height: 0; margin-bottom: 16px; }
.settings-scroll :deep(.embedded-view .view) { height: auto; min-height: 0; overflow: visible; }
.settings-scroll :deep(.embedded-view .list) { flex: none; min-height: 0; max-height: none; overflow: visible; }
.settings-scroll :deep(.settings-section) { margin-top: 14px; padding: 18px; border: 1px solid var(--border); border-radius: 7px; background: #151a17; }
.settings-scroll :deep(.section-head) { display: flex; align-items: flex-start; justify-content: space-between; gap: 18px; margin-bottom: 16px; }
.settings-scroll :deep(.section-head h2) { display: flex; align-items: center; gap: 7px; margin: 0; color: #cbd5cd; font-size: 14px; font-weight: 600; }
.settings-scroll :deep(.section-head p), .settings-scroll :deep(.settings-section > div > p) { margin: 5px 0 0; color: #849087; font-size: 11px; }
.settings-scroll :deep(.form-grid) { display: grid; gap: 12px; }
.settings-scroll :deep(.form-grid.three) { grid-template-columns: repeat(3, minmax(0, 1fr)); }
.settings-scroll :deep(label) { display: flex; flex-direction: column; gap: 6px; color: #9aa59d; font-size: 11px; }
.settings-scroll :deep(.check-label),
.settings-scroll :deep(.extension-chip) {
  flex-direction: row;
  align-items: center;
  gap: 6px;
}
.settings-scroll :deep(.extension-chip) {
  display: inline-flex;
  width: auto;
  white-space: nowrap;
}
.settings-scroll :deep(label.extension-chip) {
  display: inline-flex;
  flex-direction: row !important;
  align-items: center;
  justify-content: flex-start;
  width: auto;
  min-height: 18px;
  white-space: nowrap;
}
.settings-scroll :deep(.extension-chip input[type="checkbox"]) {
  appearance: none;
  -webkit-appearance: none;
  width: 14px;
  height: 14px;
  flex: 0 0 14px;
  padding: 0;
  margin: 0;
  border: 1px solid #111;
  border-radius: 0;
  background: #fff;
  accent-color: transparent;
  color: #fff;
}
.settings-scroll :deep(.extension-chip input[type="checkbox"]:checked) {
  background: #111;
}
.settings-scroll :deep(.extension-chip input[type="checkbox"]:checked::after) {
  width: auto;
  height: auto;
  border: 0;
  transform: none;
  content: "✓";
  display: block;
  color: #fff;
  text-align: center;
  font-size: 11px;
  line-height: 12px;
  font-weight: 700;
}
.settings-scroll :deep(.switch-row input[type="checkbox"]),
.settings-scroll :deep(.check-label input[type="checkbox"]) {
  appearance: none;
  -webkit-appearance: none;
  width: 16px;
  height: 16px;
  flex: 0 0 16px;
  margin: 0;
  border: 1px solid #111;
  border-radius: 0;
  background: #fff;
  accent-color: transparent;
  color: #fff;
}
.settings-scroll :deep(.switch-row input[type="checkbox"]:checked),
.settings-scroll :deep(.check-label input[type="checkbox"]:checked) {
  background: #111;
}
.settings-scroll :deep(.switch-row input[type="checkbox"]:checked::after),
.settings-scroll :deep(.check-label input[type="checkbox"]:checked::after) {
  width: auto;
  height: auto;
  border: 0;
  transform: none;
  content: "✓";
  display: block;
  color: #fff;
  text-align: center;
  font-size: 12px;
  line-height: 14px;
  font-weight: 700;
}
.settings-scroll :deep(label small) { color: var(--muted); font-size: 10px; line-height: 1.35; }
.settings-scroll :deep(label input), .settings-scroll :deep(label select), .settings-scroll :deep(textarea) { width: 100%; min-width: 0; }
.settings-scroll :deep(textarea) { resize: vertical; background: var(--panel-2); border: 1px solid var(--border); border-radius: 6px; padding: 8px 10px; color: var(--text); font: inherit; outline: none; }
.settings-scroll :deep(textarea:focus) { border-color: var(--accent); }
.settings-scroll :deep(.switch-row) { display: flex; flex-wrap: wrap; gap: 15px; margin-top: 15px; }
.settings-scroll :deep(.switch-row label) { flex-direction: row; align-items: center; gap: 6px; color: #aeb9b0; }
.settings-scroll :deep(.switch-row input) { width: auto; accent-color: var(--accent); }
.settings-scroll :deep(.save-button), .settings-scroll :deep(.secondary-button), .settings-scroll :deep(.danger-button) { display: inline-flex; align-items: center; gap: 6px; padding: 7px 11px; border-radius: 5px; font-size: 11px; }
.settings-scroll :deep(.save-button) { color: #142017; background: var(--accent); border-color: var(--accent); }
.settings-scroll :deep(.save-button:hover) { background: #a9d9b4; border-color: #a9d9b4; }
.settings-scroll :deep(.secondary-button) { background: #222a25; border-color: #344139; color: #c4d0c7; }
.settings-scroll :deep(.danger-button) { background: transparent; border-color: #593a37; color: #f0a09a; }
.settings-scroll :deep(.action-row) { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 16px; }
.settings-scroll :deep(.rule-form) { display: grid; grid-template-columns: 140px 140px minmax(0, 1fr) minmax(0, 1fr) auto; gap: 7px; }
.settings-scroll :deep(.rule-list) { margin-top: 12px; border-top: 1px solid #27312a; }
.settings-scroll :deep(.rule-row) { display: flex; align-items: center; gap: 8px; min-height: 35px; border-bottom: 1px solid #27312a; color: #c0cbc2; font-size: 11px; }
.settings-scroll :deep(.rule-row .mono) { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.settings-scroll :deep(.tiny-button) { width: 27px; height: 27px; margin-left: auto; }
.settings-scroll :deep(.empty-line) { padding: 16px 0; color: var(--muted); font-size: 12px; }
.settings-scroll :deep(.data-section) { display: flex; justify-content: space-between; align-items: center; gap: 18px; }
.settings-scroll :deep(.data-section > div:first-child) { min-width: 0; }
.settings-scroll :deep(.data-section .muted) { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.settings-scroll :deep(.model-hint) { margin-top: 14px; line-height: 1.5; }
.settings-scroll :deep(.status) { display: inline-flex; align-items: center; gap: 6px; color: #929d95; font-size: 11px; }
.settings-scroll :deep(.status span) { width: 7px; height: 7px; border-radius: 50%; background: #59635c; }
.settings-scroll :deep(.status.on) { color: #a7d7b1; }
.settings-scroll :deep(.status.on span) { background: #78c78b; box-shadow: 0 0 8px #78c78b; }
.settings-scroll :deep(.server-url) { margin-bottom: 14px; padding: 10px; color: #a8d8b4; background: #1b2b20; border: 1px solid #34533d; border-radius: 5px; overflow-wrap: anywhere; }
.settings-scroll :deep(.wide-label) { margin-top: 4px; }
.settings-scroll :deep(.exclude-label) { margin-top: 14px; }
.settings-scroll :deep(.log-toolbar) { margin-bottom: 10px; }
.settings-scroll :deep(.log-toolbar select) { min-width: 150px; }
.settings-scroll :deep(.logs) { border-top: 1px solid #27312a; }
.settings-scroll :deep(.log-row) { display: grid; grid-template-columns: 82px 60px minmax(0, 1fr); gap: 10px; align-items: baseline; min-height: 35px; padding: 8px 0; border-bottom: 1px solid #27312a; color: #bdc8bf; font-size: 11px; }
.settings-scroll :deep(.log-time) { color: #748078; }
.settings-scroll :deep(.log-kind) { color: #9eb7a3; }
.settings-scroll :deep(.log-kind.error) { color: #f29f97; }
/* Keep the settings surface aligned with the flat white/black shell. */
.settings-sidebar { background: #fff; }
.settings-subtitle, .settings-main-head p, .settings-sidebar-foot { color: #666; }
.settings-nav-item { color: #444; border-radius: 2px; }
.settings-nav-item:hover { color: #111; background: #f0f0f0; border-color: #111; }
.settings-nav-item.active { color: #fff; background: #111; border-color: #111; }
.settings-scroll :deep(.settings-section) { background: #fff; border-radius: 2px; }
.settings-scroll :deep(.settings-section) { border-color: #111; }
.settings-scroll :deep(.section-head h2) { color: #111; }
.settings-scroll :deep(.section-head p), .settings-scroll :deep(.settings-section > div > p), .settings-scroll :deep(label) { color: #555; }
.settings-scroll :deep(textarea) { background: #fff; border-color: #111; border-radius: 2px; color: #111; }
.settings-scroll :deep(.save-button) { color: #fff; background: #111; border-color: #111; border-radius: 2px; }
.settings-scroll :deep(.save-button:hover) { background: #333; border-color: #333; }
.settings-scroll :deep(.secondary-button) { background: #fff; border-color: #111; color: #111; border-radius: 2px; }
.settings-scroll :deep(.danger-button) { background: #fff; border-color: #9b2923; color: #9b2923; border-radius: 2px; }
.settings-scroll :deep(.rule-row), .settings-scroll :deep(.log-row) { border-color: #ddd; color: #222; }
.settings-scroll :deep(.rule-list), .settings-scroll :deep(.logs) { border-color: #111; }
.settings-scroll :deep(.switch-row label) { color: #222; }
.settings-scroll :deep(.switch-row input) { accent-color: #111; }
.settings-scroll :deep(input), .settings-scroll :deep(select) { background: #fff; border-color: #111; border-radius: 2px; color: #111; }
.settings-scroll :deep(input::placeholder), .settings-scroll :deep(textarea::placeholder) { color: #777; }
.settings-scroll :deep(.model-hint), .settings-scroll :deep(.empty-line) { color: #555; }
.settings-scroll :deep(.status) { color: #555; }
.settings-scroll :deep(.status.on) { color: #111; }
.settings-scroll :deep(.status.on span) { background: #111; box-shadow: none; }
.settings-scroll :deep(.server-url) { color: #111; background: #f7f7f7; border-color: #111; border-radius: 2px; }
.settings-scroll :deep(.log-time), .settings-scroll :deep(.log-kind) { color: #555; }
.settings-scroll :deep(.log-row > span:last-child) { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
@media (max-width: 820px) {
  .settings-sidebar { width: 172px; }
  .settings-main-head, .settings-scroll { padding-inline: 18px; }
  .settings-scroll :deep(.form-grid.three) { grid-template-columns: repeat(2, minmax(0, 1fr)); }
  .settings-scroll :deep(.rule-form) { grid-template-columns: 1fr 1fr; }
  .settings-scroll :deep(.rule-form input), .settings-scroll :deep(.rule-form button) { grid-column: span 2; }
}
@media (max-width: 600px) {
  .settings-sidebar { width: 54px; }
  .settings-sidebar-head { justify-content: center; padding-inline: 8px; }
  .settings-title, .settings-subtitle, .settings-nav-item span, .settings-sidebar-foot { display: none; }
  .settings-nav { padding-inline: 7px; }
  .settings-nav-item { justify-content: center; padding-inline: 0; }
  .settings-scroll :deep(.form-grid.three) { grid-template-columns: 1fr; }
  .settings-scroll :deep(.data-section) { align-items: flex-start; flex-direction: column; }
  .settings-scroll :deep(.embedded-view) { height: auto; }
}
</style>
