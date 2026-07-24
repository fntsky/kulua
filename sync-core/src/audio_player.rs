//! OPUS 音频解码 + 播放
//!
//! 接收 scrcpy 传来的完整 OPUS 帧，使用 `opus` crate 解码为 PCM i16 数据，
//! 转为 f32 后通过 `rodio::Player` 推送到系统默认音频设备播放。

use std::num::{NonZeroU16, NonZeroU32};
use std::time::Duration;

use rodio::MixerDeviceSink;

// ── PCM 数据源 ────────────────────────────────────────────────────

/// 包装 `Vec<f32>` 使其实现 `rodio::Source`，可喂给 `rodio::Player`。
struct PcmSource {
    data: Vec<f32>,
    pos: usize,
    channels: NonZeroU16,
    sample_rate: NonZeroU32,
}

impl PcmSource {
    fn new(data: Vec<f32>, channels: NonZeroU16, sample_rate: NonZeroU32) -> Self {
        Self {
            data,
            pos: 0,
            channels,
            sample_rate,
        }
    }
}

impl Iterator for PcmSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos < self.data.len() {
            let s = self.data[self.pos];
            self.pos += 1;
            Some(s)
        } else {
            None
        }
    }
}

impl rodio::Source for PcmSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> NonZeroU16 {
        self.channels
    }
    fn sample_rate(&self) -> NonZeroU32 {
        self.sample_rate
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

// ── 播放器 ────────────────────────────────────────────────────────

/// OPUS 音频播放器。
///
/// 持有 OPUS 解码器 + `rodio::Player` 播放队列。
/// 每帧经 `feed_frame` 喂入，解码为 PCM 后立即加入播放队列。
pub struct AudioPlayer {
    decoder: opus::Decoder,
    /// 保持设备输出流存活（drop 后播放停止）
    _device_sink: MixerDeviceSink,
    player: rodio::Player,
    channels: NonZeroU16,
    sample_rate: NonZeroU32,
}

impl AudioPlayer {
    /// 创建播放器。
    ///
    /// scrcpy 的 OPUS 音频流固定为 **48 kHz 立体声**。
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        const SAMPLE_RATE: u32 = 48_000;
        const CHANNELS: u16 = 2;

        let decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)?;

        // 打开系统默认音频输出设备
        let device_sink = rodio::DeviceSinkBuilder::open_default_sink()
            .map_err(|e| format!("failed to open audio output device: {e:?}"))?;
        // 将 Player 连接到设备 mixer，音频才能真正输出
        let player = rodio::Player::connect_new(&device_sink.mixer());

        Ok(Self {
            decoder,
            _device_sink: device_sink,
            player,
            channels: NonZeroU16::new(CHANNELS).unwrap(),
            sample_rate: NonZeroU32::new(SAMPLE_RATE).unwrap(),
        })
    }

    /// 喂入一帧裸 OPUS 数据。
    ///
    /// 解码后的 PCM 自动加入播放队列，无需手动触发。
    pub fn feed_frame(&mut self, data: &[u8]) -> Result<(), opus::Error> {
        // 缓冲区足够容纳任意有效 OPUS 帧：
        //   最大帧 120 ms @ 48 kHz × 2 ch = 11 520 个 i16 采样
        const MAX_SAMPLES: usize = 16_384;
        let mut pcm_i16 = vec![0i16; MAX_SAMPLES];

        let samples_per_channel = self.decoder.decode(data, &mut pcm_i16, false)?;
        let total = samples_per_channel * (self.channels.get() as usize);

        // 将 OPUS 解码输出的 i16 转为 rodio 所需的 f32 采样
        let mut pcm_f32 = Vec::with_capacity(total);
        for &s in pcm_i16[..total].iter() {
            pcm_f32.push(s as f32 / 32768.0);
        }

        let source = PcmSource::new(pcm_f32, self.channels, self.sample_rate);
        self.player.append(source);
        Ok(())
    }

    /// 清空播放队列（停止当前播放）。
    pub fn clear(&self) {
        self.player.clear();
    }

    /// 设置播放音量（0.0–1.0）。
    pub fn set_volume(&self, vol: f32) {
        self.player.set_volume(vol.clamp(0.0, 1.0));
    }

    /// 队列是否为空。
    pub fn empty(&self) -> bool {
        self.player.empty()
    }
}
