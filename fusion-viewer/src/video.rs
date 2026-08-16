//! 视频流读取线程：读 scrcpy 视频 socket → 解码 → 送渲染事件。
//!
//! 流格式（scrcpy v4.0 源码核实）：
//! - 先读 4 字节 codec id（大端）
//! - 然后循环读 12 字节帧头：
//!   - bit63（SESSION 标志）：session 帧，后 8 字节为 width(u32be) + height(u32be)，
//!     `header[3] & 1` = client_resized；无 payload
//!   - 否则：8 字节 pts/flags（bit62=config，bit61=keyframe，pts = 低 61 位）
//!     + 4 字节 payload 长度，随后是 payload

use std::io::Read;
use std::net::TcpStream;
use std::sync::mpsc::Sender;

use winit::event_loop::EventLoopProxy;

use crate::decoder::{DecodedFrame, VideoDecoder};

/// 单帧最大字节数（安全上限，防异常流撑爆内存）。
const MAX_FRAME_SIZE: usize = 50 * 1024 * 1024;

/// 视频线程 → 主线程事件。
pub enum VideoEvent {
    /// 解码出的一帧（YUV420P）
    Frame(DecodedFrame),
    /// session 元数据（分辨率变化）
    Session {
        width: u32,
        height: u32,
        client_resized: bool,
    },
    /// 流错误（解码失败 / 连接断开）
    Error(String),
    /// 流正常结束
    Closed,
}

/// 启动视频读取线程。每产生一帧都会通过 proxy 发送 user event 唤醒事件循环。
pub fn spawn_video_thread(video: TcpStream, tx: Sender<VideoEvent>, proxy: EventLoopProxy<()>) {
    std::thread::spawn(move || {
        let mut video = video;
        let _ = video.set_read_timeout(None);

        // 1. 设备名（64 字节，send_device_meta 写入 video socket；dummy byte 已由部署阶段读取）
        let mut name_buf = [0u8; 64];
        if read_exact(&mut video, &mut name_buf).is_err() {
            let _ = tx.send(VideoEvent::Error("读取设备名失败".into()));
            return;
        }
        let name_end = name_buf.iter().position(|&b| b == 0).unwrap_or(64);
        let device_name = String::from_utf8_lossy(&name_buf[..name_end]);
        println!("[viewer] 设备名: {}", device_name);

        // 2. codec id（4 字节大端；v4.0 为 ASCII 名称如 "h264"/"h265"/"av1"）
        let mut codec_buf = [0u8; 4];
        if read_exact(&mut video, &mut codec_buf).is_err() {
            let _ = tx.send(VideoEvent::Error("读取 codec id 失败".into()));
            return;
        }
        let codec_id = u32::from_be_bytes(codec_buf);
        println!(
            "[viewer] codec id: {:#010x} ({})",
            codec_id,
            String::from_utf8_lossy(&codec_buf)
        );
        let mut decoder = match VideoDecoder::new(codec_id) {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.send(VideoEvent::Error(e));
                return;
            }
        };

        // 3. 帧循环
        loop {
            let mut header = [0u8; 12];
            if read_exact(&mut video, &mut header).is_err() {
                let _ = tx.send(VideoEvent::Closed);
                return;
            }

            // session 帧（bit63 = 1）
            if header[0] & 0x80 != 0 {
                let width = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
                let height = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
                let client_resized = header[3] & 1 != 0;
                if tx
                    .send(VideoEvent::Session {
                        width,
                        height,
                        client_resized,
                    })
                    .is_err()
                {
                    return;
                }
                let _ = proxy.send_event(());
                continue;
            }

            let pts_flags = u64::from_be_bytes(header[..8].try_into().unwrap());
            let len = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
            if len == 0 || len > MAX_FRAME_SIZE {
                let _ = tx.send(VideoEvent::Error(format!("非法帧长度: {}", len)));
                return;
            }
            let mut payload = vec![0u8; len];
            if read_exact(&mut video, &mut payload).is_err() {
                let _ = tx.send(VideoEvent::Closed);
                return;
            }

            let config = pts_flags & (1 << 62) != 0;
            let keyframe = pts_flags & (1 << 61) != 0;
            let pts = (pts_flags & ((1 << 61) - 1)) as i64;

            if config {
                // config 帧（MediaCodec csd）：转换为 Annex-B 后设为 extradata。
                // 不能直接使用原始 csd（通常是 avcC 长度前缀格式，会迫使解码器
                // 进入 length-prefixed 模式，与 Annex-B 媒体帧冲突导致错位）；
                // 也不能丢弃——部分设备（如 vivo）的 IDR 帧不自带 SPS/PPS，
                // 丢弃会 'non-existing PPS' 无法解码。
                if !decoder.is_open() {
                    let annexb = if crate::decoder::is_annexb(&payload) {
                        payload
                    } else {
                        match crate::decoder::avcc_to_annexb(&payload) {
                            Some(conv) => conv,
                            None => {
                                let _ = tx.send(VideoEvent::Error(
                                    "config 帧既不是 Annex-B 也无法解析为 avcC".into(),
                                ));
                                return;
                            }
                        }
                    };
                    if let Err(e) = decoder.set_extradata(&annexb) {
                        let _ = tx.send(VideoEvent::Error(format!("设置 extradata 失败: {}", e)));
                        return;
                    }
                    if let Err(e) = decoder.open() {
                        let _ = tx.send(VideoEvent::Error(format!("打开解码器失败: {}", e)));
                        return;
                    }
                }
                continue;
            }

            // 确保解码器已打开（无 config 帧的异常流也能解码）
            if !decoder.is_open() {
                if let Err(e) = decoder.open() {
                    let _ = tx.send(VideoEvent::Error(format!("打开解码器失败: {}", e)));
                    return;
                }
            }

            match decoder.decode(&payload, Some(pts), keyframe) {
                Ok(Some(frame)) => {
                    if tx.send(VideoEvent::Frame(frame)).is_err() {
                        return;
                    }
                    let _ = proxy.send_event(());
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = tx.send(VideoEvent::Error(format!("解码失败: {}", e)));
                    return;
                }
            }
        }
    });
}

fn read_exact(stream: &mut TcpStream, buf: &mut [u8]) -> std::io::Result<()> {
    stream.read_exact(buf)
}
