//! control 流可靠传输（Rust 客户端侧）。
//!
//! Java（phone 侧）用相同算法实现 `ReliableSender/Receiver`，字段逐一对齐。
//!
//! - 发送：单调 `seq`，每个 control 分片独占一个 seq（ACK 粒度 = 分片）；
//!   未确认帧保存在窗口内，超时重传（指数退避），丧失进展判死。
//! - 接收：按 `seq` 缓冲，连续完整后按序投递整条消息，回复累积 ACK。
//!
//! ACK 帧：`Frame{stream=CTRL, type=ACK, payload=ack_seq}`，不带 seq、不重传
//! （累积确认天然抗丢）。

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use prost::Message;

use crate::codec::MAX_FRAGMENT;
use crate::generated::{Frame, frame};

/// control 滑动窗口大小（未确认分片上限）。
pub const WINDOW: usize = 32;
/// 初始重传超时。
pub const INITIAL_RTO: Duration = Duration::from_millis(200);
/// 最大重传超时（指数退避上限）。
pub const MAX_RTO: Duration = Duration::from_millis(1600);
/// 连续无进展判死阈值。
pub const LOSS_LIMIT: u32 = 8;
/// 接收缓冲上限（防恶意/异常积压）。
pub const MAX_RECV_BUFFER: usize = 4096;

/// 发送方向的单条未确认分片。
struct OutEntry {
    seq: u32,
    /// 待重传的完整 Frame（已编码字节，避免重编码）。
    wire: Vec<u8>,
    last_sent: Instant,
    loss_count: u32,
}

/// 可靠发送器（control 流，PC 侧）。
pub struct ReliableSender {
    next_seq: u32,
    window: VecDeque<OutEntry>,
    stalled: bool,
}

impl ReliableSender {
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            window: VecDeque::new(),
            stalled: false,
        }
    }

    /// 把一条 CtrlMsg（已编码 bytes）分片并登记待确认，返回待发出的 Frame 列表。
    ///
    /// 所有分片共享 `msg_id`，各自独占 `seq`。返回的 Frame 应立刻写入 socket。
    pub fn send_ctrl_bytes(&mut self, msg_id: u32, payload: &[u8]) -> Vec<Frame> {
        let total = if payload.is_empty() {
            1
        } else {
            payload.len().div_ceil(MAX_FRAGMENT) as u32
        };
        let mut out = Vec::new();
        for (i, chunk) in payload.chunks(MAX_FRAGMENT).enumerate() {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            let frame = Frame {
                stream: frame::Stream::Ctrl as i32,
                r#type: frame::Type::Data as i32,
                seq,
                msg_id,
                frag: i as u32,
                frag_total: total,
                media_pts: 0,
                media_flags: 0,
                payload: Some(frame::Payload::Data(chunk.to_vec())),
            };
            let mut wire = Vec::with_capacity(frame.encoded_len() + 16);
            prost::Message::encode(&frame, &mut wire).expect("encode ctrl frame");
            self.window.push_back(OutEntry {
                seq,
                wire,
                last_sent: Instant::now(),
                loss_count: 0,
            });
            out.push(frame);
        }
        self.stalled = false;
        out
    }

    /// 处理累积 ACK：释放 ack_seq 之前（含）的所有分片。
    pub fn on_ack(&mut self, ack_seq: u32) {
        let before = self.window.len();
        while let Some(front) = self.window.front() {
            if Self::seq_le(front.seq, ack_seq) {
                self.window.pop_front();
            } else {
                break;
            }
        }
        if self.window.len() < before && !self.window.is_empty() {
            // 有进展：重置判死
            self.stalled = false;
        }
    }

    /// 检查是否有到期需重传的分片，返回其 wire 字节。
    pub fn due_timeouts(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for e in self.window.iter_mut() {
            let rto = Self::rto_for(e.loss_count);
            if now.duration_since(e.last_sent) >= rto {
                e.last_sent = now;
                e.loss_count += 1;
                // 超限判死
                if e.loss_count > LOSS_LIMIT {
                    self.stalled = true;
                    continue;
                }
                out.push(e.wire.clone());
            }
        }
        out
    }

    /// 是否已判死（丧失进展）。
    pub fn is_stalled(&self) -> bool {
        self.stalled
    }

    /// 当前未确认分片数。
    pub fn pending(&self) -> usize {
        self.window.len()
    }

    fn rto_for(loss: u32) -> Duration {
        let mut rto = INITIAL_RTO;
        for _ in 0..loss.min(4) {
            rto *= 2;
        }
        rto.min(MAX_RTO)
    }

    /// `a <= b`（支持 u32 回绕的环形序比较）。
    fn seq_le(a: u32, b: u32) -> bool {
        if a == b {
            return true;
        }
        let diff = a.wrapping_sub(b);
        diff >= 0x8000_0000
    }
}

/// 接收方向的一条分片。
struct RecvFragment {
    msg_id: u32,
    frag: u32,
    total: u32,
    data: Vec<u8>,
}

/// 可靠接收器（control 流，PC 侧）。
pub struct ReliableReceiver {
    next_seq: u32,
    buf: BTreeMap<u32, RecvFragment>,
    dead: bool,
}

impl ReliableReceiver {
    pub fn new() -> Self {
        Self {
            // 从 1 开始（发送方 next_seq=1）
            next_seq: 1,
            buf: BTreeMap::new(),
            dead: false,
        }
    }

    /// 输入一个 control DATA 分片帧，返回完整投递的 CtrlMsg 字节列表
    /// （通常 0 或 1 条；乱序积压时可一次投递多条）。
    pub fn on_frame(&mut self, frame: &Frame) -> Vec<Vec<u8>> {
        if self.dead {
            return Vec::new();
        }
        let data = match &frame.payload {
            Some(frame::Payload::Data(d)) => d.clone(),
            _ => return Vec::new(),
        };
        if frame.seq < self.next_seq {
            return Vec::new(); // 重复
        }
        let total = if frame.frag_total == 0 {
            1
        } else {
            frame.frag_total
        };
        if frame.frag >= total {
            return Vec::new();
        }
        self.buf.insert(
            frame.seq,
            RecvFragment {
                msg_id: frame.msg_id,
                frag: frame.frag,
                total,
                data,
            },
        );

        // 从 next_seq 起做连续投递（单分片与完整多分片消息都可推进）
        let mut delivered: Vec<Vec<u8>> = Vec::new();
        loop {
            // 单分片消息
            if let Some(f) = self.buf.get(&self.next_seq) {
                if f.total == 1 && f.frag == 0 {
                    let f = self.buf.remove(&self.next_seq).unwrap();
                    self.next_seq = self.next_seq.wrapping_add(1);
                    delivered.push(f.data);
                    continue;
                }
            }
            // 多分片消息：frag 0 在 next_seq，检查后续连续分片齐全
            match self.collect_run() {
                Some(run) => {
                    self.next_seq = self.next_seq.wrapping_add(run.frag_count);
                    delivered.push(run.data);
                    continue;
                }
                None => break,
            }
        }
        // 防积压判死
        if self.buf.len() > MAX_RECV_BUFFER {
            self.dead = true;
        }
        delivered
    }

    /// 尝试从 next_seq 取出一次完整连续的分片组（含多分片消息），返回拼接后的字节。
    fn collect_run(&mut self) -> Option<Run> {
        let first = self.buf.get(&self.next_seq)?;
        let total = first.total as u32;
        if first.frag != 0 {
            return None; // 等待消息起点
        }
        let start = self.next_seq;
        let mut pieces = Vec::with_capacity(total as usize);
        let mut bytes = 0usize;
        for i in 0..total {
            let frag = self.buf.get(&(start.wrapping_add(i)))?;
            if frag.msg_id != first.msg_id || frag.frag != i {
                return None; // 不连续/不匹配，等重传
            }
            bytes += frag.data.len();
            pieces.push(frag);
        }
        let mut out = Vec::with_capacity(bytes);
        for p in &pieces {
            out.extend_from_slice(&p.data);
        }
        for i in 0..total {
            self.buf.remove(&start.wrapping_add(i));
        }
        Some(Run {
            data: out,
            frag_count: total,
        })
    }

    /// 累积 ACK 值 = 已确认的最大连续 seq（next_seq - 1）。
    pub fn ack_seq(&self) -> u32 {
        self.next_seq.wrapping_sub(1)
    }

    pub fn is_dead(&self) -> bool {
        self.dead
    }
}

struct Run {
    data: Vec<u8>,
    frag_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_receiver_single_roundtrip() {
        let mut s = ReliableSender::new();
        let mut r = ReliableReceiver::new();
        let payload = b"hello clipboard".to_vec();
        let frames = s.send_ctrl_bytes(1, &payload);
        assert_eq!(frames.len(), 1);
        let mut got = Vec::new();
        for f in &frames {
            got.extend(r.on_frame(f));
        }
        assert_eq!(got, vec![payload]);
        let ack = r.ack_seq();
        s.on_ack(ack);
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn sender_receiver_multifragment_roundtrip() {
        let mut s = ReliableSender::new();
        let mut r = ReliableReceiver::new();
        let payload = vec![7u8; 5000];
        let frames = s.send_ctrl_bytes(3, &payload);
        assert!(frames.len() > 1);
        let mut got = Vec::new();
        for f in &frames {
            got.extend(r.on_frame(f));
        }
        assert_eq!(got, vec![payload]);
    }

    #[test]
    fn receiver_waits_for_retransmit() {
        let mut s = ReliableSender::new();
        let mut r = ReliableReceiver::new();
        let payload = vec![1u8; 5000];
        let frames = s.send_ctrl_bytes(5, &payload);
        // 丢第一个分片：其余到达时不应投递
        let mut got = Vec::new();
        for f in frames.iter().skip(1) {
            got.extend(r.on_frame(f));
        }
        assert!(got.is_empty(), "首个分片缺失不应投递");
        // 重传首片后补齐
        got.extend(r.on_frame(&frames[0]));
        assert_eq!(got, vec![payload]);
    }

    #[test]
    fn ack_releases_window() {
        let mut s = ReliableSender::new();
        let mut r = ReliableReceiver::new();
        let frames = s.send_ctrl_bytes(1, b"msg1");
        let frames2 = s.send_ctrl_bytes(2, b"msg2");
        let mut got = Vec::new();
        for f in frames.iter().chain(frames2.iter()) {
            got.extend(r.on_frame(f));
        }
        assert_eq!(got.len(), 2);
        s.on_ack(r.ack_seq());
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn out_of_order_control_is_buffered_then_delivered() {
        let mut s = ReliableSender::new();
        let mut r = ReliableReceiver::new();
        let f1 = s.send_ctrl_bytes(1, b"first");
        let f2 = s.send_ctrl_bytes(2, b"second");
        // 乱序到达：second 先
        let mut got = Vec::new();
        got.extend(r.on_frame(&f2[0]));
        assert!(got.is_empty(), "乱序不应提前投递");
        got.extend(r.on_frame(&f1[0]));
        assert_eq!(got.len(), 2);
        assert_eq!(&got[0], b"first");
        assert_eq!(&got[1], b"second");
    }
}
