<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { RefreshCw } from "lucide-vue-next";
import type { Activity } from "../api";
import { t } from "../i18n";

const props = defineProps<{
  logs: Activity[];
  logKind: string;
  refreshLogs: () => Promise<void>;
}>();

const emit = defineEmits<{
  (event: "update-log-kind", value: string): void;
}>();

const kind = computed({
  get: () => props.logKind,
  set: (value: string) => emit("update-log-kind", value),
});

type TimingEntry = {
  id: number;
  ts: number;
  file: string;
  stage: string;
  elapsedMs: number;
  progress?: number;
  message: string;
};

const timingOpen = ref(false);

function parseTiming(entry: Activity): TimingEntry | null {
  if (entry.kind !== "perf" && entry.kind !== "index_timing" && !entry.msg.includes("index_timing|")) return null;
  const raw = entry.msg.startsWith("index_timing|")
    ? entry.msg.slice("index_timing|".length)
    : entry.msg.includes("index_timing|")
      ? entry.msg.slice(entry.msg.indexOf("index_timing|") + "index_timing|".length)
      : entry.msg;
  const values: Record<string, string> = {};
  for (const part of raw.split("|")) {
    const separator = part.indexOf("=");
    if (separator > 0) values[part.slice(0, separator)] = part.slice(separator + 1);
  }
  const elapsedMs = Number(values.ms ?? values.elapsed_ms ?? values.elapsed ?? 0);
  const file = values.file || values.path || values.name || "未知文件";
  if (!Number.isFinite(elapsedMs) || elapsedMs < 0) return null;
  let decodedFile = file;
  try { decodedFile = decodeURIComponent(file); } catch { /* preserve raw path */ }
  const progress = Number(values.progress ?? values.percent);
  return {
    id: entry.id,
    ts: entry.ts,
    file: decodedFile,
    stage: values.stage || values.phase || "处理",
    elapsedMs,
    progress: Number.isFinite(progress) ? Math.max(0, Math.min(100, progress)) : undefined,
    message: entry.msg,
  };
}

const timingEntries = computed(() => props.logs.map(parseTiming).filter((item): item is TimingEntry => Boolean(item)));

type TimingFileGroup = {
  file: string;
  totalMs: number;
  stages: TimingEntry[];
};

// A file can emit separate extraction and model timings. Group those records
// before rendering so the log answers "which file is slow" and "where" in one
// row instead of forcing the user to correlate interleaved activity entries.
const timingFiles = computed(() => {
  const groups = new Map<string, TimingFileGroup>();
  for (const entry of timingEntries.value) {
    const group = groups.get(entry.file) ?? { file: entry.file, totalMs: 0, stages: [] };
    group.stages.push(entry);
    group.totalMs += entry.elapsedMs;
    groups.set(entry.file, group);
  }
  return [...groups.values()].sort((a, b) => b.totalMs - a.totalMs);
});
const timingTotalMs = computed(() => timingFiles.value.reduce((total, item) => total + item.totalMs, 0));
const timingOverview = computed(() => timingFiles.value.map((group) => ({
  ...group,
  width: timingTotalMs.value > 0 ? (group.totalMs / timingTotalMs.value) * 100 : 0,
  slowestStage: group.stages.reduce<TimingEntry | null>((slowest, item) => (
    !slowest || item.elapsedMs > slowest.elapsedMs ? item : slowest
  ), null),
})));
const recentLogs = computed(() => props.logs
  .filter((entry) => !entry.msg.includes("index_timing|") && entry.kind !== "perf" && entry.kind !== "index_timing")
  .slice(0, 12));

let refreshTimer: ReturnType<typeof setInterval> | undefined;
let refreshing = false;
async function refreshWhileOpen() {
  if (refreshing) return;
  refreshing = true;
  try { await props.refreshLogs(); } finally { refreshing = false; }
}
onMounted(() => {
  // Activity records are persisted by the backend while an index is running.
  // Refresh only while this tab is mounted so the log progress does not add
  // polling or database work to the search and index views.
  refreshTimer = setInterval(refreshWhileOpen, 1500);
});
onBeforeUnmount(() => {
  if (refreshTimer) clearInterval(refreshTimer);
  refreshTimer = undefined;
});

function formatElapsed(ms: number) {
  if (ms < 1000) return `${Math.round(ms)} ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(1)} s`;
  return `${Math.floor(seconds / 60)}分${Math.floor(seconds % 60)}秒`;
}

function timingColor(file: string) {
  let hash = 0;
  for (let index = 0; index < file.length; index += 1) hash = (hash * 31 + file.charCodeAt(index)) | 0;
  return `hsl(${Math.abs(hash) % 360} 66% 42%)`;
}
</script>

<template>
  <section class="settings-section log-section settings-log-tab">
    <div class="section-head">
      <div><h2>{{ t("活动日志") }}</h2><p>{{ t("记录扫描、索引、模型和服务状态。") }}</p></div>
      <button class="secondary-button" @click="props.refreshLogs"><RefreshCw :size="14" />{{ t("刷新") }}</button>
    </div>
    <div class="log-toolbar">
      <select v-model="kind" @change="props.refreshLogs">
        <option value="">{{ t("全部") }}</option>
        <option value="scan">{{ t("扫描") }}</option>
        <option value="index">{{ t("索引") }}</option>
        <option value="model">{{ t("模型") }}</option>
        <option value="op">{{ t("文件操作") }}</option>
        <option value="watch">{{ t("监控") }}</option>
        <option value="server">{{ t("局域网") }}</option>
        <option value="perf">{{ t("性能") }}</option>
        <option value="error">{{ t("错误") }}</option>
      </select>
    </div>
    <section v-if="timingEntries.length" class="timing-panel">
      <button class="timing-summary" type="button" :aria-expanded="timingOpen" @click="timingOpen = !timingOpen">
        <span class="timing-summary-title">索引耗时</span>
        <span v-if="timingFiles[0]" class="timing-summary-detail">
          最慢：{{ timingFiles[0].file }} · {{ timingOverview[0]?.slowestStage?.stage }} · {{ formatElapsed(timingFiles[0].totalMs) }}
        </span>
        <span class="timing-summary-chevron" :class="{ open: timingOpen }">⌄</span>
      </button>
      <div class="timing-overview" role="img" aria-label="索引文件耗时分布">
        <span
          v-for="group in timingOverview"
          :key="group.file"
          class="timing-overview-segment"
          :title="`${group.file} · ${formatElapsed(group.totalMs)}`"
          :style="{ width: `${Math.max(group.width, timingOverview.length > 1 ? 0.8 : 100 / Math.max(timingOverview.length, 1))}%`, backgroundColor: timingColor(group.file) }"
        />
      </div>
      <div v-if="timingOpen" class="timing-list">
        <div v-for="group in timingOverview" :key="group.file" class="timing-file-group">
          <span class="timing-color" :style="{ backgroundColor: timingColor(group.file) }" />
          <div class="timing-main">
            <div class="timing-file" :title="group.file">{{ group.file }}</div>
            <div class="timing-stage-list">
              <span v-for="stage in group.stages" :key="stage.id" class="timing-stage-detail">
                {{ stage.stage }} {{ formatElapsed(stage.elapsedMs) }}
              </span>
            </div>
          </div>
          <span class="timing-duration mono">{{ formatElapsed(group.totalMs) }}</span>
        </div>
      </div>
    </section>
    <div class="logs">
      <div v-for="entry in recentLogs" :key="entry.id" class="log-row">
        <span class="log-time mono">{{ new Date(entry.ts * 1000).toLocaleTimeString() }}</span>
        <span class="log-kind" :class="entry.kind">{{ entry.kind }}</span>
        <span>{{ entry.msg }}</span>
      </div>
      <div v-if="!props.logs.length" class="empty-line">{{ t("暂无日志") }}</div>
    </div>
  </section>
</template>

<style scoped>
.timing-panel {
  margin: 12px 0 16px;
  border: 1px solid var(--border, #d5d5d5);
  background: var(--panel, #fff);
}
.timing-summary {
  display: flex;
  width: 100%;
  align-items: center;
  gap: 10px;
  min-height: 36px;
  padding: 8px 10px;
  border: 0;
  color: var(--text, #111);
  background: transparent;
  text-align: left;
  cursor: pointer;
}
.timing-summary-title { font-weight: 600; }
.timing-summary-detail {
  min-width: 0;
  flex: 1;
  overflow: hidden;
  color: var(--muted, #666);
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 11px;
}
.timing-summary-chevron { margin-left: auto; transition: transform .16s ease; }
.timing-summary-chevron.open { transform: rotate(180deg); }
.timing-overview {
  display: flex;
  height: 9px;
  margin: 0 10px 10px;
  overflow: hidden;
  border-radius: 3px;
  background: color-mix(in srgb, var(--muted, #999) 18%, transparent);
}
.timing-overview-segment {
  display: block;
  min-width: 1px;
  height: 100%;
  transition: width .2s ease;
}
.timing-list { border-top: 1px solid var(--border, #d5d5d5); }
.timing-file-group {
  display: grid;
  grid-template-columns: 8px minmax(0, 1fr) 70px;
  align-items: center;
  gap: 8px;
  min-height: 48px;
  padding: 7px 10px;
  border-bottom: 1px solid color-mix(in srgb, var(--border, #d5d5d5) 70%, transparent);
  font-size: 11px;
}
.timing-file-group:last-child { border-bottom: 0; }
.timing-color { width: 8px; height: 25px; border-radius: 2px; }
.timing-main { min-width: 0; }
.timing-file { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.timing-stage-list { display: flex; flex-wrap: wrap; gap: 4px 8px; margin-top: 5px; color: var(--muted, #666); font-size: 10px; }
.timing-stage-detail { white-space: nowrap; }
.timing-duration { text-align: right; white-space: nowrap; }
.recent-logs { max-height: 260px; overflow: auto; }
@media (max-width: 650px) {
  .timing-file-group { grid-template-columns: 8px minmax(0, 1fr) 60px; }
}
</style>

