<script setup lang="ts">
// 图形与 GPU 设置页（REQ-093 / REQ-094 / REQ-097 / REQ-098 / REQ-103 / REQ-104 / REQ-105 / REQ-106 / REQ-108）。
//
// 这一页只做两件事：把后端探测结果如实摆给用户看，以及提供「猜不准就手动改」
// 的开关。所有硬件探测都在 Rust 侧完成（ai/gpu.rs），前端不重复实现一遍，
// 否则两边结论不一致时用户根本不知道该信谁。
import { computed, onMounted, ref } from "vue";
import { Activity, Cpu, Gauge, RefreshCw, Sparkles } from "lucide-vue-next";
import { api, type AppConfig, type GpuDevice } from "../api";
import { store, toast } from "../store";
import { t } from "../i18n";
import { QUALITY_PROFILES, type QualityPreset } from "../galaxy/capability";
import { notifyRenderSettingsChanged, renderSettings, type BackendPreference } from "../galaxy/renderSettings";

const props = defineProps<{ cfg?: AppConfig | null; saveModelConfig?: () => Promise<boolean | void> }>();
const devices = ref<GpuDevice[]>([]);
const vulkanAvailable = ref(false);
const selectedDevice = computed(() => props.cfg?.model?.gpu_device || "auto");
const loading = ref(false);
const changingDevice = ref(false);

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

function fmtMb(mb: number): string {
  if (!mb) return "0";
  return mb >= 1024 ? (mb / 1024).toFixed(1) + " GB" : mb + " MB";
}
async function refresh() {
  loading.value = true;
  try {
    const [detectedDevices, report] = await Promise.all([
      api.gpuDevices(),
      api.gpuReport().catch(() => null),
    ]);
    devices.value = detectedDevices.sort((a, b) => Number(a.integrated) - Number(b.integrated) || b.total_memory - a.total_memory);
    vulkanAvailable.value = Boolean(report?.backends.some((backend) => backend.name.toLowerCase().includes("vulkan")));
  } catch (error) {
    toast(t("读取 GPU 设备失败：") + error, "error");
  } finally {
    loading.value = false;
  }
}

async function chooseDevice(id: string) {
  if (!props.cfg || changingDevice.value) return;
  const previous = props.cfg.model.gpu_device || "auto";
  if (previous === id) return;
  changingDevice.value = true;
  try {
    const activeModel = await api.modelInfo().catch(() => null);
    props.cfg.model.gpu_device = id;
    const saved = props.saveModelConfig ? await props.saveModelConfig() : true;
    if (saved === false) {
      props.cfg.model.gpu_device = previous;
      return;
    }
    if (activeModel?.ready && activeModel.backend !== "hash") {
      const modelPath = props.cfg.model.embed_model.trim() || activeModel.model;
      const reloaded = await api.modelLoad("auto", modelPath || undefined);
      if (!reloaded.ready) throw new Error(reloaded.detail || t("模型未能在所选 GPU 上加载"));
      store.model = reloaded;
    }
  } catch (error) {
    props.cfg.model.gpu_device = previous;
    await props.saveModelConfig?.();
    toast(t("保存 GPU 设备失败：") + error, "error");
  } finally {
    changingDevice.value = false;
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
      </div>
    </div>

    <article class="gpu-card">
      <header class="gpu-card-head"><Cpu :size="15" /><strong>{{ t("GPU 硬件选择") }}</strong></header>
      <div class="hardware-grid">
        <button class="hardware-button" :class="{ on: selectedDevice === 'auto' }" :aria-pressed="selectedDevice === 'auto'" :disabled="changingDevice" @click="chooseDevice('auto')">
          <strong>{{ t("自动选择") }}</strong><small>{{ t("优先独立显卡，按可用推理后端适配") }}</small>
        </button>
        <button v-for="device in devices" :key="device.id" class="hardware-button" :class="{ on: selectedDevice === device.id }" :aria-pressed="selectedDevice === device.id" :disabled="changingDevice" @click="chooseDevice(device.id)">
          <strong>{{ device.name }}</strong><small>{{ device.backend }} · {{ fmtMb(Math.round(device.total_memory / 1024 / 1024)) }}{{ device.integrated ? ` · ${t("核显")}` : '' }}</small>
        </button>
      </div>
      <div v-if="!devices.length" class="gpu-empty">
        <template v-if="loading">{{ t("正在检测硬件…") }}</template>
        <template v-else>
          <strong>{{ t("未检测到可选 GPU") }}</strong>
          <small>{{ t("列表来自当前编译的推理后端；核显和 AMD 显卡需要 Vulkan 构建及对应驱动。") }}</small>
          <small v-if="!vulkanAvailable">{{ t("当前构建未包含 Vulkan，无法列出核显或 AMD 显卡。") }}</small>
          <small v-else>{{ t("当前构建包含 Vulkan；请检查显卡 Vulkan 驱动是否正常。") }}</small>
        </template>
      </div>
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
.gpu-empty > strong, .gpu-empty > small { display: block; }
.gpu-empty > strong { margin-bottom: 4px; color: #111; font-size: 11px; }
.gpu-empty > small { margin-top: 3px; line-height: 1.5; }
.gpu-kv { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 6px 16px; margin: 0; }
.gpu-kv > div { display: flex; gap: 8px; min-width: 0; font-size: 11px; }
.gpu-kv dt { color: #777; white-space: nowrap; }
.gpu-kv dd { margin: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.gpu-choice { display: flex; flex-wrap: wrap; gap: 6px; }
.hardware-grid { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 8px; }
.hardware-button { min-height: 74px; padding: 9px; border: 1px solid #111; background: #fff; color: #111; text-align: left; cursor: pointer; }
.hardware-button strong { display: block; overflow-wrap: anywhere; font-size: 11px; line-height: 1.35; }
.hardware-button small { display: block; margin-top: 5px; overflow-wrap: anywhere; color: #666; font-size: 10px; }
.hardware-button.on { background: #111; color: #fff; }
.hardware-button.on small { color: #ddd; }
.gpu-choice.inline { margin-left: auto; }
.gpu-choice.wrap { margin-top: 12px; }
.gpu-chip { padding: 6px 10px; border: 1px solid #111; border-radius: 2px; background: #fff; color: #111; font-size: 11px; cursor: pointer; }
.gpu-chip:hover { background: #f2f2f2; }
.gpu-chip.on { background: #111; color: #fff; }
.gpu-hint-line { margin: 8px 0 0; color: #777; font-size: 10px; line-height: 1.6; }
.gpu-row { display: flex; align-items: center; gap: 8px; margin-top: 10px; font-size: 11px; }
.gpu-row select { min-width: 96px; padding: 4px 6px; border: 1px solid #111; border-radius: 2px; background: #fff; font-size: 11px; }
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
