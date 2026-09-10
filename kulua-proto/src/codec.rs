//! Frame 编解码与媒体分片 / 重装辅助。
//!
//! control 端口：每个 UDP datagram = 恰好一个 `Frame`（protobuf 二进制）。
//! 媒体端口（video=P+1 / audio=P+2）：每个 UDP datagram = 33B 大端定长头 +
//! 负载，字段布局见 `proto/direct.proto` 头注释。协议语义见
//! `docs/direct-udp-protocol.md`。

use crate::generated::CtrlMsg;
/// Wi-Fi 安全 UDP 负载上限。IP/UDP 头 28B，1500 MTU 下 1200 很保守，
/// 含路径 MTU / 802.11 开销余量。媒体头另计（见 [`MEDIA_HDR_LEN`]）。
pub const MAX_FRAGMENT: usize = 1200;

/// 媒体数据报定长头大小（client_id..flags，大端）。
///
/// 布局见 `proto/direct.proto` 头部注释：24..32 是音频代次 `revision`，视频恒 0。
pub const MEDIA_HDR_LEN: usize = 33;
/// `frag_total` 的 OPEN 注册包标记（负载为空；PC → phone 登记媒体端点）。
pub const MEDIA_OPEN_TOTAL: u16 = 0xFFFF;

/// 媒体帧标志位（媒体头 flags 字节，沿用 scrcpy 帧头 bit 语义）。
pub const MEDIA_FLAG_CONFIG: u32 = 1 << 0;
pub const MEDIA_FLAG_KEYFRAME: u32 = 1 << 1;
pub const MEDIA_FLAG_SESSION: u32 = 1 << 2;

/// 媒体流标识（`MediaConfig.stream` 数值；端口派生 video=P+1 / audio=P+2）。
pub const MEDIA_STREAM_AUDIO: u32 = 1;
pub const MEDIA_STREAM_VIDEO: u32 = 2;

/// 媒体端口相对 ctrl 端口（P）的偏移。
pub const VIDEO_PORT_OFFSET: u16 = 1;
/// 音频端口相对 ctrl 端口（P）的偏移。
pub const AUDIO_PORT_OFFSET: u16 = 2;

/// 一个媒体数据报（33B 头解析结果；payload 借用输入缓冲）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFragment<'a> {
    pub client_id: u32,
    pub msg_id: u32,
    pub seq: u32,
    pub frag: u16,
    pub frag_total: u16,
    pub pts: u64,
    /// 音频代次（= 产生本帧的采集器对应的 `SetAudio.revision`；视频恒 0）。
    pub revision: u64,
    pub flags: u8,
    pub payload: &'a [u8],
}

/// 编码一个媒体数据报（33B 大端头 + 负载）追加到 `out`。
pub fn encode_media_datagram(f: &MediaFragment, out: &mut Vec<u8>) {
    out.reserve(MEDIA_HDR_LEN + f.payload.len());
    out.extend_from_slice(&f.client_id.to_be_bytes());
    out.extend_from_slice(&f.msg_id.to_be_bytes());
    out.extend_from_slice(&f.seq.to_be_bytes());
    out.extend_from_slice(&f.frag.to_be_bytes());
    out.extend_from_slice(&f.frag_total.to_be_bytes());
    out.extend_from_slice(&f.pts.to_be_bytes());
    out.extend_from_slice(&f.revision.to_be_bytes());
    out.push(f.flags);
    out.extend_from_slice(f.payload);
}

/// 解析媒体数据报；长度不足 33B 返回 None（payload 允许为空 = OPEN）。
pub fn decode_media_datagram(buf: &[u8]) -> Option<MediaFragment<'_>> {
    if buf.len() < MEDIA_HDR_LEN {
        return None;
    }
    let be32 =
        |o: usize| -> Option<u32> { Some(u32::from_be_bytes(buf.get(o..o + 4)?.try_into().ok()?)) };
    let be16 =
        |o: usize| -> Option<u16> { Some(u16::from_be_bytes(buf.get(o..o + 2)?.try_into().ok()?)) };
    Some(MediaFragment {
        client_id: be32(0)?,
        msg_id: be32(4)?,
        seq: be32(8)?,
        frag: be16(12)?,
        frag_total: be16(14)?,
        pts: u64::from_be_bytes(buf.get(16..24)?.try_into().ok()?),
        revision: u64::from_be_bytes(buf.get(24..32)?.try_into().ok()?),
        flags: *buf.get(32)?,
        payload: &buf[MEDIA_HDR_LEN..],
    })
}
/// 把一段媒体负载切成多个数据报（共享 msg_seq/msg_id；每片都带全量
/// 元数据，接收侧以 frag==0 的 pts/revision/flags 为准）。
#[allow(clippy::too_many_arguments)]
pub fn media_datagrams(
    client_id: u32,
    msg_seq: u32,
    msg_id: u32,
    pts: u64,
    revision: u64,
    flags: u32,
    data: &[u8],
) -> Vec<Vec<u8>> {
    let total = if data.is_empty() {
        1
    } else {
        data.len().div_ceil(MAX_FRAGMENT)
    };
    (0..total)
        .map(|i| {
            let start = i * MAX_FRAGMENT;
            let end = data.len().min(start + MAX_FRAGMENT);
            let mut out = Vec::with_capacity(MEDIA_HDR_LEN + (end - start));
            encode_media_datagram(
                &MediaFragment {
                    client_id,
                    msg_id,
                    seq: msg_seq,
                    frag: i as u16,
                    frag_total: if total == 1 { 0 } else { total as u16 },
                    pts,
                    revision,
                    flags: flags as u8,
                    payload: &data[start..end],
                },
                &mut out,
            );
            out
        })
        .collect()
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
}

#[derive(Debug)]
struct Part {
    seq: u32,
    total: usize,
    frags: Vec<Option<Vec<u8>>>,
    n: usize,
    pts: u64,
    revision: u64,
    flags: u32,
    touched: std::time::Instant,
}

/// 重装完成后交给调用方的媒体消息。
#[derive(Debug, Clone)]
pub struct AssembledMedia {
    pub pts: u64,
    /// 音频代次（视频恒 0）；接收侧据此丢弃上一代的残留帧。
    pub revision: u64,
    pub flags: u32,
    pub data: Vec<u8>,
}

impl Default for MediaReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaReassembler {
    pub fn new() -> Self {
        Self {
            last_seq: 0,
            parts: std::collections::HashMap::new(),
            last_emit: std::time::Instant::now(),
        }
    }

    /// 输入一个媒体数据报分片，返回重装完成的完整消息（可能为 None）。
    pub fn push(&mut self, frag: &MediaFragment) -> Option<AssembledMedia> {
        // OPEN 注册包不携带媒体数据
        if frag.frag_total == MEDIA_OPEN_TOTAL {
            return None;
        }
        let data = frag.payload.to_vec();
        let seq = frag.seq;
        // 过期 / 重复消息
        if seq < self.last_seq
            || (seq == self.last_seq
                && self.last_seq != 0
                && !self.parts.contains_key(&frag.msg_id))
        {
            return None;
        }
        let total = if frag.frag_total == 0 {
            1
        } else {
            frag.frag_total as usize
        };
        if frag.frag as usize >= total {
            return None;
        }

        // 单分片（整包）→ 直接输出
        if total == 1 {
            self.note_emitted(seq);
            self.last_emit = std::time::Instant::now();
            // 清理残留旧 part（避免 msg_id 复用）
            self.parts.remove(&frag.msg_id);
            return Some(AssembledMedia {
                pts: frag.pts,
                revision: frag.revision,
                flags: frag.flags as u32,
                data,
            });
        }

        let part = self.parts.entry(frag.msg_id).or_insert_with(|| Part {
            seq,
            total,
            frags: vec![None; total],
            n: 0,
            pts: frag.pts,
            revision: frag.revision,
            flags: frag.flags as u32,
            touched: std::time::Instant::now(),
        });
        part.touched = std::time::Instant::now();
        // 同一消息的多个 msg_id 不应并存（单生产者顺序流）
        if part.seq != seq {
            // 消息序号跳变：旧消息分片已不可挽救，丢弃重建
            if seq > part.seq {
                self.parts.remove(&frag.msg_id);
                return self.push(frag);
            }
            return None;
        }
        if part.frags[frag.frag as usize].is_none() {
            part.frags[frag.frag as usize] = Some(data);
            part.n += 1;
        }
        if part.n == part.total {
            let done = self.parts.remove(&frag.msg_id).unwrap();
            self.note_emitted(done.seq);
            self.last_emit = std::time::Instant::now();
            let mut out = Vec::with_capacity(done.total * MAX_FRAGMENT);
            for f in done.frags.into_iter().flatten() {
                out.extend_from_slice(&f);
            }
            Some(AssembledMedia {
                pts: done.pts,
                revision: done.revision,
                flags: done.flags,
                data: out,
            })
        } else {
            None
        }
    }

    /// 清理超时残留分片（重装不完整且太久无进展）。
    pub fn sweep(&mut self, timeout: std::time::Duration) {
        let now = std::time::Instant::now();
        self.parts
            .retain(|_, p| now.duration_since(p.touched) < timeout);
    }

    /// 记录一次成功输出消息的序号（乱序/重复判定用）。
    fn note_emitted(&mut self, seq: u32) {
        self.last_seq = seq;
    }
}

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

    /// 编码缓冲 → 解析结果（借用与缓冲成对存活）。
    fn decode_all(bufs: &[Vec<u8>]) -> Vec<MediaFragment<'_>> {
        bufs.iter()
            .map(|b| decode_media_datagram(b).unwrap())
            .collect()
    }

    #[test]
    fn media_datagrams_roundtrip() {
        let data: Vec<u8> = (0..2500).map(|i| (i % 251) as u8).collect();
        let bufs = media_datagrams(7, 42, 11, 123, 9, MEDIA_FLAG_KEYFRAME, &data);
        let frags = decode_all(&bufs);
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
        assert_eq!(m.revision, 9, "分片头必须携带音频代次");
        assert_eq!(m.flags, MEDIA_FLAG_KEYFRAME);
        assert!(frags.iter().all(|f| f.client_id == 7 && f.seq == 42));
    }

    #[test]
    fn open_datagram_is_rejected_by_reassembler() {
        let mut wire = Vec::new();
        encode_media_datagram(
            &MediaFragment {
                client_id: 3,
                msg_id: 0,
                seq: 0,
                frag: 0,
                frag_total: MEDIA_OPEN_TOTAL,
                pts: 0,
                revision: 0,
                flags: 0,
                payload: &[],
            },
            &mut wire,
        );
        assert_eq!(wire.len(), MEDIA_HDR_LEN);
        let f = decode_media_datagram(&wire).unwrap();
        assert_eq!(f.frag_total, MEDIA_OPEN_TOTAL);
        let mut r = MediaReassembler::new();
        assert!(r.push(&f).is_none());
    }

    #[test]
    fn media_dedup_stale_seq() {
        let bufs = media_datagrams(1, 5, 0, 0, 0, 0, &[1u8, 2, 3]);
        let frags = decode_all(&bufs);
        assert_eq!(frags.len(), 1);
        let mut r = MediaReassembler::new();
        assert!(r.push(&frags[0]).is_some());
        // 旧序号重复
        assert!(r.push(&frags[0]).is_none());
    }

    #[test]
    fn fragmented_interleaved_message_ordering() {
        // 消息 A(10) 与 B(11) 分片交错到达，各自 msg_id 不同
        let da: Vec<u8> = vec![9; 2500];
        let db: Vec<u8> = vec![8; 2500];
        let bufs_a = media_datagrams(1, 10, 1, 0, 0, 0, &da);
        let bufs_b = media_datagrams(1, 11, 2, 0, 0, 0, &db);
        let fa = decode_all(&bufs_a);
        let fb = decode_all(&bufs_b);
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
        let bufs = media_datagrams(1, 1, 5, 0, 0, 0, &data);
        let frags = decode_all(&bufs);
        assert!(frags.len() >= 2);
        let mut r = MediaReassembler::new();
        r.push(&frags[0]);
        r.push(&frags[2]);
        // 等超过超时时间后再 sweep，才能清掉残留分片
        std::thread::sleep(std::time::Duration::from_millis(10));
        r.sweep(std::time::Duration::from_millis(1));
        assert!(r.parts.is_empty());
    }

    /// 33B 媒体头的字节布局是 Java（`kulua-server/MediaDatagram.java`）与 Rust
    /// 之间的契约。这里的黄金样本由 Java 编码器实际产出（javac + java 跑
    /// `encode(0x01020304, 0x11121314, 0x05060708, 1, 2, 0x0A0B0C0D0E0F1011,
    /// 0x0102030405060708, 3, "ABCD")`），字段位置/宽度/字节序被改动即在两端
    /// 失配——这个测试会先失败。
    #[test]
    fn media_header_layout_matches_java_encoder() {
        let hex = "010203041112131405060708000100020a0b0c0d0e0f101101020304050607080341424344";
        let wire: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        assert_eq!(
            wire.len(),
            MEDIA_HDR_LEN + 4,
            "头长必须与 Java HEADER_SIZE 一致"
        );
        let f = decode_media_datagram(&wire).expect("Java 产出的字节必须能被 Rust 解析");
        assert_eq!(f.client_id, 0x0102_0304);
        assert_eq!(f.msg_id, 0x1112_1314);
        assert_eq!(f.seq, 0x0506_0708);
        assert_eq!(f.frag, 1);
        assert_eq!(f.frag_total, 2);
        assert_eq!(f.pts, 0x0A0B_0C0D_0E0F_1011);
        assert_eq!(f.revision, 0x0102_0304_0506_0708);
        assert_eq!(f.flags, 3);
        assert_eq!(f.payload, b"ABCD");
    }

    #[test]
    fn short_datagram_rejected() {
        assert!(decode_media_datagram(&[0u8; 32]).is_none());
        assert!(decode_media_datagram(&[0u8; 33]).is_some());
    }
}
