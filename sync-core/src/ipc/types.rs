use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use tokio_util::codec::{Decoder, Encoder};
use uuid::Uuid;

// ── 帧类型常量 ──

pub const FRAME_TYPE_REQUEST: u8 = 0x00;
pub const FRAME_TYPE_RESPONSE: u8 = 0x01;
pub const FRAME_TYPE_EVENT: u8 = 0x02;
pub const FRAME_TYPE_AUDIO: u8 = 0x03; // 预留

/// 单帧最大字节数（payload 部分），防止异常/恶意客户端声明超大长度导致内存膨胀。
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

// ── FrameCodec ──

/// 长度前缀 + 类型字节的帧编解码器。
///
/// 线格式：`[length:u32 LE][type:u8][payload:N]`
/// `Decoder::Item` / `Encoder::Item` = (帧类型, 载荷字节)。
#[derive(Default, Clone, Copy)]
pub struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = (u8, Vec<u8>);
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([src[0], src[1], src[2], src[3]]) as usize;
        if len > MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("frame too large: {len} > {MAX_FRAME_SIZE}"),
            ));
        }
        let total = 5 + len;
        if src.len() < total {
            src.reserve(total - src.len());
            return Ok(None);
        }
        let frame_type = src[4];
        let payload = src[5..total].to_vec();
        src.advance(total);
        Ok(Some((frame_type, payload)))
    }
}

impl Encoder<(u8, Vec<u8>)> for FrameCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: (u8, Vec<u8>), dst: &mut BytesMut) -> Result<(), Self::Error> {
        let (frame_type, payload) = item;
        dst.put_u32_le(payload.len() as u32);
        dst.put_u8(frame_type);
        dst.extend_from_slice(&payload);
        Ok(())
    }
}

// ── 会话状态（Core 内部 watch，也用于 IPC 编码转换） ──

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// UUID 主键
    pub uuid: Uuid,
    /// 设备规范 ID（来自 mDNS fullname 首段 或 get-serialno）
    pub id: String,
    /// 当前用于 ADB 命令的地址
    pub serial: String,
    /// 设备名称（来自 scrcpy 协议）
    pub name: String,
    /// ADB 连接状态
    pub state: String,
    /// session 生命周期状态: "connecting" | "running" | "failed" | "stopped"
    pub session_state: String,
    /// 剪贴板同步
    pub clipboard_sync: bool,
    /// 通知同步
    pub notification_sync: bool,
    /// 音频是否已启用
    pub audio_enabled: bool,
    /// 音量百分比（0-100）
    pub volume: u16,
    /// 音频播放队列缓冲延迟（ms），0 = 无音频/未播放
    pub audio_buffer_ms: u64,
}

// ── 测试 ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_then_decode_roundtrip() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();

        let item = (FRAME_TYPE_REQUEST, vec![1, 2, 3, 4]);
        codec.encode(item.clone(), &mut buf).unwrap();

        let decoded = codec.decode(&mut buf).unwrap();
        assert_eq!(decoded, Some(item));
        assert!(buf.is_empty(), "all bytes consumed");
    }

    #[test]
    fn decode_returns_none_on_short_buffer() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::from(&b"\x05\x00\x00\x00"[..]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
        assert!(!buf.is_empty());
    }

    #[test]
    fn decode_returns_none_on_partial_payload() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::from(&b"\x0a\x00\x00\x00\x00\x01\x02\x03"[..]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn multiple_frames_in_one_buffer() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();

        let a = (FRAME_TYPE_EVENT, vec![0xde]);
        let b = (FRAME_TYPE_AUDIO, vec![0xca, 0xfe]);
        codec.encode(a.clone(), &mut buf).unwrap();
        codec.encode(b.clone(), &mut buf).unwrap();

        assert_eq!(codec.decode(&mut buf).unwrap(), Some(a));
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(b));
        assert!(buf.is_empty());
    }

    #[test]
    fn empty_payload_roundtrip() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();

        let item = (FRAME_TYPE_RESPONSE, vec![]);
        codec.encode(item.clone(), &mut buf).unwrap();

        let decoded = codec.decode(&mut buf).unwrap();
        assert_eq!(decoded, Some(item));
    }

    #[test]
    fn wire_format_structure() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();

        codec.encode((0x00, b"hello".to_vec()), &mut buf).unwrap();

        assert_eq!(buf[0..4], [5, 0, 0, 0]);
        assert_eq!(buf[4], 0x00);
        assert_eq!(&buf[5..], b"hello");
    }

    #[test]
    fn decode_rejects_oversized_frame() {
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();

        let oversized = (MAX_FRAME_SIZE as u32) + 1;
        buf.extend_from_slice(&oversized.to_le_bytes());
        buf.extend_from_slice(&[FRAME_TYPE_REQUEST]);

        assert!(codec.decode(&mut buf).is_err(), "oversized frame should be rejected");
    }
}
