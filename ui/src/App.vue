<script setup lang="ts">
import { ref, onMounted, nextTick } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import QRCode from "qrcode";

interface DeviceInfo {
  serial: string;
  state: string;
}

interface PairingInfo {
  dns_id: string;
  psk: string;
  wifi_string: string;
}

const connected = ref(false);
const devices = ref<DeviceInfo[]>([]);
const error = ref("");
const pairingInfo = ref<PairingInfo | null>(null);
const qrCanvas = ref<HTMLCanvasElement | null>(null);

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
  if (qrCanvas.value) {
    QRCode.toCanvas(qrCanvas.value, info.wifi_string, {
      width: 200,
      margin: 2,
      color: { dark: "#e0e0e0", light: "#16213e" },
    }).catch((e) => console.error("QR render error:", e));
  }
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
  listen<PairingInfo>("pairing-info-updated", (e) => {
    renderQR(e.payload);
  });

  // load existing state
  refresh();
  const existing = await invoke<PairingInfo | null>("get_pairing_info");
  if (existing) {
    renderQR(existing);
  }
});
</script>

<template>
  <div class="container">

    <!-- main content -->
    <div class="main">
      <h1>Sync Workspace</h1>

      <!-- pairing qr code -->
      <div v-if="pairingInfo" class="qr-section">
        <div class="section">连接二维码</div>
        <div class="qr-card">
          <canvas ref="qrCanvas" class="qr-canvas"></canvas>
          <div class="qr-info">
            <div class="qr-row">
              <span class="qr-label">DNS ID</span>
              <span class="qr-value mono">{{ pairingInfo.dns_id }}</span>
            </div>
            <div class="qr-row">
              <span class="qr-label">PSK</span>
              <span class="qr-value mono">{{ pairingInfo.psk }}</span>
            </div>
          </div>
        </div>
      </div>

      <!-- device list -->
      <div class="section">在线设备</div>
      <div class="device-list">
        <div v-if="!connected" class="hint">正在连接 daemon…</div>
        <div v-else-if="devices.length === 0" class="hint">暂无在线设备</div>
        <div
          v-for="d in devices"
          :key="d.serial"
          class="device-card"
        >
          <span class="serial">{{ d.serial }}</span>
          <span class="state" :class="stateClass(d.state)">{{ d.state }}</span>
        </div>
      </div>

      <!-- error -->
      <div v-if="error" class="error">{{ error }}</div>
    </div>

    <!-- status bar (bottom-right) -->
    <div class="status-bar">
      <span class="dot" :class="{ on: connected, off: !connected }" />
      <span class="label">{{ connected ? "已连接" : "未连接" }}</span>
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
  max-width: 480px;
  min-height: 100vh;
  margin: 0 auto;
  padding: 20px;
  display: flex;
  flex-direction: column;
}
.main { flex: 1; }
h1 { font-size: 18px; font-weight: 600; margin-bottom: 20px; color: #fff; }
.status-bar {
  display: flex; align-items: center; gap: 8px;
  justify-content: flex-end;
  padding: 12px 0;
}
.dot {
  width: 10px; height: 10px; border-radius: 50%; flex-shrink: 0;
}
.dot.on { background: var(--green); box-shadow: 0 0 8px var(--green); }
.dot.off { background: var(--red); box-shadow: 0 0 8px var(--red); }
.label { font-size: 14px; font-weight: 500; }
.error {
  color: var(--red); font-size: 13px; text-align: center; padding: 10px;
  background: #2a1a1a; border-radius: 8px; margin-bottom: 12px;
}
.section {
  font-size: 13px; font-weight: 600; text-transform: uppercase;
  letter-spacing: 0.5px; color: var(--dim); margin-bottom: 10px;
}
.device-list { display: flex; flex-direction: column; gap: 8px; }
.hint { color: var(--dim); font-size: 14px; text-align: center; padding: 30px 0; }
.device-card {
  background: var(--card); border-radius: 10px;
  padding: 12px 16px; display: flex; align-items: center; justify-content: space-between;
}
.serial {
  font-size: 13px; font-weight: 500;
  font-family: "Cascadia Code", "Fira Code", monospace;
}
.state {
  font-size: 12px; padding: 3px 10px; border-radius: 20px; font-weight: 500;
}
.state.Device { background: #1b5e20; color: #a5d6a7; }
.state.Offline { background: #b71c1c; color: #ef9a9a; }
.state.Unauthorized { background: #e65100; color: #ffcc80; }
.state.Unknown { background: #37474f; color: #b0bec5; }

/* QR code */
.qr-section { margin-bottom: 16px; }
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
</style>
