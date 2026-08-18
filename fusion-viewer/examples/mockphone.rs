//! 本地 mock「手机」：在无真机环境下复现/验证 viewer 的 UDP 会话、事件循环与渲染。
//!
//! 行为对齐真实 kulua-server 的握手 + 媒体：
//! - HELLO → HELLO_ACK
//! - CreateDisplay（control DATA）→ ACK + 回 DisplayReady，随后按文件推真实 H.264
//! - 对每条 control DATA 回累积 ACK（否则 viewer 的可靠发送窗口积压 → 判死关闭）
//!
//! 用法：`mockphone <port> [h264_file]`
//! 默认读 `test_160x120.h264`（与 exe 同目录）；不带文件则只握手不发视频。
//! 然后：`fusion-viewer.exe --connect 127.0.0.1:<port> --package com.mock --debug`

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use kulua_proto::generated::{CtrlMsg, DisplayReady, Frame, HelloAck, ctrl_msg, frame};
use kulua_proto::prost::Message;

fn wire(f: &Frame) -> Vec<u8> {
    let mut w = Vec::with_capacity(64);
    Message::encode(f, &mut w).unwrap();
    w
}

fn send(sock: &UdpSocket, src: std::net::SocketAddr, f: &Frame) {
    let w = wire(f);
    let _ = sock.send_to(&w, src);
}

/// 解析 Annex-B H.264：返回 (config[SPS+PPS], 逐帧 AU 列表)。每个 NAL 保留起始码。
fn parse_h264(bytes: &[u8]) -> (Option<Vec<u8>>, Vec<Vec<u8>>) {
    // 找所有 NAL 边界（起始码 00 00 01 / 00 00 00 01）
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i + 3 < bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 {
            let four = i + 3 < bytes.len() && bytes[i + 2] == 0 && bytes[i + 3] == 1;
            let three = !four && bytes[i + 2] == 1;
            if four {
                starts.push(i);
                i += 4;
            } else if three {
                starts.push(i);
                i += 3;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    if starts.is_empty() {
        return (None, Vec::new());
    }
    let nals: Vec<&[u8]> = starts
        .iter()
        .enumerate()
        .map(|(idx, &s)| {
            let e = starts.get(idx + 1).copied().unwrap_or(bytes.len());
            &bytes[s..e]
        })
        .collect();

    let mut config: Option<Vec<u8>> = None;
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();

    for nal in nals {
        // nal 形如 [00 00 01] hdr data...；头部字节取其 3/4 字节起始码后的第一个字节
        let mut hdr = 0u8;
        for &b in nal.iter().skip(3).take(1) {
            hdr = b;
        }
        if hdr == 0 {
            continue;
        }
        let ntype = hdr & 0x1f;
        match ntype {
            7 | 8 => {
                // SPS/PPS：并入 config；同时以其为新 AU（关键帧）边界
                config = config.or_else(|| Some(Vec::new()));
                if let Some(c) = config.as_mut() {
                    c.extend_from_slice(nal);
                }
                if !cur.is_empty() {
                    frames.push(std::mem::take(&mut cur));
                }
            }
            _ => {
                cur.extend_from_slice(nal);
            }
        }
    }
    if !cur.is_empty() {
        frames.push(cur);
    }
    (config, frames)
}

fn main() {
    let port: u16 = std::env::args().nth(1).expect("port").parse().unwrap();
    let file = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "test_160x120.h264".to_string());
    let clip = std::fs::read(&file).ok();
    let (video_config, video_frames) = match &clip {
        Some(b) => parse_h264(b),
        None => (None, Vec::new()),
    };
    println!(
        "[mockphone] listening 127.0.0.1:{port}, clip={} config={} frames={}",
        file,
        video_config.is_some(),
        video_frames.len()
    );

    let sock = UdpSocket::bind(("127.0.0.1", port)).unwrap();
    let mut peer: Option<std::net::SocketAddr> = None;
    let mut started = false;
    let mut frame_idx: usize = 0;
    let mut last_video = Instant::now();
    let mut vseq: u32 = 1;
    let mut buf = vec![0u8; 65536];
    sock.set_read_timeout(Some(Duration::from_millis(10))).ok();

    loop {
        if started && !video_frames.is_empty() && last_video.elapsed() >= Duration::from_millis(33)
        {
            last_video = Instant::now();
            let Some(peer) = peer else { continue };
            let data = &video_frames[frame_idx % video_frames.len()];
            let frags = kulua_proto::codec::media_fragments(
                frame::Stream::Video,
                vseq,
                vseq,
                last_video.elapsed().as_micros() as u64,
                0,
                data,
            );
            for f in frags {
                send(&sock, peer, &f);
            }
            vseq += 1;
            frame_idx += 1;
        }

        let Ok((n, src)) = sock.recv_from(&mut buf) else {
            continue;
        };
        peer = Some(src);
        let Ok(f) = Frame::decode(&buf[..n]) else {
            continue;
        };
        match frame::Type::try_from(f.r#type) {
            Ok(frame::Type::Hello) => {
                let scid = match f.payload {
                    Some(frame::Payload::Hello(h)) => h.scid.clone(),
                    _ => "mock".into(),
                };
                println!("[mockphone] HELLO scid={scid}");
                send(
                    &sock,
                    src,
                    &Frame {
                        stream: frame::Stream::Ctrl as i32,
                        r#type: frame::Type::HelloAck as i32,
                        seq: 0,
                        msg_id: 0,
                        frag: 0,
                        frag_total: 0,
                        media_pts: 0,
                        media_flags: 0,
                        payload: Some(frame::Payload::HelloAck(HelloAck {
                            scid,
                            audio_codec: 0,
                            video_codec: 0x68323634,
                        })),
                    },
                );
            }
            Ok(frame::Type::Data)
                if frame::Stream::try_from(f.stream) == Ok(frame::Stream::Ctrl) =>
            {
                send(
                    &sock,
                    src,
                    &Frame {
                        stream: frame::Stream::Ctrl as i32,
                        r#type: frame::Type::Ack as i32,
                        seq: 0,
                        msg_id: 0,
                        frag: 0,
                        frag_total: 0,
                        media_pts: 0,
                        media_flags: 0,
                        payload: Some(frame::Payload::AckSeq(f.seq)),
                    },
                );

                if let Some(frame::Payload::Data(d)) = &f.payload {
                    if let Ok(ctrl) = CtrlMsg::decode(d.as_slice()) {
                        match ctrl.msg {
                            Some(ctrl_msg::Msg::CreateDisplay(cd)) => {
                                println!(
                                    "[mockphone] CreateDisplay {}x{}@{}",
                                    cd.width, cd.height, cd.dpi
                                );
                                send(
                                    &sock,
                                    src,
                                    &Frame {
                                        stream: frame::Stream::Ctrl as i32,
                                        r#type: frame::Type::Data as i32,
                                        seq: 1,
                                        msg_id: 1,
                                        frag: 0,
                                        frag_total: 0,
                                        media_pts: 0,
                                        media_flags: 0,
                                        payload: Some(frame::Payload::Data(
                                            CtrlMsg {
                                                msg: Some(ctrl_msg::Msg::DisplayReady(
                                                    DisplayReady {
                                                        display_id: 1,
                                                        codec_id: 0x68323634,
                                                        width: cd.width,
                                                        height: cd.height,
                                                        dpi: cd.dpi,
                                                    },
                                                )),
                                            }
                                            .encode_to_vec(),
                                        )),
                                    },
                                );
                                if let Some(cfg) = &video_config {
                                    send(
                                        &sock,
                                        src,
                                        &Frame {
                                            stream: frame::Stream::Ctrl as i32,
                                            r#type: frame::Type::Data as i32,
                                            seq: 2,
                                            msg_id: 2,
                                            frag: 0,
                                            frag_total: 0,
                                            media_pts: 0,
                                            media_flags: 0,
                                            payload: Some(frame::Payload::Data(
                                                CtrlMsg {
                                                    msg: Some(ctrl_msg::Msg::MediaConfig(
                                                        kulua_proto::generated::MediaConfig {
                                                            stream: 2,
                                                            data: cfg.clone(),
                                                        },
                                                    )),
                                                }
                                                .encode_to_vec(),
                                            )),
                                        },
                                    );
                                }
                                started = true;
                            }
                            Some(ctrl_msg::Msg::StartApp(s)) => {
                                println!("[mockphone] StartApp {}", s.package);
                            }
                            Some(ctrl_msg::Msg::InjectTouch(t)) => {
                                println!("[mockphone] InjectTouch x={} y={}", t.x, t.y);
                            }
                            Some(ctrl_msg::Msg::InjectKeycode(k)) => {
                                println!("[mockphone] InjectKeycode code={}", k.keycode);
                            }
                            Some(ctrl_msg::Msg::ResizeDisplay(r)) => {
                                println!("[mockphone] ResizeDisplay {}x{}", r.width, r.height);
                            }
                            other => {
                                println!("[mockphone] ctrl: {:?}", variant_name(&other));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn variant_name(m: &Option<ctrl_msg::Msg>) -> &'static str {
    match m {
        Some(ctrl_msg::Msg::InjectKeycode(_)) => "InjectKeycode",
        Some(ctrl_msg::Msg::InjectText(_)) => "InjectText",
        Some(ctrl_msg::Msg::InjectTouch(_)) => "InjectTouch",
        Some(ctrl_msg::Msg::InjectScroll(_)) => "InjectScroll",
        Some(ctrl_msg::Msg::BackOrScreenOn(_)) => "BackOrScreenOn",
        Some(ctrl_msg::Msg::SetClipboard(_)) => "SetClipboard",
        Some(ctrl_msg::Msg::StartApp(_)) => "StartApp",
        Some(ctrl_msg::Msg::ResizeDisplay(_)) => "ResizeDisplay",
        Some(ctrl_msg::Msg::CreateDisplay(_)) => "CreateDisplay",
        Some(ctrl_msg::Msg::DestroyDisplay(_)) => "DestroyDisplay",
        Some(ctrl_msg::Msg::SetAudioCodec(_)) => "SetAudioCodec",
        Some(ctrl_msg::Msg::ClipboardChanged(_)) => "ClipboardChanged",
        Some(ctrl_msg::Msg::AckClipboard(_)) => "AckClipboard",
        Some(ctrl_msg::Msg::MediaConfig(_)) => "MediaConfig",
        Some(ctrl_msg::Msg::AudioReady(_)) => "AudioReady",
        Some(ctrl_msg::Msg::DisplayReady(_)) => "DisplayReady",
        None => "None",
    }
}
