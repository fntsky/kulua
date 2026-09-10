<script setup lang="ts">
import { ref, computed, watch, onMounted } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AdbDevice,
  AdbRow,
  AppInfo,
  DeviceConfig,
  DeviceInfo,
  LocalFusionWindow,
  PairingInfo,
  ScrcpyDraft,
  SettingsPayload,
  SettingsState,
} from "./types";
import { sessionStateText } from "./utils";
import AdbList from "./components/AdbList.vue";
import AppPickerModal from "./components/AppPickerModal.vue";
import DeviceCard from "./components/DeviceCard.vue";
import PairingQr from "./components/PairingQr.vue";
import SettingsPanel from "./components/SettingsPanel.vue";

const connected = ref(false);
const theme = ref(localStorage.getItem("theme") || "dark");
watch(theme, (v) => {
  localStorage.setItem("theme", v);
  // 主题变化后的二维码重绘由 PairingQr 监听 theme prop 自行完成
});
function toggleTheme() {
  theme.value = theme.value === "dark" ? "light" : "dark";
}
function setTheme(mode: "dark" | "light") {
  theme.value = mode;
}
const devices = ref<DeviceInfo[]>([]);
// 当前页面：设备卡片 / ADB 连接 / 设置
const view = ref<"devices" | "adb" | "settings">("devices");
const settings = ref<SettingsState>({
  autostartEnabled: false,
  autostartSupported: false,
  videoBitRate: 8000000,
  videoMaxSize: 0,
  videoMaxFps: 0,
  audioBitRate: 128000,
  audioCodec: "opus",
});
// 设置页本地编辑中的编码参数（Mbps / kbps 显示单位）
const scrcpyDraft = ref<ScrcpyDraft>({
  videoBitRateMbps: 8,
  videoMaxSize: 0,
  videoMaxFps: 0,
  audioBitRateKbps: 128,
  audioCodec: "opus",
});
// 读取设置失败时的错误文案（不误显示“不支持”）
const settingsError = ref("");
function applySettings(s: SettingsPayload) {
  settings.value = {
    autostartEnabled: s.autostart_enabled,
    autostartSupported: s.autostart_supported,
    videoBitRate: s.video_bit_rate,
    videoMaxSize: s.video_max_size,
    videoMaxFps: s.video_max_fps,
    audioBitRate: s.audio_bit_rate,
    audioCodec: s.audio_codec,
  };
  scrcpyDraft.value = {
    videoBitRateMbps: s.video_bit_rate / 1_000_000,
    videoMaxSize: s.video_max_size,
    videoMaxFps: s.video_max_fps,
    audioBitRateKbps: s.audio_bit_rate / 1000,
    audioCodec: s.audio_codec,
  };
}
async function loadSettings() {
  try {
    const s = await invoke<SettingsPayload>("get_settings");
    applySettings(s);
    settingsError.value = "";
  } catch (e) {
    console.error("get_settings failed:", e);
    settingsError.value = `读取设置失败：${String(e)}`;
  }
}
async function toggleAutostart() {
  const target = !settings.value.autostartEnabled;
  try {
    const s = await invoke<SettingsPayload>("set_autostart", { enabled: target });
    applySettings(s);
    settingsError.value = "";
  } catch (e) {
    console.error("set_autostart failed:", e);
    settingsError.value = `设置自启动失败：${String(e)}`;
  }
}
// 保存编码参数（0 值 = 不限制 / 用 server 默认）
async function saveScrcpyParams() {
  const d = scrcpyDraft.value;
  const videoBitRate = Math.max(0, Math.round(d.videoBitRateMbps * 1_000_000));
  const videoMaxSize = Math.max(0, Math.round(d.videoMaxSize));
  const videoMaxFps = Math.max(0, Math.round(d.videoMaxFps));
  const audioBitRate = Math.max(0, Math.round(d.audioBitRateKbps * 1000));
  try {
    const s = await invoke<SettingsPayload>("set_scrcpy_params", {
      videoBitRate,
      videoMaxSize,
      videoMaxFps,
      audioBitRate,
      audioCodec: d.audioCodec,
    });
    applySettings(s);
    settingsError.value = "";
  } catch (e) {
    console.error("set_scrcpy_params failed:", e);
    settingsError.value = `保存编码参数失败：${String(e)}`;
  }
}
// adb 原始设备列表（含 Offline/Unauthorized）
const adbDevices = ref<AdbDevice[]>([]);
const adbStateTextMap: Record<string, string> = {
  Device: "已连接",
  Offline: "离线",
  Unauthorized: "未授权",
};
function adbStateText(s: string): string {
  return adbStateTextMap[s] || s.replace(/^Unknown\(|\)$/g, "");
}
// adb 列表的设备名：匹配合并列表（name 来自 session getprop ro.product.model）
function adbName(serial: string): string {
  return devices.value.find((x) => x.serial === serial)?.name || "";
}
const error = ref("");
const pairingInfo = ref<PairingInfo | null>(null);
const deviceConfigs = ref<Record<string, DeviceConfig>>({});
// session 生命周期状态（来自 sessions-updated，按 uuid）
const sessionStates = ref<Record<string, string>>({});
// 音频缓冲延迟 ms（来自 sessions-updated，按 uuid）
const audioBuffers = ref<Record<string, number>>({});
// 音频运行态（off | starting | on | stopping | failed）与失败原因
const audioStates = ref<Record<string, string>>({});
const audioErrors = ref<Record<string, string>>({});
// 切换中（命令已下发，等设备回执）：UI 开关已显示目标值，这里给出"尚未生效"反馈
function audioPending(uuid: string): boolean {
  const s = audioStates.value[uuid];
  return s === "starting" || s === "stopping";
}
function getDeviceConfig(uuid: string): DeviceConfig {
  if (!deviceConfigs.value[uuid]) {
    deviceConfigs.value[uuid] = {
      clipboardSync: true,
      notificationSync: true,
      audioSync: false,
      volume: 80,
    };
  }
  return deviceConfigs.value[uuid];
}

async function toggleClipboardSync(uuid: string) {
  const cfg = getDeviceConfig(uuid);
  cfg.clipboardSync = !cfg.clipboardSync;
  try {
    await invoke("update_session_config", {
      uuid,
      clipboardSync: cfg.clipboardSync,
      notificationSync: cfg.notificationSync,
      audioSync: cfg.audioSync,
      volume: cfg.volume,
    });
  } catch (e) {
    console.error("toggleClipboardSync failed:", e);
  }
}

async function toggleNotificationSync(uuid: string) {
  const cfg = getDeviceConfig(uuid);
  cfg.notificationSync = !cfg.notificationSync;
  try {
    await invoke("update_session_config", {
      uuid,
      clipboardSync: cfg.clipboardSync,
      notificationSync: cfg.notificationSync,
      audioSync: cfg.audioSync,
      volume: cfg.volume,
    });
  } catch (e) {
    console.error("toggleNotificationSync failed:", e);
  }
}

async function toggleAudioSync(uuid: string) {
  const cfg = getDeviceConfig(uuid);
  cfg.audioSync = !cfg.audioSync;
  try {
    await invoke("update_session_config", {
      uuid,
      clipboardSync: cfg.clipboardSync,
      notificationSync: cfg.notificationSync,
      audioSync: cfg.audioSync,
      volume: cfg.volume,
    });
  } catch (e) {
    console.error("toggleAudioSync failed:", e);
  }
}

async function setVolume(uuid: string, volume: number) {
  const cfg = getDeviceConfig(uuid);
  cfg.volume = Math.max(0, Math.min(100, Math.round(volume)));
  try {
    await invoke("update_session_config", {
      uuid,
      clipboardSync: cfg.clipboardSync,
      notificationSync: cfg.notificationSync,
      audioSync: cfg.audioSync,
      volume: cfg.volume,
    });
  } catch (e) {
    console.error("setVolume failed:", e);
  }
}

async function retrySession(uuid: string) {
  try {
    await invoke("retry_session", { uuid });
  } catch (e) {
    console.error("retrySession failed:", e);
  }
}

// ADB 行对应设备的 session 状态（经合并列表 uuid 关联，支持地址变更）
function adbSessionState(serial: string): string | undefined {
  const dev = devices.value.find((x) => x.serial === serial);
  return dev ? sessionStates.value[dev.uuid] : undefined;
}
// ADB 行 session 状态文案（无会话 → 空串）
function adbSessionStateText(serial: string): string {
  const s = adbSessionState(serial);
  return s ? sessionStateText[s] || s : "";
}
// 可建立会话：设备在线 且 无活跃（connecting/running）session；failed 墓碑允许重建
function canStartAdbSession(serial: string, state: string): boolean {
  if (state !== "Device") return false;
  const s = adbSessionState(serial);
  return s === undefined || s === "failed" || s === "stopped";
}
async function startAdbSession(serial: string, state: string) {
  if (!canStartAdbSession(serial, state)) return;
  try {
    await invoke("start_session", { serial });
  } catch (e) {
    console.error("start_session failed:", e);
  }
}
// ADB 列表的展示行（派生数据在此算好，AdbList 只负责渲染）
const adbRows = computed<AdbRow[]>(() =>
  adbDevices.value.map((d) => ({
    serial: d.serial,
    state: d.state,
    stateText: adbStateText(d.state),
    name: adbName(d.serial),
    sessionState: adbSessionState(d.serial),
    sessionStateText: adbSessionStateText(d.serial),
    clickable: canStartAdbSession(d.serial, d.state),
  }))
);
// 会话总数（含 failed 墓碑），tab 徽标用
const sessionCount = computed(() => Object.keys(sessionStates.value).length);
// ── 应用选择器（融合模式）──
// 当前打开选择器的设备（null = 关闭）
const appPicker = ref<{ uuid: string; name: string } | null>(null);
const apps = ref<AppInfo[]>([]);
const appsLoading = ref(false);
const appsError = ref("");
const appsSearch = ref("");
const hideSystemApps = ref(false);
// 正在打开的应用包名（点击后 loading 防重复）
const openingApp = ref<string | null>(null);
const openFeedback = ref("");
// 融合模式由 UI 进程内嵌 WebCodecs 渲染，恒可用
const fusionSupported = ref(true);
// 融合窗口列表（UI 本地维护：open_app 成功时记录，fusion-window-closed 时移除）
const fusionWindows = ref<LocalFusionWindow[]>([]);

function fusionWindowsOf(serial: string): LocalFusionWindow[] {
  return fusionWindows.value.filter((w) => w.serial === serial);
}

async function loadApps(force: boolean) {
  if (!appPicker.value) return;
  appsLoading.value = true;
  appsError.value = "";
  openFeedback.value = "";
  try {
    const r = await invoke<{ apps: AppInfo[]; fusion_supported: boolean }>("get_apps", {
      uuid: appPicker.value.uuid,
      force,
    });
    apps.value = r.apps;
    fusionSupported.value = r.fusion_supported;
  } catch (e) {
    console.error("get_apps failed:", e);
    appsError.value = String(e);
    apps.value = [];
  } finally {
    appsLoading.value = false;
  }
}

function openAppPicker(uuid: string, name: string) {
  appPicker.value = { uuid, name };
  apps.value = [];
  appsError.value = "";
  openFeedback.value = "";
  appsSearch.value = "";
  hideSystemApps.value = false;
  loadApps(false);
}

async function openApp(app: AppInfo) {
  if (!appPicker.value || openingApp.value) return;
  openingApp.value = app.package_name;
  openFeedback.value = "";
  try {
    const windowId = await invoke<number>("open_app", {
      uuid: appPicker.value.uuid,
      packageName: app.package_name,
    });
    // 本地记录窗口（serial 从设备列表取，用于按设备计数显示）
    const dev = devices.value.find((d) => d.uuid === appPicker.value!.uuid);
    fusionWindows.value.push({
      windowId,
      serial: dev?.serial ?? "",
      label: app.label || app.package_name,
    });
    openFeedback.value = `正在打开 ${app.label || app.package_name}…`;
  } catch (e) {
    console.error("open_app failed:", e);
    openFeedback.value = `打开失败：${String(e)}`;
  } finally {
    openingApp.value = null;
  }
}

function updateUI(conn: boolean, devs: DeviceInfo[]) {
  connected.value = conn;
  devices.value = devs;
  error.value = "";
}

function showError(msg: string) {
  error.value = msg;
  connected.value = false;
  devices.value = [];
}

async function refresh() {
  try {
    const [devs, conn] = await Promise.all([
      invoke<DeviceInfo[]>("get_devices"),
      invoke<boolean>("get_connection_status"),
    ]);
    updateUI(conn, devs);
  } catch (e) {
    showError(String(e));
  }
}

onMounted(async () => {
  listen<DeviceInfo[]>("devices-updated", (e) => {
    updateUI(connected.value, e.payload);
  });
  listen<boolean>("connection-changed", (e) => {
    if (e.payload) {
      invoke<DeviceInfo[]>("get_devices").then((d) => updateUI(true, d));
      loadSettings();
    } else {
      updateUI(false, []);
    }
  });
  listen<{ serial: string; state: string }[]>("adb-updated", (e) => {
    adbDevices.value = e.payload;
  });
  listen<PairingInfo>("pairing-info-updated", (e) => {
    pairingInfo.value = e.payload;
  });
  listen<string>("fusion-window-closed", (e) => {
    // 融合窗口销毁（Rust 侧 Destroyed 事件）→ 移出本地列表
    const closedId = Number(e.payload.replace("fusion-", ""));
    fusionWindows.value = fusionWindows.value.filter((w) => w.windowId !== closedId);
  });
  listen<{ sessions: Array<{ uuid: string; clipboard_sync: boolean; notification_sync: boolean; audio_enabled: boolean; audio_state: string; audio_error: string; volume: number; session_state: string; audio_buffer_ms: number }> }>("sessions-updated", (e) => {
    // 全量推送：同步重建状态表（列表外的 uuid 状态清除）
    const next: Record<string, string> = {};
    const nextBuffers: Record<string, number> = {};
    const nextAudioStates: Record<string, string> = {};
    const nextAudioErrors: Record<string, string> = {};
    for (const s of e.payload.sessions) {
      deviceConfigs.value[s.uuid] = {
        clipboardSync: s.clipboard_sync,
        notificationSync: s.notification_sync,
        // 开关呈现"目标值"：设备回执慢/失败也不回弹，另用状态标记呈现进度与错误
        audioSync: s.audio_enabled,
        volume: s.volume,
      };
      next[s.uuid] = s.session_state;
      nextBuffers[s.uuid] = s.audio_buffer_ms;
      nextAudioStates[s.uuid] = s.audio_state;
      nextAudioErrors[s.uuid] = s.audio_error;
    }
    sessionStates.value = next;
    audioBuffers.value = nextBuffers;
    audioStates.value = nextAudioStates;
    audioErrors.value = nextAudioErrors;
  });

  // load existing state
  refresh();
  invoke<Array<{ serial: string; state: string }>>("get_adb_devices")
    .then((d) => (adbDevices.value = d))
    .catch((e) => console.error("get_adb_devices failed:", e));
  const existing = await invoke<PairingInfo | null>("get_pairing_info");
  if (existing) {
    pairingInfo.value = existing;
  }
  loadSettings();
});
</script>

<template>
  <div class="container" :class="theme">
    <div class="left-panel">
      <!-- device list -->
      <div class="tabs">
        <span class="tab" :class="{ active: view === 'devices' }" @click="view = 'devices'">
          在线设备 <span class="tab-badge">{{ sessionCount }}</span>
        </span>
        <span class="tab" :class="{ active: view === 'adb' }" @click="view = 'adb'">
          ADB 连接 <span class="tab-badge">{{ adbDevices.length }}</span>
        </span>
        <span class="tab" :class="{ active: view === 'settings' }" @click="view = 'settings'">
          设置
        </span>
      </div>
      <div v-if="view === 'devices'" class="device-list">
        <div v-if="!connected" class="hint">正在连接 daemon…</div>
        <div v-else-if="devices.length === 0" class="hint">暂无在线设备</div>
        <DeviceCard
          v-for="d in devices"
          :key="d.uuid"
          :device="d"
          :config="getDeviceConfig(d.uuid)"
          :session-state="sessionStates[d.uuid]"
          :audio-state="audioStates[d.uuid]"
          :audio-error="audioErrors[d.uuid]"
          :audio-buffer="audioBuffers[d.uuid] ?? 0"
          :audio-pending="audioPending(d.uuid)"
          :fusion-window-count="fusionWindowsOf(d.serial).length"
          @toggle-clipboard="toggleClipboardSync(d.uuid)"
          @toggle-notification="toggleNotificationSync(d.uuid)"
          @update-audio="toggleAudioSync(d.uuid)"
          @update-volume="setVolume(d.uuid, $event)"
          @retry="retrySession(d.uuid)"
          @open-fusion="openAppPicker(d.uuid, d.name || d.serial)"
        />
      </div>
      <!-- adb raw device list -->
      <AdbList
        v-else-if="view === 'adb'"
        :connected="connected"
        :rows="adbRows"
        @start-session="startAdbSession"
      />
      <!-- 设置 -->
      <SettingsPanel
        v-else
        :connected="connected"
        :settings="settings"
        :draft="scrcpyDraft"
        :settings-error="settingsError"
        :theme="theme"
        @set-theme="setTheme"
        @toggle-autostart="toggleAutostart"
        @save="saveScrcpyParams"
      />
      <!-- error -->
      <div v-if="error" class="error">{{ error }}</div>
    </div>
    <div class="right-panel">
      <PairingQr :info="pairingInfo" :theme="theme" />
      <div class="status-bar">
        <span class="theme-btn" @click="toggleTheme" :title="theme === 'dark' ? '切换到白天模式' : '切换到黑夜模式'">
          <svg v-if="theme === 'dark'" xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="5"/>
            <line x1="12" y1="1" x2="12" y2="3"/>
            <line x1="12" y1="21" x2="12" y2="23"/>
            <line x1="4.22" y1="4.22" x2="5.64" y2="5.64"/>
            <line x1="18.36" y1="18.36" x2="19.78" y2="19.78"/>
            <line x1="1" y1="12" x2="3" y2="12"/>
            <line x1="21" y1="12" x2="23" y2="12"/>
            <line x1="4.22" y1="19.78" x2="5.64" y2="18.36"/>
            <line x1="18.36" y1="5.64" x2="19.78" y2="4.22"/>
          </svg>
          <svg v-else xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/>
          </svg>
        </span>
        <span class="dot" :class="{ on: connected, off: !connected }" />
      </div>
    </div>

    <!-- 应用选择器（融合模式弹层） -->
    <AppPickerModal
      v-if="appPicker"
      :picker="appPicker"
      :fusion-supported="fusionSupported"
      :apps="apps"
      :apps-loading="appsLoading"
      :apps-error="appsError"
      :opening-app="openingApp"
      :open-feedback="openFeedback"
      v-model:search="appsSearch"
      v-model:hide-system="hideSystemApps"
      @close="appPicker = null"
      @refresh="loadApps(true)"
      @open-app="openApp"
    />
  </div>
</template>

<style>
:root {
  --bg: #1a1a2e;
  --card: #16213e;
  --text: #e0e0e0;
  --dim: #888;
  --green: #4caf50;
  --red: #e94560;
  --border: rgba(255,255,255,0.08);
  --toggle-bg: #444;
  --toggle-knob: #888;
  --error-bg: #2a1a1a;
  --device-name: #fff;
  --slider-bg: #444;
  color-scheme: dark;
}
.container.light {
  --bg: #f5f5f5;
  --card: #ffffff;
  --text: #333;
  --dim: #999;
  --border: rgba(0,0,0,0.1);
  --toggle-bg: #ccc;
  --toggle-knob: #f5f5f5;
  --error-bg: #ffe0e0;
  --device-name: #1a1a1a;
  --slider-bg: #ccc;
  color-scheme: light;
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  background: var(--bg);
  color: var(--text);
  padding: 0;
  user-select: none;
}
.container {
  display: flex;
  flex-direction: row;
  height: 100vh;
  padding: 0;
  background: var(--bg);
}
.left-panel {
  flex: 1;
  display: flex;
  flex-direction: column;
  padding: 20px;
  border-right: 1px solid var(--border);
  overflow-y: auto;
  gap: 10px;
}
.right-panel {
  flex: 1;
  display: flex;
  flex-direction: column;
  padding: 20px;
  gap: 16px;
}
.status-bar {
  display: flex; align-items: center; gap: 8px;
  margin-top: auto;
  padding: 8px 0 0;
}
.theme-btn {
  cursor: pointer; font-size: 16px; line-height: 1;
  padding: 2px 4px; border-radius: 4px;
  user-select: none;
}
.theme-btn:hover { background: var(--border); }
.error {
  color: var(--red); font-size: 13px; text-align: center; padding: 10px;
  background: var(--error-bg); border-radius: 8px; margin-bottom: 12px;
}
.section {
  font-size: 13px; font-weight: 600; text-transform: uppercase;
  letter-spacing: 0.5px; color: var(--dim); margin-bottom: 10px;
}
.device-list { display: flex; flex-direction: column; gap: 8px; }
.tabs {
  display: flex; gap: 8px;
}
.tab {
  font-size: 12px; padding: 4px 14px; border-radius: 6px;
  color: var(--dim); cursor: pointer; user-select: none;
  border: 1px solid var(--border);
}
.tab.active { color: var(--text); background: var(--border); }
.tab-badge {
  display: inline-block;
  min-width: 16px;
  padding: 0 5px;
  margin-left: 4px;
  font-size: 10px;
  line-height: 15px;
  text-align: center;
  border-radius: 8px;
  background: var(--toggle-bg);
  color: var(--text);
}
.tab.active .tab-badge { background: var(--green); color: #fff; }
.adb-list { display: flex; flex-direction: column; gap: 8px; }
.adb-tip { font-size: 12px; color: var(--dim); text-align: center; }
.adb-row {
  background: var(--card); border-radius: 10px;
  padding: 12px 16px;
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
}
.adb-row.clickable { cursor: pointer; }
.adb-row.clickable:hover { outline: 1px solid var(--green); }
.adb-states {
  display: flex; flex-direction: column; align-items: flex-end; gap: 4px;
}
.session-none { color: var(--dim); }
.hint { color: var(--dim); font-size: 14px; text-align: center; padding: 30px 0; }
.device-card {
  background: var(--card); border-radius: 10px;
  padding: 12px 16px; display: flex; flex-direction: column; gap: 8px;
}
.device-header {
  display: flex; align-items: center; justify-content: space-between;
}
.device-states {
  display: flex; flex-direction: column; align-items: flex-end; gap: 2px;
}
.session-state {
  font-size: 11px; color: var(--dim); font-weight: 500;
}
.session-connecting { color: #f0c040; }
.session-running { color: var(--green); }
.session-failed { color: var(--red); }
.audio-latency {
  font-size: 11px; color: var(--dim); font-weight: 500;
}
.audio-latency.high {
  color: var(--red);
}
.audio-state {
  font-size: 11px; font-weight: 500;
}
.audio-pending { color: #f0c040; }
.audio-failed { color: var(--red); }
.retry-row {
  display: flex; justify-content: flex-end;
}
.retry-btn {
  background: var(--red); color: #fff;
  border: none; border-radius: 4px;
  padding: 4px 14px; font-size: 12px; cursor: pointer;
}
.retry-btn:hover { opacity: 0.85; }
/* 融合模式：应用按钮 + 窗口计数 */
.fusion-row {
  display: flex; align-items: center; gap: 8px;
  border-top: 1px solid var(--border); padding-top: 8px;
}
.apps-btn {
  background: var(--green); color: #fff;
  border: none; border-radius: 4px;
  padding: 4px 16px; font-size: 12px; cursor: pointer;
}
.apps-btn:hover { opacity: 0.85; }
.fusion-count { font-size: 11px; color: var(--dim); }
/* 应用选择器弹层 */
.modal-overlay {
  position: fixed; inset: 0;
  background: rgba(0, 0, 0, 0.55);
  display: flex; align-items: center; justify-content: center;
  z-index: 100;
}
.modal {
  width: 420px; max-width: 90vw; max-height: 80vh;
  background: var(--card); border-radius: 12px;
  padding: 16px; display: flex; flex-direction: column; gap: 10px;
  border: 1px solid var(--border);
}
.modal-header {
  display: flex; align-items: center; justify-content: space-between;
}
.modal-title { font-size: 14px; font-weight: 600; color: var(--text); }
.modal-close {
  cursor: pointer; color: var(--dim); font-size: 14px; padding: 2px 6px;
}
.modal-close:hover { color: var(--text); }
.modal-toolbar {
  display: flex; align-items: center; gap: 8px;
}
.modal-search {
  flex: 1; padding: 5px 8px; font-size: 12px;
  color: var(--text); background: var(--toggle-bg);
  border: 1px solid var(--border); border-radius: 6px; outline: none;
}
.modal-search:focus { border-color: var(--green); }
.modal-hide {
  font-size: 12px; color: var(--dim); display: flex; align-items: center; gap: 4px;
  cursor: pointer; user-select: none; white-space: nowrap;
}
.modal-refresh {
  background: var(--toggle-bg); color: var(--text);
  border: 1px solid var(--border); border-radius: 6px;
  padding: 4px 10px; font-size: 12px; cursor: pointer;
}
.modal-refresh:hover { border-color: var(--green); }
.modal-refresh:disabled { opacity: 0.5; cursor: default; }
.modal-hint { color: var(--dim); font-size: 13px; text-align: center; padding: 24px 0; }
.modal-error {
  color: var(--red); font-size: 12px; padding: 8px 12px;
  background: var(--error-bg); border-radius: 8px;
}
.modal-list {
  overflow-y: auto; max-height: 45vh;
  display: flex; flex-direction: column; gap: 4px;
}
.modal-app {
  padding: 8px 10px; border-radius: 8px; cursor: pointer;
  display: flex; flex-direction: column; gap: 2px;
}
.modal-app:hover { background: var(--toggle-bg); }
.modal-app.disabled { opacity: 0.6; cursor: default; }
.modal-app-name {
  font-size: 13px; font-weight: 500; color: var(--text);
  display: flex; align-items: center; gap: 6px;
}
.modal-app-pkg { font-size: 11px; color: var(--dim); font-family: "Cascadia Code", monospace; }
.modal-badge {
  font-size: 10px; color: var(--dim); padding: 1px 6px;
  border-radius: 8px; background: var(--toggle-bg); font-family: inherit;
}
.modal-opening { font-size: 11px; color: var(--green); }
.modal-feedback {
  font-size: 12px; color: var(--text); text-align: center;
  padding: 6px; border-radius: 6px; background: var(--toggle-bg);
}
.modal-help { font-size: 11px; color: var(--dim); text-align: center; }
.device-toggles {
  display: flex; align-items: center; gap: 16px; padding-top: 4px;
  border-top: 1px solid var(--border);
}
.toggle-row {
  display: flex; align-items: center; gap: 8px; cursor: pointer;
  font-size: 12px; color: var(--dim);
}
.toggle-row.disabled { cursor: not-allowed; opacity: 0.5; }
.toggle-label { user-select: none; }
.settings-panel { display: flex; flex-direction: column; gap: 10px; }
.settings-card {
  background: var(--card); border-radius: 10px;
  padding: 14px 16px;
  display: flex; flex-direction: column; gap: 10px;
}
.theme-segment {
  display: inline-flex; gap: 4px; margin-left: auto;
  padding: 3px; border-radius: 8px; background: var(--toggle-bg);
}
.theme-option {
  font-size: 12px; padding: 3px 14px; border-radius: 6px;
  color: var(--dim); cursor: pointer; user-select: none;
}
.theme-option.active {
  color: #fff; background: var(--green);
}
.settings-help {
  font-size: 11px; color: var(--dim); line-height: 1.6;
  border-top: 1px solid var(--border); padding-top: 10px;
}
.settings-note {
  font-size: 11px; color: var(--red); margin-left: auto;
}
.settings-error {
  font-size: 12px; color: var(--red); padding: 8px 12px;
  background: var(--error-bg); border-radius: 8px;
}
.settings-field {
  display: flex; align-items: center; justify-content: space-between;
  gap: 8px; font-size: 12px; color: var(--dim);
}
.settings-field-label { user-select: none; }
.settings-input {
  width: 110px; padding: 4px 8px;
  font-size: 12px; color: var(--text);
  background: var(--toggle-bg); border: 1px solid var(--border);
  border-radius: 6px; outline: none;
}
.settings-input:focus { border-color: var(--green); }
.settings-save-row {
  display: flex; align-items: center; gap: 8px;
  border-top: 1px solid var(--border); padding-top: 10px;
}
.save-btn {
  background: var(--green); color: #fff;
  border: none; border-radius: 6px;
  padding: 5px 18px; font-size: 12px; cursor: pointer;
}
.save-btn:hover { opacity: 0.85; }
.settings-help-inline { font-size: 11px; color: var(--dim); }
.toggle-switch {
  position: relative; width: 36px; height: 20px;
  background: var(--toggle-bg); border-radius: 10px; transition: 0.2s; cursor: pointer;
  flex-shrink: 0;
}
.toggle-switch::after {
  content: ''; position: absolute; top: 2px; left: 2px;
  width: 16px; height: 16px; border-radius: 50%;
  background: var(--toggle-knob); transition: 0.2s;
}
.toggle-switch.active { background: var(--green); }
.toggle-switch.active::after {
  left: 18px; background: #fff;
}
.serial {
  font-size: 11px; font-weight: 400; color: var(--dim);
  font-family: "Cascadia Code", "Fira Code", monospace;
}
.device-title {
  display: flex; flex-direction: column; gap: 1px;
  min-width: 0;
}
.device-name {
  font-size: 14px; font-weight: 600; color: var(--device-name);
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.state {
  font-size: 12px; padding: 3px 10px; border-radius: 20px; font-weight: 500;
}

.volume-slider {
  -webkit-appearance: none;
  appearance: none;
  width: 80px;
  height: 4px;
  border-radius: 2px;
  background: var(--slider-bg);
  outline: none;
  cursor: pointer;
  flex-shrink: 0;
}
.volume-slider::-webkit-slider-thumb {
  -webkit-appearance: none;
  appearance: none;
  width: 14px;
  height: 14px;
  border-radius: 50%;
  background: var(--green);
  cursor: pointer;
}
.volume-slider::-moz-range-thumb {
  width: 14px;
  height: 14px;
  border-radius: 50%;
  background: var(--green);
  cursor: pointer;
}
.state.Device { background: #1b5e20; color: #a5d6a7; }
.state.Offline { background: #b71c1c; color: #ef9a9a; }
.state.Unauthorized { background: #e65100; color: #ffcc80; }
.state.Unknown { background: #37474f; color: #b0bec5; }
/* QR code */
.qr-section { margin-bottom: 0; }
.qr-card {
  background: var(--card); border-radius: 12px; padding: 20px;
  display: flex; flex-direction: column; align-items: center; gap: 14px;
}
.qr-canvas { border-radius: 8px; display: block; }
.qr-info { width: 100%; }
.qr-row {
  display: flex; justify-content: space-between; align-items: center;
  padding: 6px 0; font-size: 12px;
}
.qr-label { color: var(--dim); }
.qr-value { color: var(--text); font-weight: 500; }
.mono { font-family: "Cascadia Code", "Fira Code", monospace; }
.qr-placeholder { flex: 1; display: flex; flex-direction: column; }
.placeholder-card {
  background: var(--card); border-radius: 12px; padding: 20px;
  display: flex; align-items: center; justify-content: center;
  min-height: 120px;
}
.placeholder-text { color: var(--dim); font-size: 13px; text-align: center; }
</style>
