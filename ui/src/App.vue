<script setup lang="ts">
import { ref, computed, watch, onMounted, nextTick } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import QRCode from "qrcode";

interface DeviceInfo {
  uuid: string;
  serial: string;
  state: string;
  name: string;
}

interface PairingInfo {
  dns_id: string;
  psk: string;
  wifi_string: string;
}
interface DeviceConfig {
  clipboardSync: boolean;
  notificationSync: boolean;
  audioSync: boolean;
  volume: number;
}
const connected = ref(false);
const theme = ref(localStorage.getItem("theme") || "dark");
watch(theme, (v) => {
  localStorage.setItem("theme", v);
  nextTick(() => rerenderQR());
});
function toggleTheme() {
  theme.value = theme.value === "dark" ? "light" : "dark";
}
const devices = ref<DeviceInfo[]>([]);
// 当前页面：设备卡片 / ADB 连接
const view = ref<"devices" | "adb">("devices");
// adb 原始设备列表（含 Offline/Unauthorized）
const adbDevices = ref<Array<{ serial: string; state: string }>>([]);
const adbStateTextMap: Record<string, string> = {
  Device: "已连接",
  Offline: "离线",
  Unauthorized: "未授权",
};
function adbStateText(s: string): string {
  return adbStateTextMap[s] || s.replace(/^Unknown\(|\)$/g, "");
}
// adb 列表的设备名：匹配合并列表（name 来自 scrcpy 协议）
function adbName(serial: string): string {
  return devices.value.find((x) => x.serial === serial)?.name || "";
}
const error = ref("");
const pairingInfo = ref<PairingInfo | null>(null);
const qrCanvas = ref<HTMLCanvasElement | null>(null);
const deviceConfigs = ref<Record<string, DeviceConfig>>({});
// session 生命周期状态（来自 sessions-updated，按 uuid）
const sessionStates = ref<Record<string, string>>({});
// 音频缓冲延迟 ms（来自 sessions-updated，按 uuid）
const audioBuffers = ref<Record<string, number>>({});
const sessionStateText: Record<string, string> = {
  connecting: "连接中",
  running: "运行中",
  failed: "连接失败",
  stopped: "已停止",
};
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
// 会话总数（含 failed 墓碑），tab 徽标用
const sessionCount = computed(() => Object.keys(sessionStates.value).length);
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

async function renderQR(info: PairingInfo) {
  pairingInfo.value = info;
  await nextTick();
  renderQRCanvas();
}
function renderQRCanvas() {
  const info = pairingInfo.value;
  if (!info || !qrCanvas.value) return;
  const isDark = theme.value === "dark";
  QRCode.toCanvas(qrCanvas.value, info.wifi_string, {
    width: 200,
    margin: 2,
    color: { dark: isDark ? "#e0e0e0" : "#333", light: isDark ? "#16213e" : "#fff" },
  }).catch((e) => console.error("QR render error:", e));
}
function rerenderQR() {
  renderQRCanvas();
}

onMounted(async () => {
  listen<DeviceInfo[]>("devices-updated", (e) => {
    updateUI(connected.value, e.payload);
  });
  listen<boolean>("connection-changed", (e) => {
    if (e.payload) {
      invoke<DeviceInfo[]>("get_devices").then((d) => updateUI(true, d));
    } else {
      updateUI(false, []);
    }
  });
  listen<{ serial: string; state: string }[]>("adb-updated", (e) => {
    adbDevices.value = e.payload;
  });
  listen<PairingInfo>("pairing-info-updated", (e) => {
    renderQR(e.payload);
  });
  listen<{ sessions: Array<{ uuid: string; clipboard_sync: boolean; notification_sync: boolean; audio_enabled: boolean; volume: number; session_state: string; audio_buffer_ms: number }> }>("sessions-updated", (e) => {
    // 全量推送：同步重建状态表（列表外的 uuid 状态清除）
    const next: Record<string, string> = {};
    const nextBuffers: Record<string, number> = {};
    for (const s of e.payload.sessions) {
      deviceConfigs.value[s.uuid] = {
        clipboardSync: s.clipboard_sync,
        notificationSync: s.notification_sync,
        audioSync: s.audio_enabled,
        volume: s.volume,
      };
      next[s.uuid] = s.session_state;
      nextBuffers[s.uuid] = s.audio_buffer_ms;
    }
    sessionStates.value = next;
    audioBuffers.value = nextBuffers;
  });

  // load existing state
  refresh();
  invoke<Array<{ serial: string; state: string }>>("get_adb_devices")
    .then((d) => (adbDevices.value = d))
    .catch((e) => console.error("get_adb_devices failed:", e));
  const existing = await invoke<PairingInfo | null>("get_pairing_info");
  if (existing) {
    renderQR(existing);
  }
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
      </div>
      <div v-if="view === 'devices'" class="device-list">
        <div v-if="!connected" class="hint">正在连接 daemon…</div>
        <div v-else-if="devices.length === 0" class="hint">暂无在线设备</div>
        <div
          v-for="d in devices"
          :key="d.uuid"
          class="device-card"
        >
          <div class="device-header">
            <div class="device-title">
              <span class="device-name">{{ d.name || d.serial }}</span>
              <span class="serial">{{ d.serial }}</span>
            </div>
            <div class="device-states">
              <span class="state" :class="stateClass(d.state)">{{ d.state }}</span>
              <span v-if="sessionStates[d.uuid]" class="session-state" :class="'session-' + sessionStates[d.uuid]">
                {{ sessionStateText[sessionStates[d.uuid]] || sessionStates[d.uuid] }}
              </span>
              <span v-else class="session-state session-none">未建立会话</span>
            </div>
          </div>
          <div class="device-toggles">
            <div class="toggle-row" @click="toggleClipboardSync(d.uuid)">
              <div class="toggle-switch" :class="{ active: getDeviceConfig(d.uuid).clipboardSync }" />
              <span class="toggle-label">剪贴板</span>
            </div>
            <div class="toggle-row" @click="toggleNotificationSync(d.uuid)">
              <div class="toggle-switch" :class="{ active: getDeviceConfig(d.uuid).notificationSync }" />
              <span class="toggle-label">通知</span>
            </div>
            <div class="toggle-row" @click="toggleAudioSync(d.uuid)">
              <div class="toggle-switch" :class="{ active: getDeviceConfig(d.uuid).audioSync }" />
              <span class="toggle-label">音频</span>
              <span
                v-if="getDeviceConfig(d.uuid).audioSync && audioBuffers[d.uuid] > 0"
                class="audio-latency"
                :class="{ high: audioBuffers[d.uuid] >= 200 }"
                :title="'播放队列积压 ' + audioBuffers[d.uuid] + 'ms（延迟高时优先检查此项）'"
              >
                缓冲 {{ audioBuffers[d.uuid] }}ms
              </span>
            </div>
            <div class="toggle-row" style="gap:4px">
              <input
                class="volume-slider"
                type="range"
                min="0" max="100"
                :value="getDeviceConfig(d.uuid).volume"
                @input="setVolume(d.uuid, Number(($event.target as HTMLInputElement).value))"
              />
              <span class="toggle-label">{{ getDeviceConfig(d.uuid).volume }}</span>
            </div>
          </div>
          <div v-if="sessionStates[d.uuid] === 'failed'" class="retry-row">
            <button class="retry-btn" @click="retrySession(d.uuid)">重试</button>
          </div>
        </div>
      </div>
      <!-- adb raw device list -->
      <div v-else class="adb-list">
        <div v-if="!connected" class="hint">正在连接 daemon…</div>
        <div v-else-if="adbDevices.length === 0" class="hint">无 ADB 设备</div>
        <template v-else>
          <div class="adb-tip">点击在线设备建立会话（每台设备一个会话）</div>
          <div
            v-for="d in adbDevices"
            :key="d.serial"
            class="adb-row"
            :class="{ clickable: canStartAdbSession(d.serial, d.state) }"
            @click="startAdbSession(d.serial, d.state)"
          >
            <div class="device-title">
              <span class="device-name">{{ adbName(d.serial) || d.serial }}</span>
              <span class="serial">{{ d.serial }}</span>
            </div>
            <div class="adb-states">
              <span v-if="adbSessionState(d.serial)" class="session-state" :class="'session-' + adbSessionState(d.serial)">
                {{ adbSessionStateText(d.serial) }}
              </span>
              <span class="state" :class="stateClass(d.state)">{{ adbStateText(d.state) }}</span>
            </div>
          </div>
        </template>
      </div>
      <!-- error -->
      <div v-if="error" class="error">{{ error }}</div>
    </div>
    <div class="right-panel">
      <!-- pairing qr code -->
      <div v-if="pairingInfo" class="qr-section">
        <div class="section">连接二维码</div>
        <div class="qr-card">
          <canvas ref="qrCanvas" class="qr-canvas"></canvas>
          <div class="qr-info">
            <div class="qr-row">
              <span class="qr-label">地址</span>
              <span class="qr-value mono">{{ pairingInfo.dns_id }}</span>
            </div>
            <div class="qr-row">
              <span class="qr-label">配对码</span>
              <span class="qr-value mono">{{ pairingInfo.psk }}</span>
            </div>
          </div>
        </div>
      </div>
      <div v-else class="qr-placeholder">
        <div class="section">连接二维码</div>
        <div class="placeholder-card">
          <span class="placeholder-text">等待 daemon 提供配对信息…</span>
        </div>
      </div>
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
  </div>
</template>

<script lang="ts">
function stateClass(s: string): string {
  return s.replace(/[^a-zA-Z]/g, "");
}
</script>

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
.retry-row {
  display: flex; justify-content: flex-end;
}
.retry-btn {
  background: var(--red); color: #fff;
  border: none; border-radius: 4px;
  padding: 4px 14px; font-size: 12px; cursor: pointer;
}
.retry-btn:hover { opacity: 0.85; }
.device-toggles {
  display: flex; align-items: center; gap: 16px; padding-top: 4px;
  border-top: 1px solid var(--border);
}
.toggle-row {
  display: flex; align-items: center; gap: 8px; cursor: pointer;
  font-size: 12px; color: var(--dim);
}
.toggle-label { user-select: none; }
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
