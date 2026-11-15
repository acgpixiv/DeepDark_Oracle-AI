<script setup lang="ts">
import { onBeforeUnmount, ref, watch } from "vue";
import type { AppConfig } from "../api";
import { AlertTriangle, Languages, Rocket } from "lucide-vue-next";
import { t } from "../i18n";

const props = defineProps<{
  cfg: AppConfig;
  saveGeneralConfig: () => Promise<boolean>;
}>();

const professionalWarningOpen = ref(false);
const autoSaving = ref(false);
let autoSaveTimer: ReturnType<typeof setTimeout> | undefined;

async function saveAll() {
  if (autoSaving.value || !props.cfg) return;
  autoSaving.value = true;
  try {
    await props.saveGeneralConfig();
  } catch (error) {
    console.error("automatic settings save failed", error);
  } finally {
    autoSaving.value = false;
  }
}

function scheduleAutoSave() {
  if (autoSaving.value) return;
  if (autoSaveTimer) clearTimeout(autoSaveTimer);
  autoSaveTimer = setTimeout(() => {
    autoSaveTimer = undefined;
    void saveAll();
  }, 250);
}

watch([() => props.cfg.ui, () => props.cfg.model], scheduleAutoSave, { deep: true });
onBeforeUnmount(() => {
  if (autoSaveTimer) clearTimeout(autoSaveTimer);
});

function toggleProfessionalMode() {
  if (props.cfg.ui.professional_mode) {
    props.cfg.ui.professional_mode = false;
    if (props.cfg.model.parallel_mode === "custom") props.cfg.model.parallel_mode = "auto";
    return;
  }
  professionalWarningOpen.value = true;
}

function enableProfessionalMode() {
  props.cfg.ui.professional_mode = true;
  professionalWarningOpen.value = false;
}
</script>

<template>
  <div class="settings-general-tab">
    <section class="settings-section">
      <div class="section-head">
        <div><h2><Languages :size="17" />{{ t("语言与主题") }}</h2><p>{{ t("界面语言和配色方案，修改后立即生效。") }}</p></div>
      </div>
      <div class="form-grid three">
        <label>
          {{ t("界面语言") }}
          <select v-model="props.cfg.ui.language">
            <option value="zh">{{ t("简体中文") }}</option>
            <option value="en">English</option>
          </select>
        </label>
        <label>
          {{ t("主题") }}
          <select v-model="props.cfg.ui.theme">
            <option value="light">{{ t("明亮") }}</option>
            <option value="dark">{{ t("暗色") }}</option>
            <option value="system">{{ t("跟随系统") }}</option>
          </select>
        </label>
        <label>
          {{ t("默认搜索模式") }}
          <select v-model="props.cfg.ui.default_search_mode">
            <option value="fulltext">{{ t("全文搜索") }}</option>
            <option value="semantic">{{ t("语义搜索") }}</option>
            <option value="hybrid">{{ t("综合（全文 + 语义）") }}</option>
          </select>
        </label>
      </div>
      <div class="switch-row">
        <label><input type="checkbox" v-model="props.cfg.ui.remember_window" />{{ t("记住窗口大小和位置") }}</label>
      </div>
    </section>

    <section class="settings-section professional-section">
      <div class="professional-control">
        <h2>{{ t("专业配置") }}</h2>
        <button
          type="button"
          class="boolean-switch"
          :class="{ active: props.cfg.ui.professional_mode }"
          role="switch"
          :aria-checked="!!props.cfg.ui.professional_mode"
          @click="toggleProfessionalMode"
        ><span>{{ t("开启") }}</span><span>{{ t("关闭") }}</span></button>
      </div>
    </section>

    <div v-if="professionalWarningOpen" class="modal-backdrop" role="presentation" @click.self="professionalWarningOpen = false">
      <section class="warning-dialog" role="dialog" aria-modal="true" :aria-label="t('专业配置')">
        <AlertTriangle :size="22" aria-hidden="true" />
        <h3>{{ t("专业配置") }}</h3>
        <p>{{ t("专业配置可能导致部分功能运作不正常。确定开启吗？") }}</p>
        <div class="dialog-actions">
          <button type="button" class="secondary-button" @click="professionalWarningOpen = false">{{ t("取消") }}</button>
          <button type="button" class="save-button" @click="enableProfessionalMode">{{ t("确定") }}</button>
        </div>
      </section>
    </div>

    <section class="settings-section">
      <div class="section-head">
        <div><h2><Rocket :size="17" />{{ t("启动行为") }}</h2><p>{{ t("开机自启与启动时的索引维护策略。") }}</p></div>
      </div>
      <div class="switch-row">
        <label><input type="checkbox" v-model="props.cfg.ui.autostart" />{{ t("开机自动启动") }}</label>
        <label><input type="checkbox" v-model="props.cfg.ui.auto_scan_on_start" />{{ t("启动时自动扫描索引目录") }}</label>
        <label><input type="checkbox" v-model="props.cfg.ui.notify_file_changes" />{{ t("发现文件变化时给出提示") }}</label>
      </div>
    </section>
  </div>
</template>

<style scoped>
.field-hint { margin: 12px 0 0; color: #555; font-size: 11px; line-height: 1.5; }
.plain-label { justify-content: flex-start; }
.professional-section { padding: 12px 14px; }
.professional-control { display: flex; align-items: center; justify-content: space-between; gap: 14px; width: 100%; }
.professional-control h2 { margin: 0; font-size: 13px; text-align: left; }
.boolean-switch { display: inline-grid; grid-template-columns: 1fr 1fr; min-width: 112px; padding: 0; border: 1px solid #111; border-radius: 2px; color: #111; background: #fff; cursor: pointer; }
.boolean-switch span { padding: 5px 10px; font-size: 11px; text-align: center; }
.boolean-switch span:first-child { background: #fff; color: #111; }
.boolean-switch span:last-child { background: #111; color: #fff; }
.boolean-switch.active span:first-child { background: #111; color: #fff; }
.boolean-switch.active span:last-child { background: #fff; color: #111; }
.inline-check { display: inline-flex; align-items: center; gap: 6px; color: #222; }
.inline-check input { width: auto; accent-color: #111; }
.modal-backdrop { position: fixed; inset: 0; z-index: 200; display: grid; place-items: center; padding: 16px; background: rgba(0,0,0,.42); }
.warning-dialog { width: min(420px, 100%); padding: 22px; border: 1px solid #111; background: #fff; color: #111; box-shadow: 0 18px 50px rgba(0,0,0,.22); }
.warning-dialog > svg { color: #8c1d24; }
.warning-dialog h3 { margin: 10px 0 6px; font-size: 16px; }
.warning-dialog p { margin: 0; color: #444; font-size: 12px; line-height: 1.6; }
.dialog-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 20px; }
</style>
