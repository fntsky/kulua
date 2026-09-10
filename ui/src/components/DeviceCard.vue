<script setup lang="ts">
// 设备卡片（纯展示）：数据由 App.vue 经 props 传入，交互经 emits 回抛。
// 注意：本组件 DOM 结构与 class 名被 App.vue 的全局 <style> 选择器依赖，不可改动。
import type { DeviceConfig, DeviceInfo } from "../types";
import { sessionStateText, stateClass } from "../utils";

defineProps<{
  device: DeviceInfo;
  // getDeviceConfig(uuid) 的结果（App.vue 侧惰性创建并持有）
  config: DeviceConfig;
  sessionState?: string;
  audioState?: string;
  audioError?: string;
  audioBuffer: number;
  audioPending: boolean;
  fusionWindowCount: number;
}>();

const emit = defineEmits<{
  (e: "toggle-clipboard"): void;
  (e: "toggle-notification"): void;
  (e: "update-audio"): void;
  (e: "update-volume", volume: number): void;
  (e: "retry"): void;
  (e: "open-fusion"): void;
  (e: "disconnect"): void;
}>();
</script>

<template>
  <div class="device-card">
    <div class="device-header">
      <div class="device-title">
        <span class="device-name">{{ device.name || device.serial }}</span>
        <span class="serial">{{ device.serial }}</span>
      </div>
      <div class="device-states">
        <span class="state" :class="stateClass(device.state)">{{ device.state }}</span>
        <span v-if="sessionState" class="session-state" :class="'session-' + sessionState">
          {{ sessionStateText[sessionState] || sessionState }}
        </span>
        <span v-else class="session-state session-none">未建立会话</span>
      </div>
    </div>
    <div class="device-toggles">
      <div class="toggle-row" @click="emit('toggle-clipboard')">
        <div class="toggle-switch" :class="{ active: config.clipboardSync }" />
        <span class="toggle-label">剪贴板</span>
      </div>
      <div class="toggle-row" @click="emit('toggle-notification')">
        <div class="toggle-switch" :class="{ active: config.notificationSync }" />
        <span class="toggle-label">通知</span>
      </div>
      <div class="toggle-row" @click="emit('update-audio')">
        <div class="toggle-switch" :class="{ active: config.audioSync }" />
        <span class="toggle-label">音频</span>
        <span v-if="audioPending" class="audio-state audio-pending">切换中…</span>
        <span
          v-else-if="audioState === 'failed'"
          class="audio-state audio-failed"
          :title="audioError || '音频启用失败'"
        >
          音频失败
        </span>
        <span
          v-if="config.audioSync && audioBuffer > 0"
          class="audio-latency"
          :class="{ high: audioBuffer >= 200 }"
          :title="'播放队列积压 ' + audioBuffer + 'ms（延迟高时优先检查此项）'"
        >
          缓冲 {{ audioBuffer }}ms
        </span>
      </div>
      <div class="toggle-row" style="gap:4px">
        <input
          class="volume-slider"
          type="range"
          min="0" max="100"
          :value="config.volume"
          @input="emit('update-volume', Number(($event.target as HTMLInputElement).value))"
        />
        <span class="toggle-label volume-value">{{ config.volume }}</span>
      </div>
    </div>
    <!-- 会话操作行：与「应用」同一排；无会话时整行不渲染 -->
    <div v-if="sessionState" class="device-actions">
      <button v-if="sessionState === 'running'" class="apps-btn" @click="emit('open-fusion')">
        应用
      </button>
      <button v-if="sessionState === 'failed'" class="retry-btn" @click="emit('retry')">
        重试
      </button>
      <span v-if="fusionWindowCount > 0" class="fusion-count">
        {{ fusionWindowCount }} 个窗口
      </span>
      <button
        class="disconnect-btn"
        :title="'停止 ' + (device.name || device.serial) + ' 的会话（不断开 adb）'"
        @click="emit('disconnect')"
      >
        断开
      </button>
    </div>
  </div>
</template>
