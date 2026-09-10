// App.vue 与子组件共享的类型（拆分前为 App.vue 内的局部 interface/type）
export interface DeviceInfo {
  uuid: string;
  serial: string;
  state: string;
  name: string;
}

export interface PairingInfo {
  dns_id: string;
  psk: string;
  wifi_string: string;
}

export interface DeviceConfig {
  clipboardSync: boolean;
  notificationSync: boolean;
  audioSync: boolean;
  volume: number;
}

// 设备上的一个可启动应用（app.list 结果项）
export interface AppInfo {
  package_name: string;
  label: string;
  system: boolean;
}

// adb 原始设备列表项（含 Offline/Unauthorized）
export interface AdbDevice {
  serial: string;
  state: string;
}

// ADB 列表行：由 App.vue 预计算好展示所需数据，AdbList 只负责渲染
export interface AdbRow {
  serial: string;
  state: string;
  stateText: string;
  name: string;
  sessionState?: string;
  sessionStateText: string;
  clickable: boolean;
}

// 融合窗口（UI 本地维护：open_app 成功时记录，fusion-window-closed 时移除）
export interface LocalFusionWindow {
  windowId: number;
  serial: string;
  label: string;
}

// 设置：开机自启动 + 编码参数
export interface SettingsState {
  autostartEnabled: boolean;
  autostartSupported: boolean;
  videoBitRate: number; // bps
  videoMaxSize: number; // px, 0=不限
  videoMaxFps: number; // fps, 0=不限
  audioBitRate: number; // bps
  audioCodec: string; // opus/aac/flac/raw
}

// daemon 返回的原始设置字段（snake_case）
export interface SettingsPayload {
  autostart_enabled: boolean;
  autostart_supported: boolean;
  video_bit_rate: number;
  video_max_size: number;
  video_max_fps: number;
  audio_bit_rate: number;
  audio_codec: string;
}

// 设置页本地编辑中的编码参数（Mbps / kbps 显示单位）
export interface ScrcpyDraft {
  videoBitRateMbps: number;
  videoMaxSize: number;
  videoMaxFps: number;
  audioBitRateKbps: number;
  audioCodec: string;
}
