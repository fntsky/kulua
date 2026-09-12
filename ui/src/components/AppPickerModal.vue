<script setup lang="ts">
// 应用选择器弹层（融合模式，纯展示）：应用列表/加载态由 App.vue 持有，搜索词与"隐藏系统应用"双向绑定。
import { computed } from "vue";
import type { AppInfo } from "../types";

const props = defineProps<{
  picker: { uuid: string; name: string };
  fusionSupported: boolean;
  apps: AppInfo[];
  appsLoading: boolean;
  appsError: string;
  openingApp: string | null;
  openFeedback: string;
}>();

const search = defineModel<string>("search", { required: true });
const hideSystem = defineModel<boolean>("hideSystem", { required: true });

const emit = defineEmits<{
  (e: "close"): void;
  (e: "refresh"): void;
  (e: "open-app", app: AppInfo): void;
}>();

// 搜索 + 隐藏系统应用过滤
const filteredApps = computed(() => {
  let list = props.apps;
  if (hideSystem.value) {
    list = list.filter((a) => !a.system);
  }
  const q = search.value.trim().toLowerCase();
  if (q) {
    list = list.filter(
      (a) => a.label.toLowerCase().includes(q) || a.package_name.toLowerCase().includes(q)
    );
  }
  return list;
});
</script>

<template>
  <div class="modal-overlay" @click.self="emit('close')">
    <div class="modal">
      <div class="modal-header">
        <span class="modal-title">打开应用 — {{ picker.name }}</span>
        <span class="modal-close" @click="emit('close')">✕</span>
      </div>
      <div v-if="!fusionSupported" class="modal-error">
        融合模式不可用：daemon 未就绪或设备不受支持（需要 Android 10+）。
      </div>
      <template v-else>
        <div class="modal-toolbar">
          <input
            class="modal-search"
            v-model="search"
            placeholder="搜索应用名 / 包名"
          />
          <label class="modal-hide">
            <input type="checkbox" v-model="hideSystem" /> 隐藏系统应用
          </label>
          <button class="modal-refresh" :disabled="appsLoading" @click="emit('refresh')">
            刷新
          </button>
        </div>
        <div v-if="appsLoading" class="modal-hint">正在枚举设备应用…</div>
        <div v-else-if="appsError" class="modal-error">{{ appsError }}</div>
        <div v-else-if="filteredApps.length === 0" class="modal-hint">没有匹配的应用</div>
        <div v-else class="modal-list">
          <div
            v-for="a in filteredApps"
            :key="a.package_name"
            class="modal-app"
            :class="{ system: a.system, disabled: openingApp !== null }"
            @click="emit('open-app', a)"
          >
            <div class="modal-app-name">
              {{ a.label || a.package_name }}
              <span v-if="openingApp === a.package_name" class="modal-opening">打开中…</span>
            </div>
            <div class="modal-app-pkg">
              {{ a.package_name }}
              <span v-if="a.system" class="modal-badge">系统</span>
            </div>
          </div>
        </div>
        <div v-if="openFeedback" class="modal-feedback">{{ openFeedback }}</div>
        <div class="modal-help">点击应用后以融合模式（独立窗口）打开，可同时打开多个</div>
      </template>
    </div>
  </div>
</template>
