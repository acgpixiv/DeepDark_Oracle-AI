<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { Download, FolderOpen, RefreshCw, Trash2 } from "lucide-vue-next";
import { api, type ExtensionStatus } from "../api";
import { toast } from "../store";
import { t } from "../i18n";

const status = ref<ExtensionStatus | null>(null);
const loading = ref(false);
type ExtensionKind = "onnxruntime" | "python" | "pytorch";
const installing = reactive<Record<ExtensionKind, boolean>>({ onnxruntime: false, python: false, pytorch: false });

function mergeStatus(next: ExtensionStatus, kind: ExtensionKind) {
  if (!status.value) {
    status.value = next;
    return;
  }
  // Concurrent installers must not overwrite another card with an older snapshot.
  const keys: (keyof ExtensionStatus)[] = kind === "onnxruntime"
    ? ["onnxruntime", "onnxruntime_path", "onnxruntime_version"]
    : kind === "python"
      ? ["python", "python_path"]
      : ["python", "python_path", "pytorch", "pytorch_path", "pytorch_version"];
  status.value = { ...status.value, ...Object.fromEntries(keys.map((key) => [key, next[key]])), root: next.root, detail: next.detail };
}

async function refresh() {
  loading.value = true;
  try {
    status.value = await api.extensionStatus();
  } catch (error) {
    toast(`${t("读取扩展状态失败：")}${error}`, "error");
  } finally {
    loading.value = false;
  }
}

async function install(kind: ExtensionKind) {
  if (installing[kind]) return;
  installing[kind] = true;
  try {
    mergeStatus(await api.extensionInstall(kind), kind);
    const label = kind === "onnxruntime" ? "ONNX Runtime" : kind === "python" ? "Python sidecar" : "PyTorch OCR 运行时";
    toast(`${t(label)} ${t("已安装到项目目录")}`, "ok");
  } catch (error) {
    toast(`${t("扩展安装失败：")}${error}`, "error");
  } finally {
    installing[kind] = false;
  }
}

async function updateOnnxRuntime() {
  if (installing.onnxruntime) return;
  installing.onnxruntime = true;
  try {
    const next = await api.extensionUpdate();
    mergeStatus(next, "onnxruntime");
    toast(next.detail || `${t("ONNX Runtime")}: ${next.onnxruntime_version}`, "ok");
  } catch (error) {
    toast(`${t("扩展更新失败：")}${error}`, "error");
  } finally {
    installing.onnxruntime = false;
  }
}

async function removeOnnxRuntime() {
  if (installing.onnxruntime) return;
  if (!window.confirm(t("确定将 ONNX Runtime 移入回收站？"))) return;
  installing.onnxruntime = true;
  try {
    mergeStatus(await api.extensionRemove("onnxruntime"), "onnxruntime");
    toast(t("ONNX Runtime 已移入回收站"), "ok");
  } catch (error) {
    toast(`${t("扩展卸载失败：")}${error}`, "error");
  } finally {
    installing.onnxruntime = false;
  }
}

async function openRoot() {
  if (status.value?.root) await api.openPath(status.value.root);
}

onMounted(refresh);
</script>

<template>
  <section class="settings-section extension-section">
    <div class="section-head">
      <div>
        <h2>{{ t("可选扩展") }}</h2>
        <p>{{ t("内置模型支持保持轻量；按需安装兼容性扩展，文件保存在项目目录。") }}</p>
      </div>
      <div class="extension-actions">
        <button class="secondary-button" :disabled="loading" @click="refresh"><RefreshCw :size="14" />{{ t("刷新") }}</button>
        <button class="secondary-button" :disabled="!status?.root" @click="openRoot"><FolderOpen :size="14" />{{ t("打开扩展目录") }}</button>
      </div>
    </div>

    <div class="extension-grid">
      <article class="extension-card">
        <div class="extension-card-head"><strong>ONNX Runtime</strong><span class="extension-state" :class="{ on: status?.onnxruntime }">{{ status?.onnxruntime ? t("已安装") : t("未安装") }}</span></div>
        <p>{{ t("用于运行标准 ONNX 模型，例如 CLIP、CLAP。不能直接读取 safetensors。") }}</p>
        <div class="extension-path mono">{{ status?.onnxruntime_path || t("项目/.tools/onnxruntime/onnxruntime.dll") }}</div>
        <div class="extension-card-actions">
          <button class="primary" :disabled="installing.onnxruntime" @click="status?.onnxruntime ? updateOnnxRuntime() : install('onnxruntime')">
            <Download :size="14" />{{ installing.onnxruntime ? t("安装中…") : t("安装") }}
          </button>
          <button v-if="status?.onnxruntime" class="remove-extension" :disabled="installing.onnxruntime" @click="removeOnnxRuntime">
            <Trash2 :size="13" />{{ t("删除") }}
          </button>
        </div>
      </article>

      <article class="extension-card">
        <div class="extension-card-head"><strong>Python sidecar</strong><span class="extension-state" :class="{ on: status?.python }">{{ status?.python ? t("已安装") : t("未安装") }}</span></div>
        <p>{{ t("可选的 Python 运行时入口，用于尚未被内置模型支持的格式；不会自动安装 PyTorch。") }}</p>
        <div class="extension-path mono">{{ status?.python_path || t("项目/.tools/python/python.exe") }}</div>
        <button class="primary" :disabled="installing.python" @click="install('python')"><Download :size="14" />{{ installing.python ? t("安装中…") : t("安装") }}</button>
      </article>

      <article class="extension-card">
        <div class="extension-card-head"><strong>PyTorch</strong><span class="extension-state" :class="{ on: status?.pytorch }">{{ status?.pytorch ? `${t("已安装")} ${status.pytorch_version}` : t("未安装") }}</span></div>
        <p>{{ t("安装本地 OCR 推理依赖。检测到 NVIDIA 显卡时使用 CUDA 版；其他设备使用 PyTorch CPU 运行时。") }}</p>
        <div class="extension-path mono">{{ status?.pytorch_path || t("项目/.tools/python/Lib/site-packages") }}</div>
        <button class="primary" :disabled="installing.pytorch" @click="install('pytorch')"><Download :size="14" />{{ installing.pytorch ? t("安装中…") : t("安装") }}</button>
      </article>

    </div>
    <p class="extension-note">{{ t("扩展按需安装在项目目录。PyTorch 安装会下载较大的运行库，不会安装 OCR 模型权重。") }}</p>
  </section>
</template>

<style scoped>
.extension-actions { display: flex; flex-wrap: wrap; gap: 7px; }
.extension-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 10px; }
.extension-card { min-width: 0; padding: 13px; border: 1px solid #111; background: #fff; }
.extension-card-head { display: flex; justify-content: space-between; gap: 10px; align-items: center; }
.extension-card p, .extension-note { color: #555; font-size: 11px; line-height: 1.55; }
.extension-card p { min-height: 34px; margin: 8px 0; }
.extension-state { color: #777; font-size: 11px; }
.extension-state.on { color: #111; font-weight: 600; }
.extension-path { margin: 8px 0 11px; overflow: hidden; color: #555; font-size: 10px; text-overflow: ellipsis; white-space: nowrap; }
.extension-card button { display: inline-flex; align-items: center; gap: 6px; }
.extension-card-actions { display: flex; align-items: center; gap: 7px; }
.remove-extension { min-width: 54px; padding: 6px 7px; border: 1px solid #b42318; border-radius: 2px; color: #b42318; background: #fff; font-size: 11px; cursor: pointer; }
.remove-extension:disabled { opacity: .55; cursor: default; }
.extension-note { margin: 12px 0 0; }
@media (max-width: 720px) { .extension-grid { grid-template-columns: 1fr; } .extension-actions { margin-top: 8px; } }
</style>
