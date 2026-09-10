<script setup lang="ts">
// 连接二维码 / 占位（展示 + 画布渲染）。
// 画布渲染时机与拆分前一致：info 或 theme 变化并完成 DOM 更新后再绘制。
import { nextTick, onMounted, ref, watch } from "vue";
import QRCode from "qrcode";
import type { PairingInfo } from "../types";

const props = defineProps<{
  info: PairingInfo | null;
  theme: string;
}>();

const qrCanvas = ref<HTMLCanvasElement | null>(null);

function renderQRCanvas() {
  const info = props.info;
  if (!info || !qrCanvas.value) return;
  const isDark = props.theme === "dark";
  QRCode.toCanvas(qrCanvas.value, info.wifi_string, {
    width: 200,
    margin: 2,
    color: { dark: isDark ? "#e0e0e0" : "#333", light: isDark ? "#16213e" : "#fff" },
  }).catch((e) => console.error("QR render error:", e));
}

onMounted(renderQRCanvas);
watch([() => props.info, () => props.theme], () => {
  nextTick(() => renderQRCanvas());
});
</script>

<template>
  <!-- pairing qr code -->
  <div v-if="info" class="qr-section">
    <div class="section">连接二维码</div>
    <div class="qr-card">
      <canvas ref="qrCanvas" class="qr-canvas"></canvas>
      <div class="qr-info">
        <div class="qr-row">
          <span class="qr-label">地址</span>
          <span class="qr-value mono">{{ info.dns_id }}</span>
        </div>
        <div class="qr-row">
          <span class="qr-label">配对码</span>
          <span class="qr-value mono">{{ info.psk }}</span>
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
</template>
