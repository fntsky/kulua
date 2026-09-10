// 纯函数 / 常量（不依赖任何响应式状态），供 App.vue 与子组件共用

// 状态字符串 → class 后缀（如 Device/Offline/Unauthorized，Unknown(xxx) → Unknown）
export function stateClass(s: string): string {
  return s.replace(/[^a-zA-Z]/g, "");
}

// session 生命周期状态文案
export const sessionStateText: Record<string, string> = {
  connecting: "连接中",
  running: "运行中",
  failed: "连接失败",
  stopped: "已停止",
};
