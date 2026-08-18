//! kulua-server 连接（UDP 直连客户端）。
//!
//! 架构（阶段 5）：server 由 daemon session 统一部署（`kulua-server.jar`，绑定
//! phone 网络 UDP 端口），viewer 只负责连接：
//! - HELLO{scid, audio=false} → HELLO_ACK 握手（scid 由端口确定，与 server 一致）
//! - 发送 `CtrlMsg.create_display{WxH/DPI}` → 收 `DisplayReady{display_id, codec_id}`
//! - 视频帧 / 控制事件经同一个 UdpSession 的流多路复用（stream=VIDEO / CTRL）
//! 窗口关闭只 close 会话（server 由 session 管理，不 kill）。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use kulua_proto::generated::{CtrlMsg, ctrl_msg};
use kulua_proto::session::{Event, UdpSession};

/// scid 前缀（与 sync-core/src/scrcpy.rs 的 SCID_PREFIX 一致）：0x4B4C << 16。
pub const SCID_PREFIX: u32 = 0x4B4C_0000;

/// 由 UDP 端口推导确定性 scid（与 daemon 一致，无需额外传入）。
pub fn scid_for_port(port: u16) -> String {
    format!("{:08x}", SCID_PREFIX | u32::from(port))
}

/// 已连接的 viewer 会话：持有 UDP 会话 + server 分配的虚拟显示器 id。
pub struct ViewerSession {
    /// UDP 会话（事件流：Video / Control；send_ctrl 注入输入）
    pub session: UdpSession,
    /// server 分配的虚拟显示器 id（输入注入/RESIZE/START_APP 用）
    pub display_id: u32,
    /// 视频 codec id（4B ASCII，如 "h264"）
    pub codec_id: u32,
}

impl ViewerSession {
    /// 连接已有 kulua-server 并创建虚拟显示器。
    ///
    /// 阻塞直到 HELLO 握手 + DisplayReady 都完成（或超时失败）。
    pub fn connect(addr: SocketAddr, display: &str) -> Result<Self, String> {
        let (width, height, dpi) = parse_display(display)?;
        let scid = scid_for_port(addr.port());
        let mut session = UdpSession::connect(addr, &scid, false)
            .map_err(|e| format!("连接 {addr} 失败: {e}"))?;

        // 发送 CreateDisplay（可靠 control）→ phone 创建虚拟显示器 + 编码器
        session
            .send_ctrl(&super::control::create_display(
                width as u32,
                height as u32,
                dpi as u32,
            ))
            .map_err(|e| format!("CreateDisplay 发送失败: {e}"))?;

        // 等待 DisplayReady（5s 超时）
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match session.try_recv() {
                Some(Event::Control(msg)) => {
                    if let Some(ctrl_msg::Msg::DisplayReady(d)) = msg.msg {
                        println!(
                            "[viewer] 虚拟显示器 #{} 已创建 ({}x{}@{})",
                            d.display_id, d.width, d.height, d.dpi
                        );
                        return Ok(ViewerSession {
                            session,
                            display_id: d.display_id,
                            codec_id: d.codec_id,
                        });
                    }
                }
                Some(Event::Error(e)) => return Err(format!("会话错误: {e}")),
                Some(Event::Closed) => return Err("会话被关闭".into()),
                Some(_) => {}
                None => {
                    if Instant::now() >= deadline {
                        session.close();
                        return Err("等待 DisplayReady 超时（server/网络异常？）".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    /// 发送一条 control 指令（可靠）。
    pub fn send_ctrl(&mut self, msg: &CtrlMsg) -> Result<(), String> {
        self.session
            .send_ctrl(msg)
            .map_err(|e| format!("控制通道写入失败: {e}"))
    }

    /// 取出底层 UDP 会话（把事件消费交给主循环）。
    pub fn into_inner(self) -> UdpSession {
        self.session
    }
}

/// 解析 `WxH/DPI` 显示器规格字符串（如 `1280x960/160`）。
pub fn parse_display(display: &str) -> Result<(u16, u16, u16), String> {
    let (size_part, dpi_part) = display
        .split_once('/')
        .ok_or_else(|| format!("非法显示器规格（应为 WxH/DPI）: {}", display))?;
    let (w, h) = size_part
        .split_once('x')
        .ok_or_else(|| format!("非法显示器规格（应为 WxH/DPI）: {}", display))?;
    let width: u16 = w.trim().parse().map_err(|_| format!("非法宽度: {}", w))?;
    let height: u16 = h.trim().parse().map_err(|_| format!("非法高度: {}", h))?;
    let dpi: u16 = dpi_part
        .trim()
        .parse()
        .map_err(|_| format!("非法 DPI: {}", dpi_part))?;
    if width == 0 || height == 0 || dpi == 0 {
        return Err(format!("显示器规格必须为正数: {}", display));
    }
    Ok((width, height, dpi))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kulua_proto::generated::frame::{self, Payload};
    use kulua_proto::prost::Message;

    #[test]
    fn scid_for_port_matches_daemon_formula() {
        assert_eq!(scid_for_port(27183), "4b4c6a2f");
        assert_eq!(scid_for_port(0), "4b4c0000");
    }

    #[test]
    fn parse_display_accepts_standard_format() {
        assert_eq!(parse_display("1280x960/160"), Ok((1280, 960, 160)));
        assert_eq!(parse_display(" 1920x1080/440 "), Ok((1920, 1080, 440)));
    }

    #[test]
    fn parse_display_rejects_bad_input() {
        assert!(parse_display("").is_err());
        assert!(parse_display("1280x960").is_err(), "缺少 /DPI");
        assert!(parse_display("1280/160").is_err(), "缺少 x");
        assert!(parse_display("axb/160").is_err(), "非数字");
        assert!(parse_display("0x960/160").is_err(), "零宽");
        assert!(parse_display("1280x0/160").is_err(), "零高");
        assert!(parse_display("1280x960/0").is_err(), "零 DPI");
        assert!(parse_display("1280x960/abc").is_err(), "非数字 DPI");
    }

    /// 模拟 kulua-server 的 mock（UDP）：HELLO 回 HELLO_ACK，CreateDisplay 回
    /// DisplayReady。验证 viewer 的握手与 server 完全一致（scid/message 编码）。
    #[test]
    fn connect_handshake_matches_kulua_server_protocol() {
        use std::net::UdpSocket as StdUdpSocket;
        use std::sync::mpsc;

        let listen = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = listen.local_addr().unwrap().port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // 等待第一个 HELLO
            let mut buf = vec![0u8; 2048];
            let (n, src) = listen.recv_from(&mut buf).unwrap();
            let hello = kulua_proto::generated::Frame::decode(&buf[..n]).unwrap();
            assert_eq!(frame::Type::try_from(hello.r#type), Ok(frame::Type::Hello));
            let hello_scid = match &hello.payload {
                Some(Payload::Hello(h)) => h.scid.clone(),
                _ => panic!("expected HELLO payload"),
            };
            assert_eq!(hello_scid, scid_for_port(port));
            // 回 HELLO_ACK
            let ack = kulua_proto::generated::Frame {
                stream: frame::Stream::Ctrl as i32,
                r#type: frame::Type::HelloAck as i32,
                seq: 0,
                msg_id: 0,
                frag: 0,
                frag_total: 0,
                media_pts: 0,
                media_flags: 0,
                payload: Some(Payload::HelloAck(kulua_proto::generated::HelloAck {
                    scid: scid_for_port(port),
                    audio_codec: 0,
                    video_codec: 0x68323634,
                })),
            };
            let mut wire = Vec::with_capacity(64);
            Message::encode(&ack, &mut wire).unwrap();
            listen.send_to(&wire, src).unwrap();

            // 等待 CreateDisplay（control DATA）→ 回 DisplayReady
            loop {
                let (n, _) = listen.recv_from(&mut buf).unwrap();
                let frame = kulua_proto::generated::Frame::decode(&buf[..n]).unwrap();
                if frame::Type::try_from(frame.r#type) == Ok(frame::Type::Data)
                    && frame::Stream::try_from(frame.stream) == Ok(frame::Stream::Ctrl)
                {
                    let Some(Payload::Data(data)) = &frame.payload else {
                        continue;
                    };
                    let ctrl = CtrlMsg::decode(data.as_slice()).unwrap();
                    if let Some(ctrl_msg::Msg::CreateDisplay(cd)) = ctrl.msg {
                        let ready = kulua_proto::generated::Frame {
                            stream: frame::Stream::Ctrl as i32,
                            r#type: frame::Type::Data as i32,
                            seq: 1,
                            msg_id: 1,
                            frag: 0,
                            frag_total: 0,
                            media_pts: 0,
                            media_flags: 0,
                            payload: Some(Payload::Data(
                                CtrlMsg {
                                    msg: Some(ctrl_msg::Msg::DisplayReady(
                                        kulua_proto::generated::DisplayReady {
                                            display_id: 42,
                                            codec_id: 0x68323634,
                                            width: cd.width,
                                            height: cd.height,
                                            dpi: cd.dpi,
                                        },
                                    )),
                                }
                                .encode_to_vec(),
                            )),
                        };
                        let mut w = Vec::with_capacity(64);
                        Message::encode(&ready, &mut w).unwrap();
                        listen.send_to(&w, src).unwrap();
                        done_tx.send((cd.width, cd.height, cd.dpi)).unwrap();
                        break;
                    }
                }
            }
        });

        let session = ViewerSession::connect(addr, "1280x960/160").unwrap();
        assert_eq!(session.display_id, 42);
        assert_eq!(session.codec_id, 0x68323634);
        let (w, h, dpi) = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!((w, h, dpi), (1280, 960, 160));
    }
}
