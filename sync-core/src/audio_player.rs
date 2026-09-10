//! 音频解码 + 播放（多编码器支持）
//!
//! 接收 scrcpy 传来的音频帧，按编码器解码为 PCM f32 数据，通过 `rodio::Player`
//! 推送到系统默认音频设备播放。
//!
//! 支持的编码器：
//! - **OPUS**（默认）：`opus` crate 解码，固定 48 kHz 立体声
//! - **AAC**：symphonia `AacDecoder`，需要 codec config 包（AudioSpecificConfig）
//! - **FLAC**：symphonia `FlacDecoder`，需要 codec config 包（STREAMINFO）
//! - **RAW**：S16LE 裸 PCM（48 kHz 立体声，scrcpy 固定），直接转 f32
//!
//! 延迟度量：`buffer_ms()` 报告播放队列积压时长（已喂入 − 已播放采样）。
//! rodio 队列无上限且无丢帧策略，任何一次消费慢于喂入都会让该值增长且不回落
//! ——这是音频高延迟的首要嫌疑。

use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rodio::MixerDeviceSink;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_AAC, CODEC_TYPE_FLAC, CodecParameters, DecoderOptions};
use symphonia::core::formats::Packet;

/// 播放队列积压阈值（ms）。超过即丢帧清空，防止延迟永久累积。
///
/// rodio 队列无上限且无丢帧策略：任何一次消费慢于喂入都会让积压
/// 只增不减（实测曾稳定在 ~280ms 平台，随后继续累积到 1s+）。
/// 基线延迟（server buffer 50ms + 编码 20-40ms + 传输 20-50ms + WASAPI 10-30ms）
/// 约 120-170ms，阈值 150ms 之上基本只有积压成分，丢之无损。
pub const AUDIO_BUFFER_MAX_MS: u64 = 150;

/// 支持的音频编码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Opus,
    Aac,
    Flac,
    Raw,
}

impl AudioCodec {
    /// 由 scrcpy 音频流 codec ID（大端 u32）识别编码器。
    pub fn from_codec_id(id: u32) -> Option<Self> {
        match id {
            0x6f707573 => Some(Self::Opus),
            0x00616163 => Some(Self::Aac),
            0x666c6163 => Some(Self::Flac),
            0x00726177 => Some(Self::Raw),
            _ => None,
        }
    }

    /// 配置名（与 scrcpy `audio_codec` 选项一致）。
    pub fn name(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Aac => "aac",
            Self::Flac => "flac",
            Self::Raw => "raw",
        }
    }

    /// 由配置名解析（无效名回退 raw，与 server 端白名单一致）。
    pub fn from_name(name: &str) -> Self {
        match name {
            "opus" => Self::Opus,
            "aac" => Self::Aac,
            "flac" => Self::Flac,
            _ => Self::Raw,
        }
    }

    /// 音频连接握手指令中的 1B 编码索引（0=raw 1=opus 2=aac 3=flac）。
    pub fn handshake_byte(self) -> u8 {
        match self {
            Self::Raw => 0,
            Self::Opus => 1,
            Self::Aac => 2,
            Self::Flac => 3,
        }
    }

    /// [`Self::handshake_byte`] 的逆：索引 → 编码器（未知索引返回 None）。
    pub fn from_handshake_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Raw),
            1 => Some(Self::Opus),
            2 => Some(Self::Aac),
            3 => Some(Self::Flac),
            _ => None,
        }
    }

    /// 是否需要 codec config（AudioSpecificConfig / STREAMINFO）才能建解码器。
    pub fn needs_config(self) -> bool {
        matches!(self, Self::Aac | Self::Flac)
    }
}

/// 解码器统一接口：把一帧编码数据解码为 f32 交错 PCM。
enum Decoder {
    Opus(opus::Decoder),
    Symphonia(Box<dyn symphonia::core::codecs::Decoder>),
    /// RAW 无需解码，直接按 S16LE 交错 PCM 转 f32。
    Raw,
}

/// 解码输出：交错 f32 采样 + 规格。
struct Decoded {
    samples: Vec<f32>,
    channels: NonZeroU16,
    sample_rate: NonZeroU32,
}

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

/// 音频播放器（支持 OPUS / AAC / FLAC / RAW）。
///
/// 持有解码器 + `rodio::Player` 播放队列。
/// 每帧经 `feed_frame` 喂入，解码为 PCM 后立即加入播放队列。
pub struct AudioPlayer {
    decoder: Decoder,
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
    /// - `codec`: 音频编码器
    /// - `config`: codec config 包（AAC 的 AudioSpecificConfig / FLAC 的 STREAMINFO；
    ///   OPUS 与 RAW 不需要，传 None）
    pub fn new(
        codec: AudioCodec,
        config: Option<&[u8]>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        const SAMPLE_RATE: u32 = 48_000;
        const CHANNELS: u16 = 2;

        let decoder = match codec {
            AudioCodec::Opus => {
                Decoder::Opus(opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)?)
            }
            AudioCodec::Aac => Decoder::Symphonia(make_symphonia_decoder(CODEC_TYPE_AAC, config)?),
            AudioCodec::Flac => {
                Decoder::Symphonia(make_symphonia_decoder(CODEC_TYPE_FLAC, config)?)
            }
            AudioCodec::Raw => Decoder::Raw,
        };

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

    /// 喂入一帧编码音频数据。
    ///
    /// 解码后的 PCM 自动加入播放队列，无需手动触发。
    pub fn feed_frame(
        &mut self,
        data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let decoded = match &mut self.decoder {
            Decoder::Opus(dec) => decode_opus(dec, data, self.channels)?,
            Decoder::Symphonia(dec) => decode_symphonia(dec.as_mut(), data)?,
            Decoder::Raw => decode_raw(data)?,
        };
        let total = decoded.samples.len();
        if total == 0 {
            return Ok(());
        }
        self.channels = decoded.channels;
        self.sample_rate = decoded.sample_rate;

        let source = PcmSource::new(
            decoded.samples,
            self.channels,
            self.sample_rate,
            self.played_samples.clone(),
        );
        self.fed_samples.fetch_add(total as u64, Ordering::Relaxed);
        self.player.append(source);
        Ok(())
    }

    /// 当前播放队列积压时长（毫秒），即"缓冲延迟"。
    ///
    /// = (已喂入采样 − 已播放采样) / 采样率 × 声道数 × 1000。
    /// 音频线程消费慢于喂入时该值增长且不回落（队列无上限），
    /// 这是高延迟的首要指标。
    pub fn buffer_ms(&self) -> u64 {
        let fed = self.fed_samples.load(Ordering::Relaxed);
        let played = self.played_samples.load(Ordering::Relaxed);
        let per_second = self.sample_rate.get() as u64 * self.channels.get() as u64;
        if per_second == 0 {
            return 0;
        }
        fed.saturating_sub(played) * 1000 / per_second
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

// ── 解码实现 ──────────────────────────────────────────────────────

/// OPUS：解码为 f32 交错 PCM（固定 48 kHz 立体声）。
fn decode_opus(
    decoder: &mut opus::Decoder,
    data: &[u8],
    channels: NonZeroU16,
) -> Result<Decoded, Box<dyn std::error::Error + Send + Sync>> {
    // 缓冲区足够容纳任意有效 OPUS 帧：
    //   最大帧 120 ms @ 48 kHz × 2 ch = 11 520 个 i16 采样
    const MAX_SAMPLES: usize = 16_384;
    let mut pcm_i16 = vec![0i16; MAX_SAMPLES];

    let samples_per_channel = decoder.decode(data, &mut pcm_i16, false)?;
    let total = samples_per_channel * (channels.get() as usize);

    // 将 OPUS 解码输出的 i16 转为 rodio 所需的 f32 采样
    let mut pcm_f32 = Vec::with_capacity(total);
    for &s in pcm_i16[..total].iter() {
        pcm_f32.push(s as f32 / 32768.0);
    }

    Ok(Decoded {
        samples: pcm_f32,
        channels,
        sample_rate: NonZeroU32::new(48_000).unwrap(),
    })
}

/// 用 symphonia 注册表创建 AAC / FLAC 解码器。
fn make_symphonia_decoder(
    codec: symphonia::core::codecs::CodecType,
    config: Option<&[u8]>,
) -> Result<Box<dyn symphonia::core::codecs::Decoder>, Box<dyn std::error::Error + Send + Sync>> {
    let mut params = CodecParameters::new();
    params.codec = codec;
    if let Some(cfg) = config {
        params.extra_data = Some(cfg.to_vec().into_boxed_slice());
    }
    let options = DecoderOptions { verify: false };
    let registry = symphonia::default::get_codecs();
    registry.make(&params, &options).map_err(|e| {
        Box::<dyn std::error::Error + Send + Sync>::from(format!(
            "failed to create codec decoder: {e}"
        ))
    })
}

/// AAC / FLAC：symphonia 解码为 f32 交错 PCM。
fn decode_symphonia(
    decoder: &mut dyn symphonia::core::codecs::Decoder,
    data: &[u8],
) -> Result<Decoded, Box<dyn std::error::Error + Send + Sync>> {
    let packet = Packet::new_from_slice(0, 0, 0, data);
    let decoded = decoder
        .decode(&packet)
        .map_err(|e| format!("symphonia decode error: {e}"))?;

    let spec = decoded.spec().clone();
    let n_frames = decoded.frames();
    let mut sample_buf = SampleBuffer::<f32>::new(n_frames as u64 + 1, spec);
    sample_buf.copy_interleaved_ref(decoded);
    let samples = sample_buf.samples().to_vec();

    let channels =
        NonZeroU16::new(spec.channels.count() as u16).ok_or("decoded audio has 0 channels")?;
    let sample_rate = NonZeroU32::new(spec.rate).ok_or("decoded audio has 0 sample rate")?;

    Ok(Decoded {
        samples,
        channels,
        sample_rate,
    })
}

/// RAW：scrcpy 固定 48 kHz 立体声 S16LE，直接转 f32。
fn decode_raw(data: &[u8]) -> Result<Decoded, Box<dyn std::error::Error + Send + Sync>> {
    // S16LE 交错 PCM；丢弃不足 2 字节的尾部
    let n = data.len() / 2;
    let mut samples = Vec::with_capacity(n);
    for chunk in data[..n * 2].chunks_exact(2) {
        let s = i16::from_le_bytes([chunk[0], chunk[1]]);
        samples.push(s as f32 / 32768.0);
    }
    Ok(Decoded {
        samples,
        channels: NonZeroU16::new(2).unwrap(),
        sample_rate: NonZeroU32::new(48_000).unwrap(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_id_mapping() {
        // scrcpy AudioCodec.getId() 大端字节序
        assert_eq!(
            AudioCodec::from_codec_id(0x6f707573),
            Some(AudioCodec::Opus)
        );
        assert_eq!(AudioCodec::from_codec_id(0x00616163), Some(AudioCodec::Aac));
        assert_eq!(
            AudioCodec::from_codec_id(0x666c6163),
            Some(AudioCodec::Flac)
        );
        assert_eq!(AudioCodec::from_codec_id(0x00726177), Some(AudioCodec::Raw));
        assert_eq!(AudioCodec::from_codec_id(0x12345678), None);
    }

    #[test]
    fn codec_from_name_and_handshake_byte() {
        assert_eq!(AudioCodec::from_name("opus"), AudioCodec::Opus);
        assert_eq!(AudioCodec::from_name("aac"), AudioCodec::Aac);
        assert_eq!(AudioCodec::from_name("flac"), AudioCodec::Flac);
        assert_eq!(AudioCodec::from_name("raw"), AudioCodec::Raw);
        assert_eq!(
            AudioCodec::from_name("bogus"),
            AudioCodec::Raw,
            "无效名回退 raw"
        );
        // 握手索引与 server readRequestedCodec 一致
        assert_eq!(AudioCodec::Raw.handshake_byte(), 0);
        assert_eq!(AudioCodec::Opus.handshake_byte(), 1);
        assert_eq!(AudioCodec::Aac.handshake_byte(), 2);
        assert_eq!(AudioCodec::Flac.handshake_byte(), 3);
    }

    #[test]
    fn codec_name_matches_scrcpy_option() {
        assert_eq!(AudioCodec::Opus.name(), "opus");
        assert_eq!(AudioCodec::Aac.name(), "aac");
        assert_eq!(AudioCodec::Flac.name(), "flac");
        assert_eq!(AudioCodec::Raw.name(), "raw");
    }

    #[test]
    fn decode_raw_converts_s16le() {
        // 两个采样：+1000 与 -1000（S16LE）
        let data = [0xe8, 0x03, 0x18, 0xfc];
        let decoded = decode_raw(&data).unwrap();
        assert_eq!(decoded.samples.len(), 2);
        assert!((decoded.samples[0] - (1000.0 / 32768.0)).abs() < 1e-6);
        assert!((decoded.samples[1] - (-1000.0 / 32768.0)).abs() < 1e-6);
        assert_eq!(decoded.channels.get(), 2);
        assert_eq!(decoded.sample_rate.get(), 48_000);
    }
}
