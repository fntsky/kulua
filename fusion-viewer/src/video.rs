//! 视频解码线程：接收上层转发的视频帧/配置 → FFmpeg 解码 → 送渲染事件。
//!
//! 数据源是 UDP 会话的事件流（`VideoInput`）：config 帧（H.264 SPS/PPS）经可靠
//! control 流（MediaConfig）到达，媒体帧为已重装完整的 `AssembledMedia`。
//! 本线程只做解码，不直接碰 socket。

use std::sync::mpsc::{Receiver, SyncSender};

use winit::event_loop::EventLoopProxy;

use crate::decoder::{DecodedFrame, VideoDecoder};
use kulua_proto::codec::AssembledMedia;

/// 单帧最大字节数（安全上限，防异常流撑爆内存）。
const MAX_FRAME_SIZE: usize = 50 * 1024 * 1024;

/// 视频线程 → 主线程事件。
pub enum VideoEvent {
    /// 解码出的一帧（YUV420P）
    Frame(DecodedFrame),
    /// 流错误（解码失败 / 连接断开）
    Error(String),
    /// 流正常结束
    Closed,
}

/// 解码线程的输入：配置帧 / 媒体帧。
pub enum VideoInput {
    /// codec config（H.264 SPS/PPS，可靠投递；首次设为 extradata，其后作为
    /// 新分辨率来源重置解码器）。
    Config(Vec<u8>),
    /// 一帧编码数据（已重装）。
    Frame(AssembledMedia),
}

/// 启动视频解码线程。每产生一帧都会通过 proxy 发送 user event 唤醒事件循环。
pub fn spawn_video_thread(
    rx: Receiver<VideoInput>,
    codec_id: u32,
    tx: SyncSender<VideoEvent>,
    proxy: EventLoopProxy<()>,
) {
    std::thread::spawn(move || {
        let mut decoder = match VideoDecoder::new(codec_id) {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.try_send(VideoEvent::Error(e));
                return;
            }
        };

        while let Ok(input) = rx.recv() {
            match input {
                VideoInput::Config(payload) => {
                    // 与旧实现在同一语义：非 Annex-B（通常 avcC 长度前缀）转
                    // Annex-B，防止与媒体帧模式冲突；vivo 等设备 IDR 不自带
                    // SPS/PPS，config 是唯一参数来源，不能丢弃。
                    let annexb = if crate::decoder::is_annexb(&payload) {
                        payload
                    } else {
                        match crate::decoder::avcc_to_annexb(&payload) {
                            Some(conv) => conv,
                            None => {
                                let _ = tx.try_send(VideoEvent::Error(
                                    "config 帧既不是 Annex-B 也无法解析为 avcC".into(),
                                ));
                                return;
                            }
                        }
                    };
                    if !decoder.is_open() {
                        if let Err(e) = decoder.set_extradata(&annexb) {
                            let _ =
                                tx.try_send(VideoEvent::Error(format!("设置 extradata 失败: {e}")));
                            return;
                        }
                        if let Err(e) = decoder.open() {
                            let _ = tx.try_send(VideoEvent::Error(format!("打开解码器失败: {e}")));
                            return;
                        }
                    } else {
                        // 已打开：resize 后新编码器发出新分辨率 SPS/PPS，必须重置
                        if let Err(e) = decoder.reset_with_extradata(&annexb) {
                            let _ = tx.try_send(VideoEvent::Error(format!("重置解码器失败: {e}")));
                            return;
                        }
                        println!("[viewer] 解码器已按新分辨率重置");
                    }
                }
                VideoInput::Frame(frame) => {
                    let len = frame.data.len();
                    if len == 0 || len > MAX_FRAME_SIZE {
                        continue;
                    }
                    // 防御：帧内携带 config 标志（正常走 MediaConfig，不应出现）
                    if frame.flags & 1 != 0 {
                        rx_tx_config(&mut decoder, frame.data.clone(), &tx);
                        continue;
                    }
                    if !decoder.is_open() {
                        if let Err(e) = decoder.open() {
                            let _ = tx.try_send(VideoEvent::Error(format!("打开解码器失败: {e}")));
                            return;
                        }
                    }
                    let keyframe = (frame.flags >> 1) & 1 != 0;
                    let pts = frame.pts as i64;
                    match decoder.decode(&frame.data, Some(pts), keyframe) {
                        Ok(Some(frame)) => {
                            let _ = tx.try_send(VideoEvent::Frame(frame));
                            let _ = proxy.send_event(());
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let _ = tx.try_send(VideoEvent::Error(format!("解码失败: {e}")));
                            return;
                        }
                    }
                }
            }
        }
        let _ = tx.try_send(VideoEvent::Closed);
    });
}

/// 罕见的内联 config 帧处理（与 VideoInput::Config 同逻辑）。
fn rx_tx_config(decoder: &mut VideoDecoder, data: Vec<u8>, tx: &SyncSender<VideoEvent>) {
    if let Err(e) = decoder.set_extradata(&data) {
        let _ = tx.try_send(VideoEvent::Error(format!("设置 extradata 失败: {e}")));
        return;
    }
    if !decoder.is_open() {
        if let Err(e) = decoder.open() {
            let _ = tx.try_send(VideoEvent::Error(format!("打开解码器失败: {e}")));
        }
    } else if let Err(e) = decoder.reset_with_extradata(&data) {
        let _ = tx.try_send(VideoEvent::Error(format!("重置解码器失败: {e}")));
    }
}
