<script setup lang="ts">
// 图形与 GPU 设置页（REQ-093 / REQ-094 / REQ-097 / REQ-098 / REQ-103 / REQ-104 / REQ-105 / REQ-106 / REQ-108）。
//
// 这一页只做两件事：把后端探测结果如实摆给用户看，以及提供「猜不准就手动改」
// 的开关。所有硬件探测都在 Rust 侧完成（ai/gpu.rs），前端不重复实现一遍，
// 否则两边结论不一致时用户根本不知道该信谁。
import { computed, onMounted, ref } from "vue";
import { Activity, AlertTriangle, Copy, Cpu, Gauge, RefreshCw, ShieldCheck, Sparkles, Trash2 } from "lucide-vue-next";
import { api, type GpuReport } from "../api";
import { toast } from "../store";
import { t } from "../i18n";
import { QUALITY_PROFILES, webglSupport, type QualityPreset } from "../galaxy/capability";
import { notifyRenderSettingsChanged, renderSettings, type BackendPreference } from "../galaxy/renderSettings";
import { crashLog, crashLogText, clearCrashLog } from "../galaxy/telemetry";

const report = ref<GpuReport | null>(null);
// 渲染侧（WebGL）的探测结果，和 Rust 侧推理后端是两回事，分开显示。
const support = ref(webglSupport());
const loading = ref(false);
const busy = ref(false);
const reportPath = ref("");

const QUALITY_KEYS: QualityPreset[] = ["low", "medium", "high", "ultra"];

const BACKENDS: Array<{ key: BackendPreference; label: string; hint: string }> = [
  { key: "auto", label: "自动检测", hint: "按硬件与驱动情况自行选择，推荐。" },
  { key: "webgl", label: "强制 WebGL", hint: "显卡没问题但被自动降级时可以试试。" },
  { key: "canvas", label: "强制 Canvas 2D", hint: "花屏、闪烁或驱动崩溃时的保底方案。" },
];

const DPR_OPTIONS = [
  { value: 0, label: "自动" },
  { value: 1, label: "1x" },
  { value: 2, label: "2x" },
  { value: 3, label: "3x" },
];

const STATUS_LABEL: Record<string, string> = {
  ready: "GPU 加速可用",
  degraded: "部分加速",
  cpu_only: "纯 CPU 运行",
};

const statusText = computed(() => {
  const r = report.value;
  if (!r) return "";
  return STATUS_LABEL[r.status] || r.status;
});

const tierText = computed(() => {
  const tier = report.value?.tier ?? 3;
  if (tier === 0) return "Tier 0 · 显存充足，模型可全量下放 GPU";
  if (tier === 1) return "Tier 1 · 建议使用量化模型";
  if (tier === 2) return "Tier 2 · 小模型或部分下放";
  return "Tier 3 · 显存不足，回落 CPU";
});

// 屏幕像素比必须从 window 取：Vue 模板里直接写 devicePixelRatio 会被解析成
// _ctx.devicePixelRatio（组件实例上没有这个属性），拿到 undefined 后调
// .toFixed() 直接抛错，整个设置页会变成一片空白。
const screenDpr = computed(() => (typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1));

const vramPercent = computed(() => {
  const r = report.value;
  if (!r || !r.vram_mb) return 0;
  const used = Math.max(0, r.vram_mb - r.vram_free_mb);
  return Math.round((used / r.vram_mb) * 100);
});

function fmtMb(mb: number): string {
  if (!mb) return "0";
  return mb >= 1024 ? (mb / 1024).toFixed(1) + " GB" : mb + " MB";
}

async function refresh() {
  loading.value = true;
  try {
    // 点「重新检测」时把 WebGL 探测也重跑一次（REQ-105：换了驱动/外接显卡后
    // 不该要求用户重启程序才能重新识别）。
    support.value = webglSupport(true);
    report.value = await api.gpuReport();
  } catch (error) {
    toast(t("读取 GPU 报告失败：") + error, "error");
  } finally {
    loading.value = false;
  }
}

async function writeReport() {
  busy.value = true;
  try {
    reportPath.value = await api.gpuRefreshReport();
    toast(t("已写入 ") + reportPath.value, "ok");
  } catch (error) {
    toast(t("写入诊断日志失败：") + error, "error");
  } finally {
    busy.value = false;
  }
}

function setQuality(key: QualityPreset) {
  if (renderSettings.quality === key) return;
  renderSettings.quality = key;
  notifyRenderSettingsChanged();
  toast(t("画质已切换到：") + QUALITY_PROFILES[key].label, "ok");
}

function setBackend(key: BackendPreference) {
  if (renderSettings.backend === key) return;
  renderSettings.backend = key;
  notifyRenderSettingsChanged();
  toast(t("渲染后端已切换，图谱画布会自行重建。"), "ok");
}

function setDpr(value: number) {
  renderSettings.dprOverride = value;
  notifyRenderSettingsChanged();
}

// ---- 三维数据星图（《2D转3D与星图需求明细》#1~#33）---------------------
// 这些开关会立刻作用到已经打开的星图：引擎侧 setBloom/setSketch/... 是
// 就地改 pass 的 enabled，不重建 composer，所以切换不会黑屏。
function setDim(v: "2d" | "3d") {
  if (renderSettings.dim === v) return;
  renderSettings.dim = v;
  notifyRenderSettingsChanged();
}

function toggleStar(key: "deepSpace" | "bloom" | "sketch" | "autoDegrade") {
  renderSettings[key] = !renderSettings[key];
  notifyRenderSettingsChanged();
}

async function copyCrashes() {
  const text = crashLogText();
  try {
    await navigator.clipboard.writeText(text);
    toast(t("崩溃记录已复制到剪贴板"), "ok");
  } catch {
    toast(t("复制失败，请手动选中下方文本"), "warn");
  }
}

function clearCrashes() {
  clearCrashLog();
  toast(t("崩溃记录已清空"), "ok");
}

onMounted(refresh);
</script>

<template>
  <section class="settings-section gpu-section">
    <div class="section-head">
      <div>
        <h2>{{ t("图形与 GPU") }}</h2>
        <p>{{ t("图谱的渲染后端、画质档位，以及本机 GPU 与驱动的兼容性诊断。") }}</p>
      </div>
      <div class="gpu-actions">
        <button class="secondary-button" :disabled="loading" @click="refresh"><RefreshCw :size="14" />{{ t("重新检测") }}</button>
        <button class="secondary-button" :disabled="busy" @click="writeReport"><ShieldCheck :size="14" />{{ t("写入诊断日志") }}</button>
      </div>
    </div>

    <!-- 一、GPU 能力报告（REQ-093 / REQ-097） -->
    <article class="gpu-card">
      <header class="gpu-card-head">
        <Cpu :size="15" />
        <strong>{{ t("GPU 能力报告") }}</strong>
        <span v-if="report" class="gpu-state" :class="report.status">{{ statusText }}</span>
      </header>
      <div v-if="!report" class="gpu-empty">{{ loading ? t("正在检测硬件…") : t("暂无数据") }}</div>
      <template v-else>
        <dl class="gpu-kv">
          <div><dt>{{ t("显卡") }}</dt><dd>{{ report.gpu_name || t("未检测到独立显卡") }}</dd></div>
          <div><dt>{{ t("推理后端") }}</dt><dd class="mono">{{ report.backend }}</dd></div>
          <div><dt>{{ t("显存") }}</dt><dd class="mono">{{ fmtMb(report.vram_mb - report.vram_free_mb) }} / {{ fmtMb(report.vram_mb) }}</dd></div>
          <div><dt>{{ t("驱动版本") }}</dt><dd class="mono">{{ report.driver_version || t("未知") }}</dd></div>
          <div><dt>{{ t("CUDA 工具包") }}</dt><dd class="mono">{{ report.cuda_version || (report.cuda_hint ? t("未安装 · 建议 ") + report.cuda_hint : t("未安装（本版使用 Vulkan/CPU 后端）")) }}</dd></div>
          <div><dt>{{ t("显存分档") }}</dt><dd>{{ tierText }}</dd></div>
          <div>
            <dt>ONNX EP</dt>
            <dd class="mono" :title="report.ort_present ? report.ort_providers.join(' → ') : ''">
              {{ report.ort_present ? report.ort_providers.join(" → ") : t("未安装跨模态扩展（可选）") }}
            </dd>
          </div>
        </dl>
        <div class="gpu-bar" :title="t('显存占用')">
          <i :style="{ width: vramPercent + '%' }"></i>
          <span>{{ vramPercent }}%</span>
        </div>
        <p class="gpu-reason">{{ report.reason }}</p>
        <ul v-if="report.notes.length" class="gpu-notes">
          <li v-for="(note, i) in report.notes" :key="i">{{ note }}</li>
        </ul>
        <div v-if="reportPath" class="gpu-path mono">{{ reportPath }}</div>
      </template>
    </article>

    <!-- 二、画质档位（REQ-108） -->
    <article class="gpu-card">
      <header class="gpu-card-head">
        <Gauge :size="15" />
        <strong>{{ t("画质档位") }}</strong>
        <span class="gpu-hint">{{ t("影响光晕、曲线连线、背景星点与渲染分辨率") }}</span>
      </header>
      <div class="gpu-choice">
        <button
          v-for="key in QUALITY_KEYS"
          :key="key"
          class="gpu-chip"
          :class="{ on: renderSettings.quality === key }"
          @click="setQuality(key)"
        >{{ t(QUALITY_PROFILES[key].label) }}</button>
      </div>
      <p class="gpu-hint-line">
        {{ t("节点上限") }}: {{ QUALITY_PROFILES[renderSettings.quality].maxNodes.toLocaleString() }} ·
        {{ t("渲染倍率上限") }}: {{ QUALITY_PROFILES[renderSettings.quality].maxDpr }}x
      </p>
    </article>

    <!-- 三、渲染后端与分辨率（REQ-103 / REQ-104 / REQ-106） -->
    <article class="gpu-card">
      <header class="gpu-card-head">
        <Activity :size="15" />
        <strong>{{ t("渲染后端") }}</strong>
        <span class="gpu-hint">{{ t("切换后已打开的图谱会自动重建") }}</span>
      </header>
      <div class="gpu-choice">
        <button
          v-for="b in BACKENDS"
          :key="b.key"
          class="gpu-chip"
          :class="{ on: renderSettings.backend === b.key }"
          :title="t(b.hint)"
          @click="setBackend(b.key)"
        >{{ t(b.label) }}</button>
      </div>
      <div class="gpu-row">
        <span>{{ t("渲染分辨率") }}</span>
        <select :value="renderSettings.dprOverride" @change="setDpr(Number(($event.target as HTMLSelectElement).value))">
          <option v-for="o in DPR_OPTIONS" :key="o.value" :value="o.value">{{ t(o.label) }}</option>
        </select>
      </div>
      <p class="gpu-hint-line">
        {{ t("「自动」会按画质档位与后端取上限；高分屏上手动降到 1x 能把 GPU 占用压下来。") }}
      </p>
    </article>

    <!-- 二·五、三维星图（《2D转3D与星图需求明细》#1~#33） -->
    <article class="gpu-card">
      <header class="gpu-card-head">
        <Sparkles :size="15" />
        <strong>{{ t("三维数据星图") }}</strong>
        <span class="gpu-hint">{{ t("深空场景、辉光与铅绘滤镜，掉帧时会自动降级") }}</span>
      </header>

      <div class="gpu-row">
        <span>{{ t("默认视图") }}</span>
        <div class="gpu-choice inline">
          <button class="gpu-chip" :class="{ on: renderSettings.dim === '2d' }" @click="setDim('2d')">
            {{ t("二维星系图") }}
          </button>
          <button class="gpu-chip" :class="{ on: renderSettings.dim === '3d' }" @click="setDim('3d')">
            {{ t("三维数据星图") }}
          </button>
        </div>
      </div>

      <div class="gpu-choice wrap">
        <button class="gpu-chip" :class="{ on: renderSettings.deepSpace }" @click="toggleStar('deepSpace')">
          {{ t("深空星尘") }}
        </button>
        <button class="gpu-chip" :class="{ on: renderSettings.bloom }" @click="toggleStar('bloom')">
          {{ t("辉光 Bloom") }}
        </button>
        <button class="gpu-chip" :class="{ on: renderSettings.sketch }" @click="toggleStar('sketch')">
          {{ t("铅绘滤镜") }}
        </button>
        <button class="gpu-chip" :class="{ on: renderSettings.autoDegrade }" @click="toggleStar('autoDegrade')">
          {{ t("低帧率自动降级") }}
        </button>
      </div>

      <p class="gpu-hint-line">
        {{ t("10000 个节点仍在 10 个绘制批次以内；铅绘滤镜以 50% 分辨率绘制并复用到全屏。") }}
        {{
          t(
            "关闭深空星尘能省下约 3000 个粒子的绘制开销，老显卡上更稳。",
          )
        }}
      </p>
    </article>

    <!-- 三·五、渲染侧的实际能力（REQ-104 / REQ-105 / REQ-106） -->
    <article class="gpu-card">
      <header class="gpu-card-head">
        <Activity :size="15" />
        <strong>{{ t("渲染能力") }}</strong>
        <span class="gpu-state" :class="support.level === 'none' ? 'cpu_only' : 'ready'">
          {{ support.level === "webgl2" ? "WebGL 2" : support.level === "webgl1" ? "WebGL 1" : "Canvas 2D" }}
        </span>
      </header>
      <dl class="gpu-kv">
        <div><dt>{{ t("显卡渲染器") }}</dt><dd :title="support.renderer">{{ support.renderer || t("未报告") }}</dd></div>
        <div><dt>{{ t("着色器精度") }}</dt><dd class="mono">{{ support.precision }}</dd></div>
        <div><dt>{{ t("最大纹理") }}</dt><dd class="mono">{{ support.maxTextureSize || "-" }} px</dd></div>
        <div><dt>{{ t("顶点属性") }}</dt><dd class="mono">{{ support.maxVertexAttribs || "-" }}</dd></div>
        <div><dt>{{ t("浮点混合") }}</dt><dd class="mono">{{ support.floatBlend ? t("支持") : t("不支持") }}</dd></div>
        <div><dt>{{ t("屏幕像素比") }}</dt><dd class="mono">{{ screenDpr.toFixed(2) }}x</dd></div>
      </dl>
      <ul v-if="support.patches.length" class="gpu-notes">
        <li v-for="p in support.patches" :key="p.id">{{ p.detail }}</li>
      </ul>
      <p v-else class="gpu-hint-line">{{ t("没有命中已知的驱动兼容问题。") }}</p>
    </article>

    <!-- 四、已编入的后端（REQ-096 / REQ-098 / REQ-099 / REQ-100 / REQ-101） -->
    <article v-if="report && report.backends.length" class="gpu-card">
      <header class="gpu-card-head">
        <Cpu :size="15" />
        <strong>{{ t("已编入二进制文件的后端") }}</strong>
      </header>
      <table class="gpu-table">
        <thead><tr><th>{{ t("后端") }}</th><th>{{ t("设备数") }}</th><th>{{ t("加速") }}</th><th>{{ t("特性") }}</th></tr></thead>
        <tbody>
          <tr v-for="b in report.backends" :key="b.name">
            <td class="mono">{{ b.name }}</td>
            <td>{{ b.device_count }}</td>
            <td>{{ b.accelerator ? t("是") : t("否") }}</td>
            <td class="mono gpu-feat">{{ b.features.map((f) => f[0] + "=" + f[1]).join(" ") || "-" }}</td>
          </tr>
        </tbody>
      </table>
    </article>

    <!-- 五、兼容性诊断与崩溃记录（REQ-105 / REQ-108） -->
    <article class="gpu-card">
      <header class="gpu-card-head">
        <AlertTriangle :size="15" />
        <strong>{{ t("兼容性诊断") }}</strong>
        <div class="gpu-head-tools">
          <button class="mini" :disabled="!crashLog.length" @click="copyCrashes"><Copy :size="12" />{{ t("复制") }}</button>
          <button class="mini" :disabled="!crashLog.length" @click="clearCrashes"><Trash2 :size="12" />{{ t("清空") }}</button>
        </div>
      </header>
      <p class="gpu-hint-line">
        {{ t("只记录渲染相关的显卡与驱动信息，不记录文件名、路径或任何文件内容。") }}
      </p>
      <div v-if="!crashLog.length" class="gpu-empty">{{ t("暂无异常记录，渲染一直很稳定。") }}</div>
      <ul v-else class="gpu-crash">
        <li v-for="(e, i) in crashLog.slice().reverse()" :key="i">
          <span class="mono gpu-crash-kind">{{ e.kind }}</span>
          <span>{{ e.detail }}</span>
          <span class="gpu-crash-meta mono">{{ new Date(e.ts).toLocaleString() }}</span>
        </li>
      </ul>
    </article>
  </section>
</template>

<style scoped>
.gpu-section { display: flex; flex-direction: column; gap: 12px; }
.gpu-actions { display: flex; flex-wrap: wrap; gap: 7px; }
.gpu-card { padding: 13px 14px; border: 1px solid #111; background: #fff; }
.gpu-card-head { display: flex; align-items: center; gap: 7px; margin-bottom: 10px; }
.gpu-card-head strong { font-size: 12px; }
.gpu-state { margin-left: 4px; padding: 1px 6px; border: 1px solid #111; border-radius: 2px; font-size: 10px; }
.gpu-state.ready { background: #111; color: #fff; }
.gpu-state.degraded { background: #fff; color: #8c6410; border-color: #8c6410; }
.gpu-state.cpu_only { background: #fff; color: #777; border-color: #bbb; }
.gpu-hint { margin-left: auto; color: #777; font-size: 10px; }
.gpu-head-tools { margin-left: auto; display: flex; gap: 5px; }
.gpu-empty { padding: 10px 0; color: #777; font-size: 11px; }
.gpu-kv { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 6px 16px; margin: 0; }
.gpu-kv > div { display: flex; gap: 8px; min-width: 0; font-size: 11px; }
.gpu-kv dt { color: #777; white-space: nowrap; }
.gpu-kv dd { margin: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.gpu-bar { position: relative; height: 14px; margin: 10px 0 6px; border: 1px solid #111; background: #fff; }
.gpu-bar i { display: block; height: 100%; background: #111; }
.gpu-bar span { position: absolute; inset: 0; display: grid; place-items: center; font-size: 9px; color: #111; mix-blend-mode: difference; font-variant-numeric: tabular-nums; }
.gpu-reason { margin: 6px 0 0; color: #333; font-size: 11px; line-height: 1.6; }
.gpu-notes { margin: 6px 0 0; padding-left: 16px; color: #555; font-size: 11px; line-height: 1.7; }
.gpu-path { margin-top: 8px; color: #555; font-size: 10px; word-break: break-all; }
.gpu-choice { display: flex; flex-wrap: wrap; gap: 6px; }
.gpu-choice.inline { margin-left: auto; }
.gpu-choice.wrap { margin-top: 12px; }
.gpu-chip { padding: 6px 10px; border: 1px solid #111; border-radius: 2px; background: #fff; color: #111; font-size: 11px; cursor: pointer; }
.gpu-chip:hover { background: #f2f2f2; }
.gpu-chip.on { background: #111; color: #fff; }
.gpu-hint-line { margin: 8px 0 0; color: #777; font-size: 10px; line-height: 1.6; }
.gpu-row { display: flex; align-items: center; gap: 8px; margin-top: 10px; font-size: 11px; }
.gpu-row select { min-width: 96px; padding: 4px 6px; border: 1px solid #111; border-radius: 2px; background: #fff; font-size: 11px; }
.gpu-table { width: 100%; border-collapse: collapse; font-size: 11px; }
.gpu-table th, .gpu-table td { padding: 5px 8px; border-bottom: 1px solid #e3e3e3; text-align: left; vertical-align: top; }
.gpu-table th { color: #777; font-weight: 500; }
.gpu-feat { color: #555; font-size: 10px; word-break: break-all; }
.gpu-crash { margin: 0; padding: 0; list-style: none; }
.gpu-crash li { display: grid; grid-template-columns: 96px 1fr auto; gap: 8px; padding: 6px 0; border-bottom: 1px solid #eee; font-size: 11px; align-items: start; }
.gpu-crash-kind { color: #8c6410; }
.gpu-crash-meta { color: #999; font-size: 10px; }
.mini { display: inline-flex; align-items: center; gap: 4px; padding: 3px 7px; border: 1px solid #111; border-radius: 2px; background: #fff; font-size: 10px; cursor: pointer; }
.mini:disabled { opacity: .5; cursor: default; }
@media (max-width: 520px) {
  .gpu-kv { grid-template-columns: 1fr; }
  .gpu-crash li { grid-template-columns: 1fr; }
}
</style>
