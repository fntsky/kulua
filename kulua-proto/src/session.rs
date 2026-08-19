//! UDP 会话客户端（daemon 与 fusion-viewer 共用）。
//!
//! 基于 `std::net::UdpSocket`（阻塞 + 短读超时）+ 读写两个后台线程：
//! - reader：recv 循环 → 解析 `Frame` → 分派（control 投递 / ACK / media 重装 / HELLO_ACK）
//! - writer：定时 flush ACK、control 重传、心跳、判死
//!
//! 事件经 tokio unbounded channel 暴露：daemon 用 `recv().await`（异步上下文），
//! fusion-viewer 用 `try_recv()`（winit 事件循环轮询）。

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use prost::Message;

use crate::codec::{AssembledMedia, MediaReassembler, decode_ctrl, encode_ctrl};
use crate::generated::{CtrlMsg, Frame, Hello, HelloAck, ctrl_msg, frame};
use crate::reliable::{ReliableReceiver, ReliableSender};

/// 会话事件。
#[derive(Debug, Clone)]
pub enum Event {
    /// HELLO_ACK 已收到（握手完成）。
    Connected,
    /// 收到的 control 指令（phone → PC：剪贴板 / 显示器就绪 / 音频就绪等）。
    Control(CtrlMsg),
    /// 重装完成的音频帧。
    Audio(AssembledMedia),
    /// 重装完成的视频帧。
    Video(AssembledMedia),
    /// 会话结束（BYE / 对端消失 / 自身判死）。
    Closed,
    /// 不可恢复错误。
    Error(String),
}

/// 内部握手状态。
#[derive(Default)]
struct State {
    connected: bool,
    hello_ack: Option<HelloAck>,
}

/// 心跳间隔。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// 完成后清理媒体残留的分片缓冲超时。
const MEDIA_SWEEP: Duration = Duration::from_millis(500);

/// 一个与 phone 上 kulua-server 直连的 UDP 会话。
pub struct UdpSession {
    socket: UdpSocket,
    scid: String,
    state: Arc<Mutex<State>>,
    ctrl_tx: Arc<Mutex<ReliableSender>>,
    ctrl_rx: Arc<Mutex<ReliableReceiver>>,
    audio_rx: Arc<Mutex<MediaReassembler>>,
    video_rx: Arc<Mutex<MediaReassembler>>,
    ev_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    msg_id: std::cell::Cell<u32>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    closed: Arc<AtomicBool>,
}

impl UdpSession {
    /// 连接 peer（phone ip:port）并完成 HELLO/HELLO_ACK 握手。
    ///
    /// 阻塞至多 10s；成功返回已就绪的会话（`event` 中不会再有 `Connected`）。
    pub fn connect(peer: SocketAddr, scid: &str, want_audio: bool) -> Result<Self, String> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        socket
            .connect(peer)
            .map_err(|e| format!("connect {peer}: {e}"))?;

        let (ev_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let mut s = UdpSession {
            socket,
            scid: scid.to_string(),
            state: Arc::new(Mutex::new(State::default())),
            ctrl_tx: Arc::new(Mutex::new(ReliableSender::new())),
            ctrl_rx: Arc::new(Mutex::new(ReliableReceiver::new())),
            audio_rx: Arc::new(Mutex::new(MediaReassembler::new())),
            video_rx: Arc::new(Mutex::new(MediaReassembler::new())),
            ev_tx,
            rx,
            msg_id: std::cell::Cell::new(1),
            reader: None,
            writer: None,
            closed: closed.clone(),
        };
        s.start_threads();

        // 握手：反复发 HELLO 直到 HELLO_ACK
        let deadline = Instant::now() + Duration::from_secs(10);
        let hello = Frame {
            stream: frame::Stream::Ctrl as i32,
            r#type: frame::Type::Hello as i32,
            seq: 0,
            msg_id: 0,
            frag: 0,
            frag_total: 0,
            media_pts: 0,
            media_flags: 0,
            payload: Some(frame::Payload::Hello(Hello {
                scid: s.scid.clone(),
                audio: want_audio,
            })),
        };
        let mut wire = Vec::with_capacity(64);
        prost::Message::encode(&hello, &mut wire).expect("encode hello");
        let mut last_progress = Instant::now();
        while !s.state.lock().unwrap().connected {
            if Instant::now() > deadline {
                s.close();
                return Err(format!(
                    "HELLO 握手超时（{peer}）：server 未就绪或端口不通——确认设备端 kulua-server \
                     已启动且绑定了该端口，且此地址是手机当前 Wi-Fi IP"
                ));
            }
            // 握手不是静默的：每 2s 打印一次进度，避免“卡在连接中”无感知
            if last_progress.elapsed() >= Duration::from_secs(2) {
                let elapsed = deadline.saturating_duration_since(Instant::now());
                eprintln!(
                    "[kulua] 仍在等待 {} 握手（还剩 {:.0}s）……若始终超时，检查设备 server/端口/IP",
                    peer,
                    elapsed.as_secs_f32()
                );
                last_progress = Instant::now();
            }
            let _ = s.socket.send(&wire);
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(s)
    }

    /// 启动 reader（收包分发）与 writer（ACK/重传/心跳/判死）两个后台线程。
    fn start_threads(&mut self) {
        // ── reader：收包 → 解析 → 分发事件 ──
        {
            let socket = self.socket.try_clone().expect("clone udp socket");
            let state = self.state.clone();
            let ctrl_tx = self.ctrl_tx.clone();
            let ctrl_rx = self.ctrl_rx.clone();
            let audio_rx = self.audio_rx.clone();
            let video_rx = self.video_rx.clone();
            let ev_tx = self.ev_tx.clone();
            let closed = self.closed.clone();
            self.reader = Some(std::thread::spawn(move || {
                let mut buf = vec![0u8; 1472];
                while !closed.load(Ordering::SeqCst) {
                    match socket.recv(&mut buf) {
                        Ok(n) => {
                            let maybe = Frame::decode(&buf[..n]);
                            match maybe {
                                Ok(frame) => {
                                    for evt in Self::dispatch(
                                        &frame, &state, &ctrl_tx, &ctrl_rx, &audio_rx, &video_rx,
                                    ) {
                                        let _ = ev_tx.send(evt);
                                    }
                                }
                                Err(_) => {
                                    // 非法 datagram：忽略（恶意/损坏）
                                }
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                        // 连接式 UDP：对端未绑定端口（server 尚在冷启动）时会收到
                        // ICMP 端口不可达 → Windows 上表现为 ConnectionReset /
                        // ConnectionRefused。这在 HELLO 握手重试期是正常的，忽略。
                        Err(e)
                            if e.kind() == std::io::ErrorKind::ConnectionReset
                                || e.kind() == std::io::ErrorKind::ConnectionRefused =>
                        {
                            continue;
                        }
                        Err(e) => {
                            if !closed.load(Ordering::SeqCst) {
                                let _ = ev_tx.send(Event::Error(format!("udp recv: {e}")));
                                closed.store(true, Ordering::SeqCst);
                            }
                            break;
                        }
                    }
                }
            }));
        }

        // ── writer：ACK flush / 重传 / 心跳 / 判死 ──
        {
            let socket = self.socket.try_clone().expect("clone udp socket");
            let ctrl_tx = self.ctrl_tx.clone();
            let ctrl_rx = self.ctrl_rx.clone();
            let audio_rx = self.audio_rx.clone();
            let video_rx = self.video_rx.clone();
            let ev_tx = self.ev_tx.clone();
            let closed = self.closed.clone();
            self.writer = Some(std::thread::spawn(move || {
                let mut last_ack = 0u32;
                let mut last_hb = Instant::now();
                while !closed.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    // ACK flush
                    {
                        let ack = ctrl_rx.lock().unwrap().ack_seq();
                        if ack != last_ack {
                            last_ack = ack;
                            if let Ok(wire) = encode_ack(ack) {
                                let _ = socket.send(&wire);
                            }
                        }
                    }
                    // control 重传
                    {
                        let mut tx = ctrl_tx.lock().unwrap();
                        for wire in tx.due_timeouts(now) {
                            let _ = socket.send(&wire);
                        }
                        if tx.is_stalled() {
                            let _ = ev_tx.send(Event::Error("control 确认超时，连接失效".into()));
                            closed.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    // 心跳
                    if now.duration_since(last_hb) >= HEARTBEAT_INTERVAL {
                        last_hb = now;
                        if let Ok(wire) = encode_heartbeat() {
                            let _ = socket.send(&wire);
                        }
                    }
                    // 媒体残留清理
                    {
                        audio_rx.lock().unwrap().sweep(MEDIA_SWEEP);
                        video_rx.lock().unwrap().sweep(MEDIA_SWEEP);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }));
        }
    }

    /// 分发一个收到的 Frame，返回要发出的事件列表（可能为空）。
    fn dispatch(
        frame: &Frame,
        state: &Arc<Mutex<State>>,
        ctrl_tx: &Arc<Mutex<ReliableSender>>,
        ctrl_rx: &Arc<Mutex<ReliableReceiver>>,
        audio_rx: &Arc<Mutex<MediaReassembler>>,
        video_rx: &Arc<Mutex<MediaReassembler>>,
    ) -> Vec<Event> {
        match frame::Type::try_from(frame.r#type) {
            Ok(frame::Type::Ack) => {
                if let Some(frame::Payload::AckSeq(ack)) = &frame.payload {
                    ctrl_tx.lock().unwrap().on_ack(*ack);
                }
                Vec::new()
            }
            Ok(frame::Type::HelloAck) => {
                if let Some(frame::Payload::HelloAck(ack)) = &frame.payload {
                    let mut st = state.lock().unwrap();
                    st.connected = true;
                    st.hello_ack = Some(ack.clone());
                    vec![Event::Connected]
                } else {
                    Vec::new()
                }
            }
            Ok(frame::Type::Data) => match frame::Stream::try_from(frame.stream) {
                Ok(frame::Stream::Ctrl) => {
                    let mut rx = ctrl_rx.lock().unwrap();
                    if rx.is_dead() {
                        return vec![Event::Error("control 接收缓冲溢出".into())];
                    }
                    let delivered = rx.on_frame(frame);
                    let mut out = Vec::new();
                    for d in delivered {
                        if let Some(msg) = decode_ctrl(&d) {
                            out.push(Event::Control(msg));
                        }
                    }
                    out
                }
                Ok(frame::Stream::Audio) => audio_rx
                    .lock()
                    .unwrap()
                    .push(frame)
                    .into_iter()
                    .map(Event::Audio)
                    .collect(),
                Ok(frame::Stream::Video) => video_rx
                    .lock()
                    .unwrap()
                    .push(frame)
                    .into_iter()
                    .map(Event::Video)
                    .collect(),
                Err(_) => Vec::new(),
            },
            Ok(frame::Type::Bye) => vec![Event::Closed],
            Ok(frame::Type::Heartbeat) => Vec::new(),
            Ok(frame::Type::Hello) => Vec::new(), // PC 是客户端，不应收到 HELLO
            Err(_) => Vec::new(),
        }
    }

    /// 发送一条 control 指令（可靠）。
    pub fn send_ctrl(&self, msg: &CtrlMsg) -> Result<(), String> {
        let bytes = encode_ctrl(msg);
        let id = self.msg_id.get();
        self.msg_id.set(id.wrapping_add(1));
        let frames = self.ctrl_tx.lock().unwrap().send_ctrl_bytes(id, &bytes);
        for f in frames {
            let mut wire = Vec::with_capacity(f.encoded_len() + 8);
            prost::Message::encode(&f, &mut wire).map_err(|e| format!("encode: {e}"))?;
            self.socket
                .send(&wire)
                .map_err(|e| format!("udp send: {e}"))?;
        }
        Ok(())
    }

    /// 发送 HEARTBEAT（保活；phone 把任意 datagram 都视为存活）。
    pub fn send_heartbeat(&self) -> Result<(), String> {
        let wire = encode_heartbeat().map_err(|e| e)?;
        self.socket
            .send(&wire)
            .map_err(|e| format!("udp send: {e}"))?;
        Ok(())
    }

    /// 发送 BYE（优雅关闭会话）。
    pub fn send_bye(&self) {
        if let Ok(wire) = encode_bye() {
            let _ = self.socket.send(&wire);
        }
    }

    /// 握手后返回的 codec 状态。
    pub fn hello_ack(&self) -> Option<HelloAck> {
        self.state.lock().unwrap().hello_ack.clone()
    }

    /// 视频流累计丢失的消息数（HUD 丢包率用）。
    pub fn video_lost(&self) -> u64 {
        self.video_rx.lock().unwrap().lost()
    }

    /// 关闭会话（发 BYE + 停线程 + 关 socket）。
    pub fn close(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        self.send_bye();
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
        let _ = self.rx.try_recv(); // 清残留
    }

    /// tokio 异步接收事件（daemon）。
    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }

    /// 同步非阻塞接收事件（fusion-viewer 事件循环轮询）。
    pub fn try_recv(&mut self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// 事件接收器是否还有生产端存活。
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl Drop for UdpSession {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

fn encode_ack(ack_seq: u32) -> Result<Vec<u8>, String> {
    let f = Frame {
        stream: frame::Stream::Ctrl as i32,
        r#type: frame::Type::Ack as i32,
        seq: 0,
        msg_id: 0,
        frag: 0,
        frag_total: 0,
        media_pts: 0,
        media_flags: 0,
        payload: Some(frame::Payload::AckSeq(ack_seq)),
    };
    let mut wire = Vec::with_capacity(16);
    prost::Message::encode(&f, &mut wire).map_err(|e| format!("encode ack: {e}"))?;
    Ok(wire)
}

fn encode_heartbeat() -> Result<Vec<u8>, String> {
    let f = Frame {
        stream: frame::Stream::Ctrl as i32,
        r#type: frame::Type::Heartbeat as i32,
        seq: 0,
        msg_id: 0,
        frag: 0,
        frag_total: 0,
        media_pts: 0,
        media_flags: 0,
        payload: None,
    };
    let mut wire = Vec::with_capacity(16);
    prost::Message::encode(&f, &mut wire).map_err(|e| format!("encode heartbeat: {e}"))?;
    Ok(wire)
}

fn encode_bye() -> Result<Vec<u8>, String> {
    let f = Frame {
        stream: frame::Stream::Ctrl as i32,
        r#type: frame::Type::Bye as i32,
        seq: 0,
        msg_id: 0,
        frag: 0,
        frag_total: 0,
        media_pts: 0,
        media_flags: 0,
        payload: None,
    };
    let mut wire = Vec::with_capacity(16);
    prost::Message::encode(&f, &mut wire).map_err(|e| format!("encode bye: {e}"))?;
    Ok(wire)
}

/// 由请求构造 control 指令的小工具集。
pub mod msgs {
    use super::*;
    use crate::generated::{
        BackOrScreenOn, CreateDisplay, InjectKeycode, InjectScroll, InjectText, InjectTouch,
        ResizeDisplay, SetClipboard, StartApp,
    };

    pub fn inject_keycode(
        display_id: u32,
        action: u32,
        keycode: u32,
        repeat: u32,
        meta_state: u32,
    ) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::InjectKeycode(InjectKeycode {
                display_id,
                action,
                keycode,
                repeat,
                meta_state,
            })),
        }
    }

    pub fn inject_text(display_id: u32, text: &str) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::InjectText(InjectText {
                display_id,
                text: text.to_string(),
            })),
        }
    }

    pub fn inject_touch(
        display_id: u32,
        action: u32,
        pointer_id: u64,
        x: i32,
        y: i32,
        screen_width: u32,
        screen_height: u32,
        pressure: f32,
        action_button: u32,
        buttons: u32,
    ) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::InjectTouch(InjectTouch {
                display_id,
                action,
                pointer_id,
                x,
                y,
                screen_width,
                screen_height,
                pressure,
                action_button,
                buttons,
            })),
        }
    }

    pub fn inject_scroll(
        display_id: u32,
        x: i32,
        y: i32,
        screen_width: u32,
        screen_height: u32,
        hscroll: f32,
        vscroll: f32,
        buttons: u32,
    ) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::InjectScroll(InjectScroll {
                display_id,
                x,
                y,
                screen_width,
                screen_height,
                hscroll,
                vscroll,
                buttons,
            })),
        }
    }

    pub fn back_or_screen_on(display_id: u32, action: u32) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::BackOrScreenOn(BackOrScreenOn {
                display_id,
                action,
            })),
        }
    }

    pub fn set_clipboard(sequence: u64, paste: bool, text: &str) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::SetClipboard(SetClipboard {
                sequence,
                paste,
                text: text.to_string(),
            })),
        }
    }

    pub fn start_app(display_id: u32, package: &str) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::StartApp(StartApp {
                display_id,
                package: package.to_string(),
            })),
        }
    }

    pub fn resize_display(display_id: u32, width: u32, height: u32) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::ResizeDisplay(ResizeDisplay {
                display_id,
                width,
                height,
            })),
        }
    }

    pub fn create_display(width: u32, height: u32, dpi: u32) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::CreateDisplay(CreateDisplay {
                width,
                height,
                dpi,
            })),
        }
    }

    pub fn destroy_display(display_id: u32) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::DestroyDisplay(
                crate::generated::DestroyDisplay { display_id },
            )),
        }
    }

    pub fn set_audio_codec(codec: u32) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::SetAudioCodec(
                crate::generated::SetAudioCodec { codec },
            )),
        }
    }
}
