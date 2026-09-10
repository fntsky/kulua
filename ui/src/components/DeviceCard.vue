<script setup lang="ts">
// 设备卡片（纯展示）：数据由 App.vue 经 props 传入，交互经 emits 回抛。
// 注意：本组件 DOM 结构与 class 名被 App.vue 的全局 <style> 选择器依赖，不可改动。
import type { DeviceConfig, DeviceInfo } from "../types";
import { sessionStateText, stateClass } from "../utils";

const props = defineProps<{
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

// 断开：仅在有会话时可点（无会话直接本地返回，不产生 IPC 往返）。
// 成功不做本地状态改动，daemon 会推 sessions-updated 刷新卡片。
function onDisconnect() {
  if (!props.sessionState) return;
  emit("disconnect");
}
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
        <span class="toggle-label">{{ config.volume }}</span>
      </div>
      <!-- 断开：点亮 = 该设备已建立会话；无会话时置灰且点击不发事件 -->
      <div
        class="toggle-row"
        :class="{ disabled: !sessionState }"
        :title="sessionState ? '断开该设备的会话' : '未建立会话'"
        @click="onDisconnect"
      >
        <div class="toggle-switch" :class="{ active: !!sessionState }" />
        <span class="toggle-label">断开</span>
      </div>
    </div>
    <div v-if="sessionState === 'failed'" class="retry-row">
      <button class="retry-btn" @click="emit('retry')">重试</button>
    </div>
    <!-- 融合模式：session 运行中才能打开应用窗口 -->
    <div v-if="sessionState === 'running'" class="fusion-row">
      <button class="apps-btn" @click="emit('open-fusion')">
        应用
      </button>
      <span v-if="fusionWindowCount > 0" class="fusion-count">
        {{ fusionWindowCount }} 个窗口
      </span>
    </div>
  </div>
</template>
