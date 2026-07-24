#[derive(Debug, Clone, Copy)]
/// 每个 session 的独立配置。
pub struct SessionConfig {
    /// 剪贴板同步开关
    pub clipboard_sync: bool,
    /// 通知同步开关
    pub notification_sync: bool,
    /// 音频同步开关
    pub audio_enabled: bool,
    /// 音量百分比（0-100）
    pub volume: u16,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            clipboard_sync: true,
            notification_sync: true,
            audio_enabled: true,
            volume: 80,
        }
    }
}
