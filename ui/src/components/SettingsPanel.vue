<script setup lang="ts">
// 设置面板（纯展示）：主题 / 自启动 / 编码参数。
// draft 是 App.vue 持有的响应式对象，这里用 v-model 直接改写其字段（保存由父组件负责）。
import type { ScrcpyDraft, SettingsState } from "../types";

defineProps<{
  connected: boolean;
  settings: SettingsState;
  iconTheme: string;
  draft: ScrcpyDraft;
  settingsError: string;
  theme: string;
}>();

const emit = defineEmits<{
  (e: "set-theme", mode: "dark" | "light"): void;
  (e: "set-icon-theme", mode: "dark" | "light"): void;
  (e: "toggle-autostart"): void;
  (e: "save"): void;
}>();
</script>

<template>
  <div class="settings-panel">
    <div class="section">外观</div>
    <div class="settings-card">
      <div class="toggle-row">
        <span class="toggle-label">主题</span>
        <div class="theme-segment">
          <span
            class="theme-option"
            :class="{ active: theme === 'dark' }"
            @click="emit('set-theme', 'dark')"
          >深色</span>
          <span
            class="theme-option"
            :class="{ active: theme === 'light' }"
            @click="emit('set-theme', 'light')"
          >浅色</span>
        </div>
      </div>
      <div class="toggle-row">
        <span class="toggle-label">图标（窗口/托盘）</span>
        <div class="theme-segment">
          <span
            class="theme-option"
            :class="{ active: iconTheme === 'dark' }"
            @click="emit('set-icon-theme', 'dark')"
          >
            <span class="icon-swatch icon-swatch-dark">K</span>深色
          </span>
          <span
            class="theme-option"
            :class="{ active: iconTheme === 'light' }"
            @click="emit('set-icon-theme', 'light')"
          >
            <span class="icon-swatch icon-swatch-light">K</span>浅色
          </span>
        </div>
      </div>
      <div class="settings-help">
        按任务栏/面板明暗选择：深色面板用深色图标，浅色面板用浅色图标才看得清。
      </div>
    </div>
    <div class="section">常规</div>
    <div v-if="!connected" class="hint">正在连接 daemon…</div>
    <template v-else>
      <div v-if="settingsError" class="settings-error">{{ settingsError }}</div>
      <div class="settings-card">
        <div
          class="toggle-row"
          :class="{ disabled: !settings.autostartSupported }"
          @click="emit('toggle-autostart')"
        >
          <div class="toggle-switch" :class="{ active: settings.autostartEnabled }" />
          <span class="toggle-label">开机自启动</span>
          <span v-if="!settings.autostartSupported" class="settings-note">当前平台不支持</span>
        </div>
        <div class="settings-help">
          开启后，登录系统时自动在后台运行 daemon（托盘驻留），点击托盘“打开”再显示窗口。
        </div>
      </div>
    </template>
    <div class="section">编码设置</div>
    <div v-if="!connected" class="hint">正在连接 daemon…</div>
    <template v-else>
      <div class="settings-card">
        <div class="settings-field">
          <span class="settings-field-label">视频码率 (Mbps)</span>
          <input
            class="settings-input"
            type="number"
            min="0"
            step="0.5"
            v-model.number="draft.videoBitRateMbps"
          />
        </div>
        <div class="settings-field">
          <span class="settings-field-label">最大分辨率 (px，0=不限)</span>
          <input
            class="settings-input"
            type="number"
            min="0"
            step="1"
            v-model.number="draft.videoMaxSize"
          />
        </div>
        <div class="settings-field">
          <span class="settings-field-label">最大帧率 (fps，0=不限)</span>
          <input
            class="settings-input"
            type="number"
            min="0"
            step="1"
            v-model.number="draft.videoMaxFps"
          />
        </div>
        <div class="settings-field">
          <span class="settings-field-label">音频码率 (kbps)</span>
          <input
            class="settings-input"
            type="number"
            min="0"
            step="8"
            v-model.number="draft.audioBitRateKbps"
          />
        </div>
        <div class="settings-field">
          <span class="settings-field-label">音频编码器</span>
          <select class="settings-input" v-model="draft.audioCodec">
            <option value="opus">OPUS（默认）</option>
            <option value="aac">AAC</option>
            <option value="flac">FLAC</option>
            <option value="raw">RAW（PCM）</option>
          </select>
        </div>
        <div class="settings-save-row">
          <button class="save-btn" @click="emit('save')">保存</button>
          <span class="settings-help-inline">0 表示不限制 / 使用 server 默认值；音频编码保存后立即生效（热切换，不重启会话）</span>
        </div>
      </div>
    </template>
  </div>
</template>
