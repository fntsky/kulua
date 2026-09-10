<script setup lang="ts">
// ADB 原始设备列表（纯展示）：行数据由 App.vue 预计算，点击经 emits 回抛。
import type { AdbRow } from "../types";
import { stateClass } from "../utils";

defineProps<{
  connected: boolean;
  rows: AdbRow[];
}>();

const emit = defineEmits<{
  (e: "start-session", serial: string, state: string): void;
}>();
</script>

<template>
  <div class="adb-list">
    <div v-if="!connected" class="hint">正在连接 daemon…</div>
    <div v-else-if="rows.length === 0" class="hint">无 ADB 设备</div>
    <template v-else>
      <div class="adb-tip">点击在线设备建立会话（每台设备一个会话）</div>
      <div
        v-for="d in rows"
        :key="d.serial"
        class="adb-row"
        :class="{ clickable: d.clickable }"
        @click="emit('start-session', d.serial, d.state)"
      >
        <div class="device-title">
          <span class="device-name">{{ d.name || d.serial }}</span>
          <span class="serial">{{ d.serial }}</span>
        </div>
        <div class="adb-states">
          <span v-if="d.sessionState" class="session-state" :class="'session-' + d.sessionState">
            {{ d.sessionStateText }}
          </span>
          <span class="state" :class="stateClass(d.state)">{{ d.stateText }}</span>
        </div>
      </div>
    </template>
  </div>
</template>
