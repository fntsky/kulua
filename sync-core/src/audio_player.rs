//! OPUS 音频解码 + 播放
//!
//! 接收 scrcpy 传来的完整 OPUS 帧，使用 `opus` crate 解码为 PCM i16 数据，
//! 转为 f32 后通过 `rodio::Player` 推送到系统默认音频设备播放。
//!
//! 延迟度量：`buffer_ms()` 报告播放队列积压时长（已喂入 − 已播放采样）。
//! rodio 队列无上限且无丢帧策略，任何一次消费慢于喂入都会让该值增长且不回落
//! ——这是音频高延迟的首要嫌疑。

use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rodio::MixerDeviceSink;

/// 播放队列积压阈值（ms）。超过即丢帧清空，防止延迟永久累积。
///
/// rodio 队列无上限且无丢帧策略：任何一次消费慢于喂入都会让积压
/// 只增不减（实测曾稳定在 ~280ms 平台，随后继续累积到 1s+）。
/// 基线延迟（server buffer 50ms + 编码 20-40ms + 传输 20-50ms + WASAPI 10-30ms）
/// 约 120-170ms，阈值 150ms 之上基本只有积压成分，丢之无损。
pub const AUDIO_BUFFER_MAX_MS: u64 = 150;

// ── PCM 数据源 ────────────────────────────────────────────────────

/// 包装 `Vec<f32>` 使其实现 `rodio::Source`，可喂给 `rodio::Player`。
///
/// `played` 与 `AudioPlayer::played_samples` 共享：每次产出采样时累加，
/// 供 `buffer_ms()` 计算播放进度（被音频线程消费时自然递增）。
struct PcmSource {
    data: Vec<f32>,
    pos: usize,
    channels: NonZeroU16,
    sample_rate: NonZeroU32,
    played: Arc<AtomicU64>,
}

impl PcmSource {
    fn new(
        data: Vec<f32>,
        channels: NonZeroU16,
        sample_rate: NonZeroU32,
        played: Arc<AtomicU64>,
    ) -> Self {
        Self {
            data,
            pos: 0,
            channels,
            sample_rate,
            played,
        }
    }
}

impl Iterator for PcmSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos < self.data.len() {
            let s = self.data[self.pos];
            self.pos += 1;
            self.played.fetch_add(1, Ordering::Relaxed);
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
    /// 累计喂入的采样数（f32，含双声道）
    fed_samples: Arc<AtomicU64>,
    /// 音频线程实际消费（播放）的采样数
    played_samples: Arc<AtomicU64>,
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

        let fed_samples = Arc::new(AtomicU64::new(0));
        let played_samples = Arc::new(AtomicU64::new(0));

        Ok(Self {
            decoder,
            _device_sink: device_sink,
            player,
            channels: NonZeroU16::new(CHANNELS).unwrap(),
            sample_rate: NonZeroU32::new(SAMPLE_RATE).unwrap(),
            fed_samples,
            played_samples,
        })
    }

    /// 喂入一帧裸 OPUS 数据。
    ///
    /// 解码后的 PCM 自动加入播放队列，无需手动触发。
    pub fn feed_frame(&mut self, data: &[u8]) -> Result<(), opus::Error> {
        // 缓冲区足够容纳任意有效 OPUS 帧：
        //   最大帧 120 ms @ 48 kHz × 2 ch = 11 520 个 i16 采样
        const MAX_SAMPLES: usize = 16_384;
        let mut pcm_i16 = vec![0i16; MAX_SAMPLES];

        let samples_per_channel = self.decoder.decode(data, &mut pcm_i16, false)?;
        let total = samples_per_channel * (self.channels.get() as usize);

        // 将 OPUS 解码输出的 i16 转为 rodio 所需的 f32 采样
        let mut pcm_f32 = Vec::with_capacity(total);
        for &s in pcm_i16[..total].iter() {
            pcm_f32.push(s as f32 / 32768.0);
        }

        let source = PcmSource::new(pcm_f32, self.channels, self.sample_rate, self.played_samples.clone());
        self.fed_samples.fetch_add(total as u64, Ordering::Relaxed);
        self.player.append(source);
        Ok(())
    }

    /// 当前播放队列积压时长（毫秒），即"缓冲延迟"。
    ///
    /// = (已喂入采样 − 已播放采样) / 总采样率(48k×2) × 1000。
    /// 音频线程消费慢于喂入时该值增长且不回落（队列无上限），
    /// 这是高延迟的首要指标。
    pub fn buffer_ms(&self) -> u64 {
        let fed = self.fed_samples.load(Ordering::Relaxed);
        let played = self.played_samples.load(Ordering::Relaxed);
        // 48 kHz × 2 ch = 96 000 采样/秒
        fed.saturating_sub(played) * 1000 / 96_000
    }

    /// 清空播放队列（丢帧用）。清除积压后恢复播放，等待新帧喂入。
    ///
    /// rodio 的 `clear()` 会暂停播放，必须随后 `play()` 恢复；
    /// 同时把 fed 对齐到 played，避免缓冲虚高。
    pub fn clear(&self) {
        self.player.clear();
        self.player.play();
        let played = self.played_samples.load(Ordering::Relaxed);
        self.fed_samples.store(played, Ordering::Relaxed);
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
