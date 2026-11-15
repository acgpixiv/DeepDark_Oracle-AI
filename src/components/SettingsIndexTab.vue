<script setup lang="ts">
import { computed } from "vue";
import { Database, Plus, RefreshCw, Save, Trash2 } from "lucide-vue-next";
import type { AppConfig, Rule } from "../api";
import ScanView from "../views/ScanView.vue";
import { t } from "../i18n";

const props = defineProps<{
  cfg: AppConfig;
  rules: Rule[];
  newRule: Rule;
  excludeDirsText: string;
  dataDir: string;
  saveFileConfig: () => Promise<boolean | void>;
  addRule: () => Promise<void>;
  deleteRule: (index: number) => Promise<void>;
  applyRules: () => Promise<void>;
  saveRules: () => Promise<void>;
  vacuum: () => Promise<void>;
  clearAll: () => Promise<void>;
}>();

const emit = defineEmits<{
  (event: "update-exclude-dirs", value: string): void;
}>();

const excludeDirs = computed({
  get: () => props.excludeDirsText,
  set: (value: string) => emit("update-exclude-dirs", value),
});

const tokenBudgets = [70, 140, 280, 560, 1120];

// Keep existing custom settings selectable when presenting the common presets.
function presets(current: number, values: number[]) {
  return Array.from(new Set([...values, current])).filter(Number.isFinite).sort((a, b) => a - b);
}
</script>

<template>
  <div class="settings-index-tab">
    <div class="embedded-view scan-embed"><ScanView /></div>

    <section v-if="props.cfg.ui?.professional_mode" class="settings-section index-parameters">
      <div class="section-head">
        <div>
          <h2>{{ t("索引参数") }}</h2>
          <p>{{ t("设置处理范围、向量精度与媒体预算，保存后从下一次建立索引生效。") }}</p>
        </div>
        <button class="save-button" @click="props.saveFileConfig"><Save :size="14" />{{ t("保存") }}</button>
      </div>
      <div class="param-groups">
        <section class="param-group">
          <h3>{{ t("索引库与正文") }}</h3>
          <label class="param-field">
            <span>{{ t("向量维度") }}</span>
            <select v-model.number="props.cfg.index.vector_dim">
              <option :value="768">768</option>
              <option :value="512">512</option>
              <option :value="256">256</option>
            </select>
          </label>
          <label class="param-field">
            <span>{{ t("正文字符上限") }}</span>
            <select v-model.number="props.cfg.index.max_text_chars">
              <option v-for="value in presets(props.cfg.index.max_text_chars, [50000, 100000, 200000, 500000, 1000000])" :key="value" :value="value">{{ value.toLocaleString() }}</option>
            </select>
          </label>
          <label class="param-field">
            <span>{{ t("批处理目标") }}</span>
            <select v-model.number="props.cfg.index.batch_size">
              <option v-for="value in presets(props.cfg.index.batch_size, [4, 8, 16, 32])" :key="value" :value="value">{{ value }}</option>
            </select>
          </label>
          <p class="param-note">{{ t("实际批量按显存与任务调整；修改维度需要匹配的模型并重新建立向量。") }}</p>
          <div class="param-switches">
            <label class="check-label"><input type="checkbox" v-model="props.cfg.index.auto_fulltext" />{{ t("自动抽取正文") }}</label>
            <label class="check-label"><input type="checkbox" v-model="props.cfg.index.auto_embed" />{{ t("自动生成向量") }}</label>
          </div>
        </section>
        <section class="param-group">
          <h3>{{ t("图片") }}<span>Token</span></h3>
          <label class="param-field">
            <span>{{ t("独立图片预算") }}</span>
            <select v-model.number="props.cfg.index.image_token_budget">
              <option v-for="value in presets(props.cfg.index.image_token_budget, tokenBudgets)" :key="value" :value="value">{{ value }}</option>
            </select>
          </label>
          <label class="param-field">
            <span>{{ t("PDF 内嵌图片预算") }}</span>
            <select v-model.number="props.cfg.index.pdf_image_token_budget">
              <option v-for="value in presets(props.cfg.index.pdf_image_token_budget, tokenBudgets)" :key="value" :value="value">{{ value }}</option>
            </select>
          </label>
          <label class="param-field">
            <span>{{ t("独立图片最长边") }}</span>
            <select v-model.number="props.cfg.index.image_max_side">
              <option v-for="value in presets(props.cfg.index.image_max_side, [256, 448, 512, 768, 1024, 2048])" :key="value" :value="value">{{ value }} px</option>
            </select>
          </label>
          <p class="param-note">{{ t("多模态预算适用于支持相应编码器的 GGUF + mmproj 或 safetensors 模型。") }}</p>
          <div class="param-switches">
            <label class="check-label"><input type="checkbox" v-model="props.cfg.index.vision_encoder" />{{ t("启用图片 / 视频编码器") }}</label>
          </div>
        </section>
        <section class="param-group">
          <h3>{{ t("视频与音频") }}</h3>
          <label class="param-field">
            <span>{{ t("视频每帧预算") }}</span>
            <select v-model.number="props.cfg.index.video_token_budget">
              <option v-for="value in presets(props.cfg.index.video_token_budget, tokenBudgets)" :key="value" :value="value">{{ value }} Token</option>
            </select>
          </label>
          <label class="param-field">
            <span>{{ t("音频窗口时长") }}</span>
            <select v-model.number="props.cfg.index.audio_seconds">
              <option v-for="value in presets(props.cfg.index.audio_seconds, [5, 10, 15, 30])" :key="value" :value="value">{{ value }} {{ t("秒") }}</option>
            </select>
          </label>
          <p class="param-note">{{ t("视频最长 30 秒一段，每秒 1 帧；上下文不足时自动缩短。") }}</p>
          <div class="param-switches">
            <label class="check-label"><input type="checkbox" v-model="props.cfg.index.video_index_audio" />{{ t("索引视频音轨") }}</label>
            <label class="check-label"><input type="checkbox" v-model="props.cfg.index.audio_encoder" />{{ t("启用音频编码器") }}</label>
          </div>
        </section>
        <section class="param-group">
          <h3>{{ t("文件范围") }}</h3>
          <label class="param-field">
            <span>{{ t("最大文件大小") }}</span>
            <select v-model.number="props.cfg.scan.max_size_mb">
              <option v-for="value in presets(props.cfg.scan.max_size_mb, [256, 1024, 4096, 16384, 65536])" :key="value" :value="value">{{ value.toLocaleString() }} MB</option>
            </select>
          </label>
          <label class="param-field">
            <span>{{ t("最小文件大小") }}</span>
            <select v-model.number="props.cfg.scan.min_size_kb">
              <option v-for="value in presets(props.cfg.scan.min_size_kb, [0, 1, 10, 100, 1024])" :key="value" :value="value">{{ value }} KB</option>
            </select>
          </label>
          <div class="param-switches">
            <label class="check-label"><input type="checkbox" v-model="props.cfg.scan.follow_links" />{{ t("扫描时跟随符号链接") }}</label>
          </div>
        </section>
      </div>
      <details v-if="props.cfg.model.clip_enabled" class="clip-options">
        <summary>{{ t("CLIP 备用视频采样") }}</summary>
        <div class="clip-fields">
          <label class="param-field"><span>{{ t("抽帧间隔（秒）") }}</span><input type="number" min="0.1" step="0.1" v-model.number="props.cfg.index.video_frame_interval" /></label>
          <label class="param-field"><span>{{ t("最大帧数") }}</span><input type="number" min="1" max="256" v-model.number="props.cfg.index.video_max_frames" /></label>
          <label class="param-field"><span>{{ t("去重阈值") }}</span><input type="number" min="0" max="64" v-model.number="props.cfg.index.video_dedup_distance" /></label>
        </div>
      </details>
      <label class="wide-label exclude-label">
        {{ t("排除目录（每行一个名称）") }}
        <textarea v-model="excludeDirs" rows="3" placeholder="node_modules&#10;.git&#10;System Volume Information"></textarea>
      </label>
    </section>

    <section class="settings-section">
      <div class="section-head">
        <div>
          <h2>{{ t("分类规则") }}</h2>
          <p>{{ t("按文件名、扩展名、路径或类型自动分类。") }}</p>
        </div>
      </div>
      <div class="rule-form">
        <select v-model="props.newRule.field">
          <option value="name">{{ t("文件名") }}</option>
          <option value="ext">{{ t("扩展名") }}</option>
          <option value="path">{{ t("完整路径") }}</option>
          <option value="kind">{{ t("文件类型") }}</option>
        </select>
        <select v-model="props.newRule.op">
          <option value="contains">{{ t("包含") }}</option>
          <option value="equals">{{ t("等于") }}</option>
          <option value="startswith">{{ t("开头是") }}</option>
          <option value="endswith">{{ t("结尾是") }}</option>
          <option value="regex">{{ t("正则") }}</option>
        </select>
        <input v-model="props.newRule.value" :placeholder="t('匹配内容，如 合同 / .jpg')" />
        <input v-model="props.newRule.category" :placeholder="t('归为分类')" />
        <button class="save-button" @click="props.addRule"><Plus :size="14" />{{ t("添加") }}</button>
      </div>
      <div class="rule-list">
        <div v-for="(rule, index) in props.rules" :key="index" class="rule-row">
          <input type="checkbox" v-model="rule.enabled" />
          <span class="mono">{{ rule.field }} {{ rule.op }} "{{ rule.value }}"</span>
          <span class="muted">{{ rule.category }}</span>
          <button class="icon-button tiny-button" :title="t('删除规则')" :aria-label="t('删除规则')" @click="props.deleteRule(index)"><Trash2 :size="14" /></button>
        </div>
        <div v-if="!props.rules.length" class="empty-line">{{ t("还没有规则") }}</div>
      </div>
      <div class="action-row">
        <button class="save-button" @click="props.applyRules"><RefreshCw :size="14" />{{ t("重新分类") }}</button>
        <button class="secondary-button" @click="props.saveRules"><Save :size="14" />{{ t("保存规则") }}</button>
      </div>
    </section>

    <section class="settings-section data-section">
      <div><h2>{{ t("索引数据库") }}</h2><p class="muted">{{ props.dataDir }}</p></div>
      <div class="action-row">
        <button class="secondary-button" @click="props.vacuum"><Database :size="14" />{{ t("压缩数据库") }}</button>
        <button class="danger-button" @click="props.clearAll"><Trash2 :size="14" />{{ t("清空索引") }}</button>
      </div>
    </section>
  </div>
</template>

<style scoped>
.param-groups {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 24px;
  margin: 16px 0;
}

.index-parameters .param-field {
  display: flex;
  flex-direction: row;
  justify-content: space-between;
  align-items: center;
  gap: 10px;
  margin: 9px 0;
  min-height: 30px;
}

.param-field > span {
  min-width: 0;
  line-height: 1.5;
}

.index-parameters .param-field > select,
.index-parameters .param-field > input {
  width: 138px;
  flex: 0 0 138px;
}

.param-note {
  margin: 9px 0 0;
  color: var(--muted);
  font-size: 11px;
  line-height: 1.6;
}

.param-group {
  min-width: 0;
  border-top: 1px solid var(--border);
  padding-top: 12px;
}

.param-group h3 {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin: 0 0 12px;
  color: var(--text);
  font-size: 13px;
  font-weight: 600;
}

.param-group h3 > span {
  color: var(--muted);
  font-size: 10px;
  font-weight: 400;
}

.param-switches {
  display: flex;
  flex-wrap: wrap;
  gap: 10px 16px;
  margin-top: 14px;
}

.index-parameters .check-label {
  display: flex;
  flex-direction: row;
  align-items: center;
  gap: 8px;
}

.clip-options {
  margin: 16px 0;
  padding: 12px 0;
  border-top: 1px solid var(--border);
  border-bottom: 1px solid var(--border);
  font-size: 12px;
}

.clip-options summary {
  cursor: pointer;
  color: var(--muted);
}

.clip-fields {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 0 24px;
  margin-top: 10px;
}

@media (max-width: 760px) {
  .param-groups,
  .clip-fields {
    grid-template-columns: minmax(0, 1fr);
  }
}

@media (max-width: 420px) {
  .index-parameters .param-field > select,
  .index-parameters .param-field > input {
    width: 118px;
    flex-basis: 118px;
  }
}
</style>

