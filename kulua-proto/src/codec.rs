//! Frame 编解码与媒体分片 / 重装辅助。
//!
//! 协议语义见 `proto/direct.proto` 与 `docs/direct-udp-protocol.md`。
//! 每个 UDP datagram = 恰好一个 `Frame`（protobuf 二进制）。

use crate::generated::{CtrlMsg, Frame, ctrl_msg, frame};

/// Wi-Fi 安全 UDP payload 上限。IP/UDP 头 28B，1500 MTU 下 1200 很保守，
/// 含路径 MTU / 802.11 开销余量。
pub const MAX_FRAGMENT: usize = 1200;

/// 媒体帧标志位（`Frame.media_flags`，沿用 scrcpy 帧头 bit 语义）。
pub const MEDIA_FLAG_CONFIG: u32 = 1 << 0;
pub const MEDIA_FLAG_KEYFRAME: u32 = 1 << 1;
pub const MEDIA_FLAG_SESSION: u32 = 1 << 2;

/// 把一段媒体负载切成多个 `Frame` 分片（每条消息共享 msg_id / seq）。
///
/// `seq` 为该消息序号（分片间相同，媒体流用作排序去重而非 ACK）。
/// 每条分片含完整 media 元数据（首片为准，接收侧以首片记录）。
pub fn media_fragments(
    stream: frame::Stream,
    msg_seq: u32,
    msg_id: u32,
    media_pts: u64,
    media_flags: u32,
    data: &[u8],
) -> Vec<Frame> {
    if data.is_empty() {
        return vec![media_frame(
            stream,
            msg_seq,
            msg_id,
            0,
            0,
            media_pts,
            media_flags,
            &[],
        )];
    }
    let total = data.len().div_ceil(MAX_FRAGMENT) as u32;
    data.chunks(MAX_FRAGMENT)
        .enumerate()
        .map(|(i, chunk)| {
            media_frame(
                stream,
                msg_seq,
                msg_id,
                i as u32,
                total,
                media_pts,
                media_flags,
                chunk,
            )
        })
        .collect()
}

/// 构造单条媒体 DATA 分片帧。
pub fn media_frame(
    stream: frame::Stream,
    msg_seq: u32,
    msg_id: u32,
    frag: u32,
    frag_total: u32,
    media_pts: u64,
    media_flags: u32,
    data: &[u8],
) -> Frame {
    Frame {
        stream: stream as i32,
        r#type: frame::Type::Data as i32,
        seq: msg_seq,
        msg_id,
        frag,
        frag_total,
        media_pts,
        media_flags,
        payload: Some(frame::Payload::Data(data.to_vec())),
    }
}

/// media config 数据（H.264 SPS/PPS / AAC AudioSpecificConfig / FLAC STREAMINFO）。
/// 走可靠 control 流投递，避免丢失导致解码器无法初始化。
pub fn ctrl_media_config(stream: frame::Stream, data: &[u8]) -> CtrlMsg {
    CtrlMsg {
        msg: Some(ctrl_msg::Msg::MediaConfig(crate::generated::MediaConfig {
            stream: stream as u32,
            data: data.to_vec(),
        })),
    }
}

/// 媒体接收端重装器（音频 / 视频各一个实例）。
///
/// 假定局域网内单生产者顺序发送：按 `seq` 排序去重（乱序/重复丢弃），
/// 分片按 msg_id 缓冲到连续完整后输出。丢失分片 → 超时整条丢弃。
#[derive(Debug)]
pub struct MediaReassembler {
    /// 已完成（已输出）的最大消息序号。
    last_seq: u32,
    /// 正在重装的 <msg_id, 部分数据>。
    parts: std::collections::HashMap<u32, Part>,
    /// 最近一次投递时间（超时清残留用）。
    last_emit: std::time::Instant,
    /// 已判定丢失的媒体消息数（序号跳变 / 分片超时被清扫），供 HUD 丢包率。
    lost: u64,
}

#[derive(Debug)]
struct Part {
    seq: u32,
    total: usize,
    frags: Vec<Option<Vec<u8>>>,
    n: usize,
    pts: u64,
    flags: u32,
    touched: std::time::Instant,
}

/// 重装完成后交给调用方的媒体消息。
#[derive(Debug, Clone)]
pub struct AssembledMedia {
    pub pts: u64,
    pub flags: u32,
    pub data: Vec<u8>,
}

impl MediaReassembler {
    pub fn new() -> Self {
        Self {
            last_seq: 0,
            parts: std::collections::HashMap::new(),
            last_emit: std::time::Instant::now(),
            lost: 0,
        }
    }

    /// 输入一个媒体 DATA 分片帧，返回重装完成的完整消息（可能为 None）。
    pub fn push(&mut self, frame: &Frame) -> Option<AssembledMedia> {
        let data = match &frame.payload {
            Some(frame::Payload::Data(d)) => d.clone(),
            _ => return None,
        };
        let seq = frame.seq;
        // 过期 / 重复消息
        if seq < self.last_seq
            || (seq == self.last_seq
                && self.last_seq != 0
                && !self.parts.contains_key(&frame.msg_id))
        {
            return None;
        }
        let total = if frame.frag_total == 0 {
            1
        } else {
            frame.frag_total as usize
        };
        if frame.frag as usize >= total {
            return None;
        }

        // 单分片（整包）→ 直接输出
        if total == 1 {
            self.note_emitted(seq);
            self.last_emit = std::time::Instant::now();
            // 清理残留旧 part（避免 msg_id 复用）
            self.parts.remove(&frame.msg_id);
            return Some(AssembledMedia {
                pts: frame.media_pts,
                flags: frame.media_flags,
                data,
            });
        }

        let part = self.parts.entry(frame.msg_id).or_insert_with(|| Part {
            seq,
            total,
            frags: vec![None; total],
            n: 0,
            pts: frame.media_pts,
            flags: frame.media_flags,
            touched: std::time::Instant::now(),
        });
        part.touched = std::time::Instant::now();
        // 同一消息的多个 msg_id 不应并存（单生产者顺序流）
        if part.seq != seq {
            // 消息序号跳变：旧消息分片已不可挽救，丢弃重建
            if seq > part.seq {
                self.parts.remove(&frame.msg_id);
                return self.push(frame);
            }
            return None;
        }
        if part.frags[frame.frag as usize].is_none() {
            part.frags[frame.frag as usize] = Some(data);
            part.n += 1;
        }
        if part.n == part.total {
            let done = self.parts.remove(&frame.msg_id).unwrap();
            self.note_emitted(done.seq);
            self.last_emit = std::time::Instant::now();
            let mut out = Vec::with_capacity(done.total * MAX_FRAGMENT);
            for f in done.frags.into_iter().flatten() {
                out.extend_from_slice(&f);
            }
            Some(AssembledMedia {
                pts: done.pts,
                flags: done.flags,
                data: out,
            })
        } else {
            None
        }
    }

    /// 清理超时残留分片（重装不完整且太久无进展）；每条被清的消息计一次丢帧。
    pub fn sweep(&mut self, timeout: std::time::Duration) {
        let now = std::time::Instant::now();
        let before = self.parts.len();
        self.parts
            .retain(|_, p| now.duration_since(p.touched) < timeout);
        self.lost += (before - self.parts.len()) as u64;
    }

    /// 记录一次成功输出消息的序号，并估算其间跳过（丢失）的消息数。
    fn note_emitted(&mut self, seq: u32) {
        if self.last_seq != 0 && seq > self.last_seq {
            let gap = (seq - self.last_seq - 1) as u64;
            // 上限 1024：防止异常流（序号错乱）把计数刷爆
            self.lost += gap.min(1024);
        }
        self.last_seq = seq;
    }

    /// 已判定丢失的消息数（丢包率 = lost / (lost + 成功输出数)）。
    pub fn lost(&self) -> u64 {
        self.lost
    }
}

// 便于 session 内部把指令消息编码为 control DATA 帧（配合 reliable.rs 使用）。
/// 把一条 CtrlMsg 编码为字节（作为可靠 DATA 的负载）。
pub fn encode_ctrl(msg: &CtrlMsg) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    prost::Message::encode(msg, &mut buf).expect("encode ctrl");
    buf
}

/// 解码一条 CtrlMsg。
pub fn decode_ctrl(bytes: &[u8]) -> Option<CtrlMsg> {
    prost::Message::decode(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_fragments_roundtrip() {
        let data: Vec<u8> = (0..2500).map(|i| (i % 251) as u8).collect();
        let frags = media_fragments(frame::Stream::Video, 7, 11, 123, MEDIA_FLAG_KEYFRAME, &data);
        assert!(frags.len() > 1);
        let mut reassembler = MediaReassembler::new();
        let mut got = None;
        for f in &frags {
            if let Some(m) = reassembler.push(f) {
                got = Some(m);
            }
        }
        let m = got.expect("应重装完成");
        assert_eq!(m.data, data);
        assert_eq!(m.pts, 123);
        assert_eq!(m.flags, MEDIA_FLAG_KEYFRAME);
    }

    #[test]
    fn media_dedup_stale_seq() {
        let data = vec![1u8, 2, 3];
        let f = media_frame(frame::Stream::Audio, 5, 0, 0, 0, 0, 0, &data);
        let mut r = MediaReassembler::new();
        assert!(r.push(&f).is_some());
        // 旧序号重复
        assert!(r.push(&f).is_none());
    }

    #[test]
    fn fragmented_interleaved_message_ordering() {
        // 消息 A(10) 与 B(11) 分片交错到达，各自 msg_id 不同
        let da: Vec<u8> = vec![9; 2500];
        let db: Vec<u8> = vec![8; 2500];
        let fa = media_fragments(frame::Stream::Video, 10, 1, 0, 0, &da);
        let fb = media_fragments(frame::Stream::Video, 11, 2, 0, 0, &db);
        let mut r = MediaReassembler::new();
        let mut got = Vec::new();
        // A0, B0, A1, B1, A2, B2（交错）
        assert_eq!(fa.len(), 3);
        assert_eq!(fb.len(), 3);
        got.extend(r.push(&fa[0]).iter().cloned());
        got.extend(r.push(&fb[0]).iter().cloned());
        got.extend(r.push(&fa[1]).iter().cloned());
        got.extend(r.push(&fb[1]).iter().cloned());
        got.extend(r.push(&fa[2]).iter().cloned());
        got.extend(r.push(&fb[2]).iter().cloned());
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].data, da);
        assert_eq!(got[1].data, db);
    }

    #[test]
    fn lost_fragment_cleaned_by_sweep() {
        let data: Vec<u8> = vec![1; 2500];
        let frags = media_fragments(frame::Stream::Video, 1, 5, 0, 0, &data);
        assert!(frags.len() >= 2);
        let mut r = MediaReassembler::new();
        r.push(&frags[0]);
        r.push(&frags[2]);
        // 等超过超时时间后再 sweep，才能清掉残留分片
        std::thread::sleep(std::time::Duration::from_millis(10));
        r.sweep(std::time::Duration::from_millis(1));
        assert!(r.parts.is_empty());
    }
}
