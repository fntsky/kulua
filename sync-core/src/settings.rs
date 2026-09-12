//! 应用级设置持久化。
//!
//! config 文件（`%APPDATA%\Kulua\config.json` 等）是**唯一真源**，记录：
//! - 开机自启动意图（`autostart_enabled`，机制实现见 [`crate::autostart`]）
//! - scrcpy 编码参数（码率 / 分辨率上限 / 帧率上限 / 音频码率 / 音频编码器）
//!
//! 读取失败一律回退默认值，写入失败返回 `std::io::Error`，不 panic。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// scrcpy 视频码率默认值：8 Mbps（与 scrcpy 官方默认一致）。
pub const DEFAULT_VIDEO_BIT_RATE: u32 = 8_000_000;
/// scrcpy 音频码率默认值：128 kbps（与 scrcpy 官方默认一致）。
pub const DEFAULT_AUDIO_BIT_RATE: u32 = 128_000;
/// scrcpy 音频编码器默认值。
pub const DEFAULT_AUDIO_CODEC: &str = "opus";

fn default_video_bit_rate() -> u32 {
    DEFAULT_VIDEO_BIT_RATE
}

fn default_audio_bit_rate() -> u32 {
    DEFAULT_AUDIO_BIT_RATE
}

fn default_audio_codec() -> String {
    DEFAULT_AUDIO_CODEC.to_string()
}

fn default_icon_theme() -> String {
    "dark".to_string()
}

/// config 文件内容（全部字段带默认值，保证旧文件缺字段也能解析）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 用户是否希望开机自启动 daemon
    #[serde(default)]
    pub autostart_enabled: bool,
    /// scrcpy 视频码率（bps），0 = 不传该参数（用 scrcpy 默认 8M）
    #[serde(default = "default_video_bit_rate")]
    pub video_bit_rate: u32,
    /// scrcpy 最大分辨率（px），0 = 不限制
    #[serde(default)]
    pub video_max_size: u32,
    /// scrcpy 最大帧率，0 = 不限制
    #[serde(default)]
    pub video_max_fps: u32,
    /// scrcpy 音频码率（bps），0 = 不传该参数（用 scrcpy 默认 128k）
    #[serde(default = "default_audio_bit_rate")]
    pub audio_bit_rate: u32,
    /// scrcpy 音频编码器：opus / aac / flac / raw
    #[serde(default = "default_audio_codec")]
    pub audio_codec: String,
    /// 图标主题：dark（黑）/ light（白）——软件窗口图标与托盘图标共用。
    /// 按桌面面板/任务栏明暗选择：深色面板用 dark，浅色面板用 light 才看得清。
    #[serde(default = "default_icon_theme")]
    pub icon_theme: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            autostart_enabled: false,
            video_bit_rate: DEFAULT_VIDEO_BIT_RATE,
            video_max_size: 0,
            video_max_fps: 0,
            audio_bit_rate: DEFAULT_AUDIO_BIT_RATE,
            audio_codec: DEFAULT_AUDIO_CODEC.to_string(),
            icon_theme: "dark".to_string(),
        }
    }
}

/// 传给 scrcpy-server 的编码参数快照（部署时使用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrcpyParams {
    /// 视频码率（bps），0 = 不限制/默认
    pub video_bit_rate: u32,
    /// 最大分辨率（px），0 = 不限制
    pub video_max_size: u32,
    /// 最大帧率，0 = 不限制
    pub video_max_fps: u32,
    /// 音频码率（bps），0 = 默认
    pub audio_bit_rate: u32,
    /// 音频编码器：opus / aac / flac / raw
    pub audio_codec: String,
}

impl From<&AppConfig> for ScrcpyParams {
    fn from(cfg: &AppConfig) -> Self {
        Self {
            video_bit_rate: cfg.video_bit_rate,
            video_max_size: cfg.video_max_size,
            video_max_fps: cfg.video_max_fps,
            audio_bit_rate: cfg.audio_bit_rate,
            audio_codec: cfg.audio_codec.clone(),
        }
    }
}

impl Default for ScrcpyParams {
    fn default() -> Self {
        Self {
            video_bit_rate: DEFAULT_VIDEO_BIT_RATE,
            video_max_size: 0,
            video_max_fps: 0,
            audio_bit_rate: DEFAULT_AUDIO_BIT_RATE,
            audio_codec: DEFAULT_AUDIO_CODEC.to_string(),
        }
    }
}

/// 返回本应用的持久化 config 文件路径。
///
/// Windows 用 `%APPDATA%\Kulua\config.json`，其他平台回退到
/// `~/.config/kulua/config.json`；取不到则回退当前目录下的 `kulua.json`。
pub fn config_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .filter(|_| cfg!(windows))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("kulua"))
        })
        .map(|dir| dir.join("config.json"))
        .unwrap_or_else(|| PathBuf::from("kulua.json"))
}

/// 读取 config 文件。文件不存在或解析失败视为默认值。
pub fn read() -> AppConfig {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => AppConfig::default(),
    }
}

/// 写入 config 文件（配置目录不存在时创建）。
pub fn write(config: &AppConfig) -> Result<(), std::io::Error> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(config).map_err(std::io::Error::other)?;
    std::fs::write(&path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let cfg = AppConfig {
            autostart_enabled: true,
            video_bit_rate: 4_000_000,
            video_max_size: 1280,
            video_max_fps: 30,
            audio_bit_rate: 96_000,
            audio_codec: "aac".to_string(),
            icon_theme: "light".to_string(),
        };
        let text = serde_json::to_string(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&text).unwrap();
        assert!(back.autostart_enabled);
        assert_eq!(back.video_bit_rate, 4_000_000);
        assert_eq!(back.video_max_size, 1280);
        assert_eq!(back.video_max_fps, 30);
        assert_eq!(back.audio_bit_rate, 96_000);
        assert_eq!(back.audio_codec, "aac");
        assert_eq!(back.icon_theme, "light");
    }

    #[test]
    fn old_config_without_new_fields_parses_with_defaults() {
        // 兼容旧版只有 autostart_enabled 的 config 文件
        let text = r#"{"autostart_enabled": true}"#;
        let cfg: AppConfig = serde_json::from_str(text).unwrap();
        assert!(cfg.autostart_enabled);
        assert_eq!(cfg.video_bit_rate, DEFAULT_VIDEO_BIT_RATE);
        assert_eq!(cfg.video_max_size, 0);
        assert_eq!(cfg.video_max_fps, 0);
        assert_eq!(cfg.audio_bit_rate, DEFAULT_AUDIO_BIT_RATE);
        assert_eq!(cfg.audio_codec, DEFAULT_AUDIO_CODEC);
        assert_eq!(cfg.icon_theme, "dark");
    }

    #[test]
    fn scrcpy_params_from_config() {
        let cfg = AppConfig {
            video_bit_rate: 2_000_000,
            audio_codec: "flac".to_string(),
            ..Default::default()
        };
        let params = ScrcpyParams::from(&cfg);
        assert_eq!(params.video_bit_rate, 2_000_000);
        assert_eq!(params.audio_codec, "flac");
    }
}
