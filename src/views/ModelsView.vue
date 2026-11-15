<script setup lang="ts">
// 模型管理：列出本地 GGUF 与 safetensors，切换推理/OCR 模型并支持导入、删除。
import { computed, onMounted, ref, watch } from "vue";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { AudioLines, FileText, Film, FolderOpen, Image as ImageIcon, Save, SlidersHorizontal, ScanText } from "lucide-vue-next";
import { api, type AppConfig, type ModelEntry, type ModelInfo, type ModelImportResult, type OcrStatus } from "../api";
import { refreshStats, store, toast, humanSize } from "../store";
import { t } from "../i18n";
import { hasModelListCache, modelEntries, refreshModelList } from "../modelListCache";

const props = defineProps<{
  cfg?: AppConfig;
  saveModelConfig?: () => Promise<boolean | void>;
}>();

const list = modelEntries;
const info = ref<ModelInfo | null>(null);
const dir = ref("");
const importedDir = ref("");
const loading = ref(false);
const busy = ref("");
const filter = ref("all");
const selected = ref<ModelEntry | null>(null);
const ocrModelPath = ref("");
const ocrStatus = ref<OcrStatus | null>(null);
const roleModel = ref<ModelEntry | null>(null);
const roleDialogOpen = ref(false);

// Keep the expensive OCR runtime probe outside the view instance. Settings
// tabs are mounted/unmounted as the user navigates, so without this small
// cache every visit would synchronously start Python/Tesseract discovery.
let cachedOcrStatus: OcrStatus | null = null;
let cachedOcrStatusAt = 0;

watch(list, (entries) => {
  if (!selected.value) return;
  selected.value = entries.find((model) => model.path === selected.value?.path) || null;
});

const PARALLEL_MODES = [
  { key: "eco", label: "节能", className: "eco" },
  { key: "auto", label: "自动", className: "auto" },
  { key: "custom", label: "自定义", className: "custom" },
];

const FILTERS = [
  { v: "all", label: "全部" },
  { v: "embed", label: "嵌入" },
  { v: "chat", label: "对话" },
];
const visibleParallelModes = computed(() => PARALLEL_MODES.filter(
  (mode) => mode.key !== "custom" || !!props.cfg?.ui?.professional_mode,
));
const currentParallelMode = computed(() => {
  const mode = props.cfg?.model?.parallel_mode;
  return mode === "eco" || mode === "custom" ? mode : "auto";
});

// A GGUF mmproj is a companion file, not an embedding model. When it is
// selected, use the sibling embedding entry for capability display.
const selectedEmbedding = computed(() => {
  const candidate = selected.value?.kind === "embed" || selected.value?.kind === "projector"
    ? selected.value
    : list.value.find((model) => model.active && model.kind === "embed");
  if (!candidate) return null;
  const modelDir = candidate.model_dir.toLowerCase();
  if (candidate.kind !== "projector") return candidate;
  const sameDir = list.value.find((model) => model.kind === "embed" && model.model_dir.toLowerCase() === modelDir);
  if (sameDir) return sameDir;

  // Imported packages often place the GGUF projector and the full
  // safetensors directory under separate folders. Associate the projector
  // with the best declared multimodal embedding model by filename stem so
  // the cards explain what must be activated instead of saying "unconfigured".
  const stem = candidate.file
    .toLowerCase()
    .replace(/\.(gguf|safetensors)$/i, "")
    .replace(/^mmproj[-_]?/, "");
  const multimodal = list.value
    .filter((model) => model.kind === "embed" && model.modalities.some((item) => ["image", "audio", "video"].includes(item)))
    .map((model) => {
      const modelStem = model.file.toLowerCase().replace(/\.(gguf|safetensors)$/i, "");
      const nameMatch = modelStem === stem || modelStem.includes(stem) || stem.includes(modelStem);
      return { model, rank: (nameMatch ? 100 : 0) + (model.format === "safetensors" ? 10 : 0) };
    })
    .sort((a, b) => b.rank - a.rank || b.model.size - a.model.size);
  return multimodal[0]?.model || null;
});
const hasProjectorCompanion = computed(() => {
  if (selected.value?.kind === "projector") return true;
  const dir = selectedEmbedding.value?.model_dir.toLowerCase();
  return !!dir && list.value.some((model) => model.kind === "projector" && model.model_dir.toLowerCase() === dir);
});

async function load(forceModels = false) {
  const hadCachedList = hasModelListCache();
  loading.value = forceModels || !hadCachedList;
  try {
    const models = refreshModelList(forceModels);
    if (forceModels || !hadCachedList) await models;
    else void models.catch(() => {
      // A silent background refresh failure must not hide the cached inventory.
    });
    const refreshOcrStatus = () => api.teleocrStatus().then((status) => {
      cachedOcrStatus = status;
      cachedOcrStatusAt = Date.now();
      ocrStatus.value = status;
      ocrModelPath.value = status.model_dir || ocrModelPath.value;
      return status;
    });
    let ocrRequest: Promise<OcrStatus>;
    if (forceModels || !cachedOcrStatus) {
      ocrRequest = refreshOcrStatus();
    } else {
      // Paint the cached capability card immediately. A stale cache is
      // refreshed in the background so newly installed OCR runtimes/models
      // appear without making the settings page wait on Python discovery.
      ocrRequest = Promise.resolve(cachedOcrStatus);
      if (Date.now() - cachedOcrStatusAt > 20_000) void refreshOcrStatus().catch(() => {});
    }
    const [m, d, ocr] = await Promise.all([api.modelInfo(), api.modelDir(), ocrRequest]);
    // Keep the selected card's active flag in sync after activation/deactivation.
    // Otherwise the old object can keep reporting text as available after the
    // user has switched back to the hash fallback.
    if (selected.value) {
      selected.value = list.value.find((model) => model.path === selected.value?.path) || null;
    }
    info.value = m;
    store.model = m;
    dir.value = d;
    ocrStatus.value = ocr;
    ocrModelPath.value = ocr.model_dir || "";
  } catch (e) {
    toast(`${t("读取模型列表失败：")}${e}`, "error");
  } finally {
    loading.value = false;
  }
}

const shown = computed(() =>
  (filter.value === "all" ? list.value : list.value.filter((m) => m.kind === filter.value))
    .filter((m) => m.kind !== "projector")
);

const totalSize = computed(() => shown.value.reduce((s, m) => s + m.size, 0));

function isOcrModel(model: ModelEntry): boolean {
  return !!ocrModelPath.value && model.model_dir.toLowerCase() === ocrModelPath.value.toLowerCase();
}

function hasActiveRole(model: ModelEntry): boolean {
  return model.active || isOcrModel(model);
}

function hasMultimodalCompanion(model: ModelEntry): boolean {
  if (model.modalities.some((modality) => ["image", "audio", "video"].includes(modality))) return true;
  const modelStem = model.file.toLowerCase().replace(/\.(gguf|safetensors)$/i, "");
  return list.value.some((item) => {
    if (item.kind !== "projector") return false;
    if (item.model_dir.toLowerCase() === model.model_dir.toLowerCase()) return true;
    const projectorStem = item.file.toLowerCase().replace(/\.(gguf|safetensors)$/i, "").replace(/^mmproj[-_]?/, "");
    return modelStem === projectorStem || modelStem.includes(projectorStem) || projectorStem.includes(modelStem);
  });
}

// The backend is selected from the model contents by Rust. The UI only
// exposes whether a complete, supported model package is available.
function canActivate(model: ModelEntry): boolean {
  return model.kind === "embed" && model.files_complete && model.direct_loadable;
}

function canUseOcr(model: ModelEntry): boolean {
  // OCR is a vision-to-text pipeline. A plain safetensors embedding
  // checkpoint is not interchangeable with it, even when its weights are
  // complete. The backend repeats this validation when the worker starts.
  return model.format === "safetensors" && model.ocr_compatible;
}

/** 把模型设为当前嵌入推理模型。 */
async function activate(m: ModelEntry) {
  if (m.kind !== "embed") return toast(t("此模型不能作为嵌入推理模型加载"), "warn");
  if (!canActivate(m)) {
    const reason = !m.files_complete
      ? t("此模型缺少可识别的完整配套文件，暂时不能加载")
      : m.candidate_backends.includes("candle")
        ? t("需要 Candle CUDA 后端")
        : m.candidate_backends.includes("python")
          ? t("需要 Python 推理后端")
          : t("当前构建不支持此模型架构");
    return toast(reason, "warn");
  }
  busy.value = m.path;
  try {
    const out = await api.modelLoad("auto", m.path);
    info.value = out;
    store.model = out;
    if (out.ready) toast(`${t("已切换到")} ${m.file} · ${out.dim} ${t("维")}`, "ok");
    else toast(out.detail || t("模型加载失败"), "warn");
    await load(true);
    await refreshStats();
  } catch (e) {
    toast(`${t("切换失败：")}${e}`, "error");
  } finally {
    busy.value = "";
  }
}

async function deactivateModel(m: ModelEntry) {
  busy.value = m.path;
  try { info.value = await api.modelDeactivate(); store.model = info.value; await load(true); toast("模型已停用", "ok"); }
  catch (e) { toast(`停用模型失败：${e}`, "error"); }
  finally { busy.value = ""; }
}

async function deactivateOcrModel(m: ModelEntry) {
  busy.value = m.path;
  try {
    await api.ocrModelDeactivate();
    ocrModelPath.value = "";
    toast("OCR 模型已停用", "ok");
    await load(true);
  } catch (e) { toast(`停用 OCR 模型失败：${e}`, "error"); }
  finally { busy.value = ""; }
}

function chooseRole(m: ModelEntry) {
  roleModel.value = m;
  roleDialogOpen.value = true;
}

async function setRole(role: "embed" | "ocr") {
  const m = roleModel.value;
  roleDialogOpen.value = false;
  if (!m) return;
  if (role === "embed") return activate(m);
  if (!canUseOcr(m)) {
    toast(t("此 safetensors 模型未检测到 OCR/视觉编码器配置"), "warn");
    return;
  }
  try {
    await api.saveConfig({ model: { teleocr_model_dir: m.model_dir } });
    ocrModelPath.value = m.model_dir;
    toast("已设置为 OCR 模型", "ok");
    await load(true);
  } catch (e) { toast(`OCR 模型设置失败：${e}`, "error"); }
}
async function importFile() {
  try {
    const picked = await openDialog({
      directory: true,
      multiple: false,
      title: t("选择模型文件夹（自动识别 GGUF / safetensors）"),
    });
    if (typeof picked !== "string") return;
    busy.value = "import";
    const result: ModelImportResult = await api.modelImportFolder(picked);
    importedDir.value = result.destination;
    const loadState = result.direct_loadable
      ? t("已识别，可尝试加载")
      : !result.files_complete
        ? t("已识别，但缺少可用的完整模型文件")
        : result.candidate_backends?.includes("candle")
          ? t("已识别，当前构建需要 Candle CUDA 后端")
          : result.candidate_backends?.includes("python")
            ? t("已识别，但需要 Python 推理后端")
            : t("已识别，但当前构建不支持此模型架构");
    const components = result.components.length ? ` · ${result.components.join(" + ")}` : "";
    const architecture = result.architecture
      ? `架构：${result.architecture}${components}`
      : `分类：${result.primary_kind}`;
    toast(`${t("已导入")} ${result.copied_files} ${t("个配套文件")} · ${humanSize(result.total_bytes)} · ${architecture} · ${loadState}`, result.direct_loadable ? "ok" : "warn");
    for (const warning of result.warnings) toast(warning, "warn");
    await load(true);
  } catch (e) {
    toast(`${t("导入失败：")}${e}`, "error");
  } finally {
    busy.value = "";
  }
}

async function remove(m: ModelEntry) {
  if (m.active) return toast(t("正在使用的模型不能删除，请先切换到其他模型"), "warn");
  if (!confirm(`${t("确定删除模型文件？")}\n\n${m.file}\n${m.size_label}\n\n${t("删除后不可恢复。")}`)) return;
  busy.value = m.path;
  try {
    await api.modelDelete(m.path);
    toast(`${t("已删除")} ${m.file}`, "ok");
    await load(true);
  } catch (e) {
    toast(`${t("删除失败：")}${e}`, "error");
  } finally {
    busy.value = "";
  }
}

async function openModelDir(m: ModelEntry) {
  try {
    await api.openPath(m.model_dir);
  } catch (e) {
    toast(`${t("打开模型目录失败：")}${e}`, "error");
  }
}

async function openImportedDir() {
  try {
    await api.openPath(importedDir.value);
  } catch (e) {
    toast(`${t("打开导入目录失败：")}${e}`, "error");
  }
}

function selectModel(model: ModelEntry) {
  selected.value = selected.value?.path === model.path ? null : model;
}

async function saveParams() {
  if (!props.cfg || !props.saveModelConfig) {
    toast(t("当前页面没有可保存的配置上下文"), "warn");
    return;
  }
  const ok = await props.saveModelConfig();
  if (ok) toast(t("推理参数已保存；重新加载模型后生效"), "ok");
}

async function setParallelMode(mode: string) {
  if (!props.cfg || !props.saveModelConfig) return;
  props.cfg.model.parallel_mode = mode;
  const saved = await props.saveModelConfig();
  if (!saved) return;
  const modelPath = props.cfg.model.embed_model.trim();
  if (!modelPath || !info.value?.ready) {
    toast(t("批处理线程设置已保存；加载嵌入模型后生效"), "ok");
    return;
  }
  busy.value = "parallel-mode";
  try {
    const out = await api.modelLoad("auto", modelPath);
    info.value = out;
    store.model = out;
    await load();
    toast(out.ready ? `${t("批处理线程已应用：")}${out.detail}` : out.detail, out.ready ? "ok" : "warn");
  } catch (error) {
    toast(`${t("设置已保存，但模型重载失败：")}${error}`, "error");
  } finally {
    busy.value = "";
  }
}

async function setParallelThreads() {
  if (!props.cfg) return;
  props.cfg.model.parallel_threads = Math.max(1, Math.min(128, Number(props.cfg.model.parallel_threads) || 1));
  await setParallelMode("custom");
}

const modalityCards = computed(() => {
  const selectedModalities = selectedEmbedding.value?.modalities || [];
  const declared = (key: string) => selectedModalities.length > 0
    ? selectedModalities.includes(key)
    : info.value?.modalities?.includes(key) || false;
  const selectedReady = selectedEmbedding.value?.direct_loadable
    && selectedEmbedding.value?.active
    && info.value?.backend !== "hash";
  // Hash is only a deterministic search fallback, not a configured AI model.
  const activeSupports = (key: string) => !!info.value?.ready
    && info.value.backend !== "hash"
    && info.value.modalities.includes(key);
  const pairedProjector = (key: string) => hasProjectorCompanion.value && ["image", "audio", "video"].includes(key);
  // A loaded GGUF uses the same runtime capability report as Candle. The
  // package-level declaration is only used before activation; once active,
  // `info.modalities` is authoritative and prevents an unrelated mmproj from
  // being shown as usable.
  const isUsable = (key: string) => activeSupports(key) || (selectedReady && declared(key));
  const statusFor = (key: string) => isUsable(key)
    ? "可用"
    : pairedProjector(key)
      ? "已检测"
      : (declared(key) ? "已检测" : "未配置");
  const detailFor = (key: string) => pairedProjector(key) && !isUsable(key)
    ? "已检测同目录 mmproj；加载模型后可用"
    : declared(key) && !isUsable(key)
      ? "模型已声明此能力，加载后可用"
      : activeSupports(key) ? "共享 768 维向量空间" : "未配置可用编码器";
  return [
    {
      key: "text",
      label: "文本",
      icon: FileText,
      ok: activeSupports("text") || selectedReady,
      error: false,
      status: activeSupports("text") || selectedReady ? "可用" : (selected.value?.format === "safetensors" && declared("text") ? "声明" : "未配置"),
      detail: declared("text") && !selectedReady
        ? "模型声明 · 需加载"
        : info.value?.ready ? `${info.value.dim} ${t("维")}` : "未加载",
    },
    {
      key: "image",
      label: "图片",
      icon: ImageIcon,
      ok: isUsable("image"),
      error: false,
      status: statusFor("image"),
      detail: detailFor("image"),
    },
    {
      key: "audio",
      label: "音频",
      icon: AudioLines,
      ok: isUsable("audio"),
      error: false,
      status: statusFor("audio"),
      detail: detailFor("audio"),
    },
    {
      key: "video",
      label: "视频",
      icon: Film,
      ok: isUsable("video"),
      error: false,
      status: statusFor("video"),
      detail: detailFor("video"),
    },
    {
      key: "ocr",
      label: "OCR",
      icon: ScanText,
      ok: Boolean(
        (ocrStatus.value?.teleocr_available && ocrStatus.value?.model_configured)
        || (!ocrModelPath.value && ocrStatus.value?.tesseract_available),
      ),
      error: Boolean(
        ocrModelPath.value
        && !(ocrStatus.value?.teleocr_available && ocrStatus.value?.model_configured),
      ),
      status: ocrModelPath.value
        ? (ocrStatus.value?.teleocr_available && ocrStatus.value?.model_configured ? "可用" : "缺少依赖")
        : (ocrStatus.value?.tesseract_available ? "Tesseract OCR" : "未配置"),
      detail: ocrModelPath.value && ocrStatus.value?.missing_dependencies?.length
        ? `缺少：${ocrStatus.value.missing_dependencies.join("、")}`
        : ocrStatus.value?.model_candidates?.length && !ocrModelPath.value
          ? `检测到 ${ocrStatus.value.model_candidates.length} 个 OCR 模型，可在下方设为 OCR 模型`
          : ocrStatus.value?.detail || (ocrStatus.value?.tesseract_available ? "未选择模型，使用系统 OCR" : "未找到 Tesseract OCR"),
    },
  ];
});

onMounted(() => { void load(); });
</script>

<template>
  <div class="view">
    <div class="parallel-row card">
      <div>
        <div class="sec">{{ t("单模型批处理") }}</div>
      </div>
      <div class="parallel-buttons" role="group" :aria-label="t('单模型批处理')">
        <button
          v-for="mode in visibleParallelModes"
          :key="mode.key"
          class="parallel-button"
          :class="[mode.className, { active: currentParallelMode === mode.key }]"
          :title="t(mode.label)"
          @click="setParallelMode(mode.key)"
        >{{ t(mode.label) }}</button>
      </div>
      <label v-if="props.cfg?.model?.parallel_mode === 'custom' && props.cfg?.ui?.professional_mode" class="parallel-count">
        {{ t("并行线程数") }}
        <input v-model.number="props.cfg.model.parallel_threads" type="number" min="1" max="128" step="1" @change="setParallelThreads" />
      </label>
    </div>

    <!-- Modal capability status -->
    <div class="modality-grid">
      <div v-for="card in modalityCards" :key="card.key" class="modality-card card" :class="{ supported: card.ok, error: card.error }">
        <component :is="card.icon" :size="18" aria-hidden="true" />
        <div class="modality-copy"><strong>{{ t(card.label) }}</strong><span>{{ t(card.detail) }}</span></div>
        <span class="modality-state">{{ t(card.status) }}</span>
      </div>
    </div>

    <!-- Model list toolbar -->
    <div class="bar">
      <div class="chips">
        <button
          v-for="f in FILTERS"
          :key="f.v"
          class="tab"
          :class="{ active: filter === f.v }"
          @click="filter = f.v"
        >
          {{ t(f.label) }}
          <template v-if="f.v !== 'all'">
            ({{ list.filter((m) => m.kind === f.v).length }})
          </template>
        </button>
      </div>
      <span class="grow"></span>
      <span class="muted tiny">
        {{ list.length }} {{ t("个模型") }} · {{ t("共") }} {{ humanSize(totalSize) }} · {{ dir }}
      </span>
      <button v-if="importedDir" class="ghost" @click="openImportedDir">{{ t("打开导入目录") }}</button>
      <button class="ghost" :disabled="loading" @click="load(true)">{{ t("刷新") }}</button>
      <button class="primary" :disabled="!!busy" @click="importFile">{{ t("导入模型") }}</button>
    </div>

    <div class="list card">
      <div v-if="loading" class="none">{{ t("扫描中…") }}</div>
      <div v-else-if="!shown.length" class="empty">
        <div class="big">{{ t("模型目录里还没有 GGUF 或 safetensors 权重") }}</div>
        <div class="muted tiny">{{ t("选择模型文件夹后，程序会自动识别权重及 tokenizer、配置等配套文件") }}</div>
      </div>
      <div v-for="m in shown" :key="m.path" class="item" :class="{ active: m.active, selected: selected?.path === m.path }" @click="selectModel(m)">
        <div class="left">
          <div class="row">
            <span class="nm" :title="m.name">{{ m.name }}</span>
            <span class="kind" :class="m.kind">{{ m.kind_label }}</span>
            <span class="kind format">{{ m.format.toUpperCase() }}</span>
            <span v-if="hasMultimodalCompanion(m)" class="kind multimodal">{{ t("多模态") }}</span>
          </div>
          <div class="file mono" :title="m.path">{{ m.file }} · {{ m.size_label }}</div>
          <div class="meta muted tiny">{{ m.mtime_label }} · {{ m.path }}</div>
        </div>
        <div class="ops">
          <button v-if="m.active" class="ghost danger" :title="t('停用推理模型')" :disabled="!!busy" @click.stop="deactivateModel(m)">{{ busy === m.path ? t("正在暂停并停用…") : t("停用") }}</button>
          <button v-if="isOcrModel(m)" class="ghost danger" :title="t('停用 OCR')" :disabled="!!busy" @click.stop="deactivateOcrModel(m)">{{ t("停用") }}</button>
          <button v-if="m.kind !== 'projector' && !hasActiveRole(m)" class="primary" :disabled="!!busy || (!canActivate(m) && m.format !== 'safetensors')" @click.stop="chooseRole(m)">{{ t("设为当前模型") }}</button>
          <button class="ghost" :title="`${t('打开模型文件夹：')}${m.model_dir}`" @click.stop="openModelDir(m)">
            <FolderOpen :size="14" />
          </button>
          <button class="ghost danger" :disabled="m.active || isOcrModel(m) || !!busy" @click.stop="remove(m)">
            {{ t("删除") }}
          </button>
        </div>
      </div>
    </div>

    <div v-if="roleDialogOpen && roleModel" class="role-modal-backdrop" role="presentation" @click.self="roleDialogOpen = false">
      <section class="role-modal" role="dialog" aria-modal="true" :aria-label="t('选择模型用途')">
        <button type="button" class="role-modal-close" :aria-label="t('关闭')" @click="roleDialogOpen = false">×</button>
        <h2>{{ t("选择模型用途") }}</h2>
        <p class="role-model-name" :title="roleModel.path">{{ roleModel.name }}</p>
        <div class="role-actions">
          <button class="primary" :disabled="!canActivate(roleModel) || !!busy" @click="setRole('embed')">{{ t("推理模型") }}</button>
          <button class="secondary-button" :disabled="!canUseOcr(roleModel) || !!busy" @click="setRole('ocr')">{{ t("OCR 模型") }}</button>
        </div>
      </section>
    </div>

    <!-- Click a model to expand its inference parameters. -->
    <section v-if="selected && props.cfg?.ui?.professional_mode" class="params card">
      <div class="params-head">
        <div><strong>{{ selected.name }}</strong><span class="kind format">{{ selected.format.toUpperCase() }}</span></div>
        <SlidersHorizontal :size="16" aria-hidden="true" />
      </div>
      <div class="params-grid">
        <label>{{ t("上下文长度") }}<input type="number" min="512" step="256" v-model.number="props.cfg.model.context_size" /></label>
        <label class="param-path">{{ t("模型路径") }}<input :value="selected.path" readonly /></label>
      </div>
      <p v-if="selected.format === 'safetensors'" class="params-warning">
        {{ t("safetensors 是权重容器；请确保配置文件、分片权重和 tokenizer 等配套文件位于同一目录。") }}
      </p>
      <div class="params-actions"><span class="muted tiny">{{ t("0 表示由运行时自动选择线程数。") }}</span><button class="primary" @click="saveParams"><Save :size="14" />{{ t("保存推理参数") }}</button></div>
    </section>

    <p class="muted tiny foot">
      {{ t("嵌入模型用于语义检索；切换模型后已有向量会按新维度重建（可能提示重新索引）。") }}
      {{ t("程序会自动选择匹配的加载方式；当前可加载状态以列表中的按钮为准。") }}
    </p>
  </div>
</template>

<style scoped>
.view {
  display: flex;
  flex-direction: column;
  height: 100%;
  gap: 8px;
  min-height: 0;
}
.icon-action { display: inline-grid; place-items: center; width: 32px; height: 32px; padding: 0; }
.parallel-row { display: flex; align-items: center; justify-content: space-between; gap: 16px; padding: 10px 14px; flex: none; }
.parallel-buttons { display: inline-flex; gap: 0; }
.parallel-button { min-width: 82px; padding: 8px 14px; border: 1px solid #111; border-right: 0; border-radius: 0; color: #111; background: #fff; cursor: pointer; }
.parallel-button:last-child { border-right: 1px solid #111; }
.parallel-button.eco.active { color: #fff; background: #238636; }
.parallel-button.auto.active { color: #fff; background: #1d6fd1; }
.parallel-button.custom.active { color: #fff; background: #111; }
.parallel-count { display: flex; align-items: center; gap: 8px; font-size: 12px; }
.parallel-count input { width: 86px; }
.warn-text { color: var(--warn); }
.modality-grid { display: grid; grid-template-columns: repeat(5, minmax(0, 1fr)); gap: 8px; flex: none; }
.modality-card { display: flex; align-items: center; gap: 9px; min-width: 0; padding: 11px 12px; color: #555; }
.modality-card.supported { color: #111; }
.modality-card.error { color: #b42318; border-color: #b42318; }
.modality-copy { display: flex; flex-direction: column; min-width: 0; gap: 2px; }
.modality-copy strong { font-size: 12px; font-weight: 600; }
.modality-copy span { font-size: 10px; color: #777; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.modality-state { margin-left: auto; font-size: 10px; white-space: nowrap; color: #888; }
.modality-card.supported .modality-state { color: #111; }
.modality-card.error .modality-state, .modality-card.error .modality-copy span { color: #b42318; }
.cur {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 14px;
  padding: 12px 14px;
  flex: none;
}
.curmain { min-width: 0; }
.sec { font-size: 12px; color: var(--muted); }
.curpath {
  font-size: 12px;
  color: #c4d0e3;
  margin-top: 5px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 720px;
}
.curactions { display: flex; gap: 8px; flex: none; }
.row { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.tags { margin-top: 6px; }
.pill {
  font-size: 11px;
  color: var(--accent);
  background: rgba(79, 140, 255, 0.12);
  border: 1px solid rgba(79, 140, 255, 0.35);
  border-radius: 999px;
  padding: 1px 8px;
}
.pill.muted { color: var(--muted); background: transparent; border-color: var(--border); }
.badge {
  display: inline-block;
  font-size: 11px;
  padding: 3px 9px;
  border-radius: 999px;
  border: 1px solid var(--border);
  color: var(--muted);
}
.badge.ok {
  color: #22c55e;
  border-color: rgba(34, 197, 94, 0.45);
  background: rgba(34, 197, 94, 0.12);
}
.badge.warn {
  color: #f59e0b;
  border-color: rgba(245, 158, 11, 0.45);
  background: rgba(245, 158, 11, 0.12);
}
.bar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.chips { display: flex; gap: 6px; }
.grow { flex: 1; }
.list {
  flex: 1;
  overflow: auto;
  min-height: 0;
  padding: 6px;
}
.item {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 14px;
  padding: 10px 12px;
  border-radius: 9px;
  border: 1px solid transparent;
  border-bottom: 1px solid rgba(36, 48, 74, 0.55);
}
.item:hover { background: rgba(79, 140, 255, 0.06); }
.item.active { background: rgba(34, 197, 94, 0.08); border-color: rgba(34, 197, 94, 0.3); }
.item.selected { outline: 1px solid #111; outline-offset: -1px; background: #fafafa; }
.left { min-width: 0; flex: 1; }
.nm { font-size: 13px; font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 460px; }
.kind {
  font-size: 11px;
  border-radius: 5px;
  padding: 0 6px;
  border: 1px solid var(--border);
  color: var(--muted);
}
.kind.embed { color: #38bdf8; border-color: rgba(56, 189, 248, 0.4); }
.kind.chat { color: #a78bfa; border-color: rgba(167, 139, 250, 0.4); }
.kind.projector { color: #f59e0b; border-color: rgba(245, 158, 11, 0.4); }
.inuse { font-size: 11px; color: #22c55e; }
.file { font-size: 12px; color: #b7c4da; margin-top: 4px; }
.meta { margin-top: 3px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.backend-list { margin-top: 4px; color: #555; }
.ops { display: flex; gap: 8px; flex: none; }
.danger { color: #f87171; border-color: rgba(239, 68, 68, 0.4); }
.none { color: var(--muted); font-size: 12px; padding: 20px; text-align: center; }
.empty {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 8px;
  height: 100%;
  color: var(--muted);
}
.empty .big { font-size: 15px; color: #b7c4da; }
.params { flex: none; padding: 13px 14px; }
.params-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 11px; }
.params-head > div { display: flex; align-items: center; gap: 8px; min-width: 0; }
.params-head strong { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.params-grid { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 10px; }
.params-grid label { display: flex; flex-direction: column; gap: 5px; color: #555; font-size: 11px; }
.params-grid input { width: 100%; }
.params-grid .param-path { grid-column: span 1; }
.params-actions { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-top: 11px; }
.params-actions button { display: inline-flex; align-items: center; gap: 6px; }
.params-warning { margin: 10px 0 0; padding: 8px 10px; border: 1px solid #8c6410; color: #6f4e00; font-size: 11px; line-height: 1.5; background: #fffaf0; }
.tiny { font-size: 11px; }
.foot { margin: 0; line-height: 1.6; flex: none; }
/* Flat white/black model management surface. */
.view { color: #111; }
.cur, .list { border: 1px solid #111; border-radius: 2px; background: #fff; }
.curpath, .file { color: #222; }
.pill { color: #111; background: #f2f2f2; border-color: #111; border-radius: 2px; }
.pill.muted { color: #555; background: #fff; border-color: #aaa; }
.badge { border-radius: 2px; color: #555; }
.badge.ok { color: #111; border-color: #111; background: #f2f2f2; }
.badge.warn { color: #8c6410; border-color: #8c6410; background: #fffaf0; }
.list { padding: 4px; }
.item { border-radius: 2px; border-color: transparent; border-bottom-color: #ddd; }
.item:hover { background: #f0f0f0; }
.item.active { background: #f7f7f7; border-color: #111; }
.kind { border-radius: 2px; color: #333; }
.kind.embed, .kind.chat, .kind.projector { color: #111; border-color: #777; }
.kind.multimodal { color: #111; border-color: #111; background: #f2f2f2; }
.inuse { color: #111; }
.danger { color: #9b2923; border-color: #9b2923; }
.empty .big { color: #111; }
.role-modal-backdrop { position: fixed; inset: 0; z-index: 500; display: grid; place-items: center; padding: 18px; background: rgba(0,0,0,.38); backdrop-filter: blur(4px); }
.role-modal { position: relative; width: min(390px, calc(100vw - 36px)); padding: 22px; border: 1px solid #111; background: #fff; color: #111; box-shadow: 0 18px 48px rgba(0,0,0,.2); }
.role-modal h2 { margin: 0 28px 10px 0; font-size: 16px; }
.role-modal-close { position: absolute; top: 7px; right: 9px; border: 0; background: transparent; color: #111; font-size: 22px; cursor: pointer; }
.role-model-name { overflow: hidden; margin: 0; color: #555; font-size: 12px; text-overflow: ellipsis; white-space: nowrap; }
.role-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 20px; }
@media (max-width: 700px) {
  .modality-grid { grid-template-columns: repeat(2, minmax(0, 1fr)); }
  .params-grid { grid-template-columns: 1fr; }
  .params-grid .param-path { grid-column: auto; }
  .params-actions { align-items: flex-start; flex-direction: column; }
  .parallel-row { align-items: flex-start; flex-direction: column; }
  .parallel-buttons { width: 100%; }
  .parallel-button { flex: 1; }
}
</style>
