//! UDP 会话客户端（daemon 与 fusion-viewer 共用）。
//!
//! control 端口：`std::net::UdpSocket`（阻塞 + 短读超时）+ 读写两个后台线程：
//! - reader：recv 循环 → 解析 `Frame` → 分派（control 投递 / ACK / HELLO_ACK）
//! - writer：定时 flush ACK、control 重传、心跳、媒体 OPEN 注册、判死
//!
//! 媒体端口（video=P+1 / audio=P+2，按需开启）：各自 reader 线程解析 33B
//! 定长头 → 重装 → 事件；writer 在媒体端口收到首个有效数据报前每 500ms
//! 重发 OPEN 注册包（phone 收到任意合法头数据报即登记端点）。
//!
//! 事件经 tokio unbounded channel 暴露：daemon 用 `recv().await`（异步上下文），
//! fusion-viewer 用 `try_recv()`（winit 事件循环轮询）。

use parking_lot::Mutex;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use prost::Message;

use crate::codec::{
    AUDIO_PORT_OFFSET, AssembledMedia, MEDIA_HDR_LEN, MEDIA_OPEN_TOTAL, MediaFragment,
    MediaReassembler, VIDEO_PORT_OFFSET, decode_ctrl, decode_media_datagram, encode_ctrl,
    encode_media_datagram,
};
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
/// 媒体 OPEN 注册包重发间隔（收到首个媒体包前）。
const OPEN_INTERVAL: Duration = Duration::from_millis(500);
/// 完成后清理媒体残留的分片缓冲超时。
const MEDIA_SWEEP: Duration = Duration::from_millis(500);

/// 会话连接参数。
#[derive(Debug, Clone, Copy)]
pub struct ConnectOpts {
    /// 打开音频媒体端口（daemon 会话恒 true）：音频可在会话中途热开启，
    /// 接收端点必须先注册好，否则开启后 phone 无处发送且无从再登记。
    pub audio_link: bool,
    /// `Hello.audio`：是否请求 phone 在握手时就起采集（= 会话初始音频目标）。
    pub hello_audio: bool,
    /// 打开视频媒体端口（融合窗口）。
    pub video: bool,
}

/// 一个与 phone 上 kulua-server 直连的 UDP 会话。
pub struct UdpSession {
    socket: UdpSocket,
    /// 媒体 socket（按需：want_video→video 端口 P+1，want_audio→audio 端口 P+2）。
    audio_socket: Option<UdpSocket>,
    video_socket: Option<UdpSocket>,
    /// phone 分配的客户端 id（HELLO_ACK 带回；媒体数据报头路由用）。
    client_id: Arc<AtomicU32>,
    /// 媒体端口已收到有效数据（writer 停止 OPEN 重发）。
    audio_flowing: Arc<AtomicBool>,
    video_flowing: Arc<AtomicBool>,
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
    /// 媒体 reader 线程（close 时 join）。
    media_readers: Vec<JoinHandle<()>>,
    closed: Arc<AtomicBool>,
}

impl UdpSession {
    /// 连接 peer（phone ctrl 端口 ip:port）并完成 HELLO/HELLO_ACK 握手。
    ///
    /// 媒体端口（audio=P+2 / video=P+1）按 [`ConnectOpts`] 打开。阻塞至多 10s；
    /// 成功返回已就绪的会话（`event` 中不会再有 `Connected`）。
    pub fn connect(peer: SocketAddr, scid: &str, opts: ConnectOpts) -> Result<Self, String> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();
        socket
            .connect(peer)
            .map_err(|e| format!("connect {peer}: {e}"))?;

        // 媒体 socket：与 ctrl 独立的本地端口，connect() 过滤非 phone 源。
        // 握手期 server 可能尚未绑媒体端口 → ICMP 不可达（Windows 表现为
        // ConnectionReset），reader 按 ConnectionReset 忽略。
        let bind_media = |offset: u16| -> Result<UdpSocket, String> {
            let s = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind media: {e}"))?;
            s.set_read_timeout(Some(Duration::from_millis(200))).ok();
            let media_peer = SocketAddr::new(peer.ip(), peer.port() + offset);
            s.connect(media_peer)
                .map_err(|e| format!("connect {media_peer}: {e}"))?;
            Ok(s)
        };
        let audio_socket = if opts.audio_link {
            Some(bind_media(AUDIO_PORT_OFFSET)?)
        } else {
            None
        };
        let video_socket = if opts.video {
            Some(bind_media(VIDEO_PORT_OFFSET)?)
        } else {
            None
        };

        let (ev_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let closed = Arc::new(AtomicBool::new(false));
        let mut s = UdpSession {
            socket,
            audio_socket,
            video_socket,
            client_id: Arc::new(AtomicU32::new(0)),
            audio_flowing: Arc::new(AtomicBool::new(false)),
            video_flowing: Arc::new(AtomicBool::new(false)),
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
            media_readers: Vec::new(),
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
            payload: Some(frame::Payload::Hello(Hello {
                scid: s.scid.clone(),
                audio: opts.hello_audio,
            })),
        };
        let mut wire = Vec::with_capacity(64);
        prost::Message::encode(&hello, &mut wire).expect("encode hello");
        let mut last_progress = Instant::now();
        while !s.state.lock().connected {
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

    /// 启动 reader（收包分发）、writer（ACK/重传/心跳/OPEN/判死）与媒体 reader。
    fn start_threads(&mut self) {
        // ── reader：ctrl 收包 → 解析 → 分发事件 ──
        {
            let socket = self.socket.try_clone().expect("clone udp socket");
            let state = self.state.clone();
            let ctrl_tx = self.ctrl_tx.clone();
            let ctrl_rx = self.ctrl_rx.clone();
            let client_id = self.client_id.clone();
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
                                        &frame, &state, &ctrl_tx, &ctrl_rx, &client_id,
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

        // ── 媒体 reader：每 socket 一线程，33B 头解析 → 重装 → 事件 ──
        if let Some(sock) = self.audio_socket.as_ref() {
            let sock = sock.try_clone().expect("clone audio socket");
            self.media_readers.push(Self::spawn_media_reader(
                sock,
                false,
                self.client_id.clone(),
                self.audio_flowing.clone(),
                self.audio_rx.clone(),
                self.ev_tx.clone(),
                self.closed.clone(),
            ));
        }
        if let Some(sock) = self.video_socket.as_ref() {
            let sock = sock.try_clone().expect("clone video socket");
            self.media_readers.push(Self::spawn_media_reader(
                sock,
                true,
                self.client_id.clone(),
                self.video_flowing.clone(),
                self.video_rx.clone(),
                self.ev_tx.clone(),
                self.closed.clone(),
            ));
        }

        // ── writer：ACK flush / 重传 / 心跳 / OPEN / 判死 ──
        {
            let socket = self.socket.try_clone().expect("clone udp socket");
            let ctrl_tx = self.ctrl_tx.clone();
            let ctrl_rx = self.ctrl_rx.clone();
            let audio_rx = self.audio_rx.clone();
            let video_rx = self.video_rx.clone();
            let client_id = self.client_id.clone();
            let audio_flowing = self.audio_flowing.clone();
            let video_flowing = self.video_flowing.clone();
            let audio_socket = self
                .audio_socket
                .as_ref()
                .map(|s| s.try_clone().expect("clone audio socket"));
            let video_socket = self
                .video_socket
                .as_ref()
                .map(|s| s.try_clone().expect("clone video socket"));
            let ev_tx = self.ev_tx.clone();
            let closed = self.closed.clone();
            self.writer = Some(std::thread::spawn(move || {
                let mut last_hb = Instant::now();
                let mut last_open = Instant::now();
                let mut last_rtx_warn = Instant::now();
                while !closed.load(Ordering::SeqCst) {
                    let now = Instant::now();
                    // ACK flush
                    {
                        if let Some(ack) = ctrl_rx.lock().take_ack() {
                            if let Ok(wire) = encode_ack(ack) {
                                let _ = socket.send(&wire);
                            }
                        }
                    }
                    // control 重传
                    {
                        let mut tx = ctrl_tx.lock();
                        let wires = tx.due_timeouts(now);
                        // 诊断：重传压力 = 对端 ACK 断供（phone ctrl 循环被阻塞/
                        // 链路拥塞），提前告警，别等判死才发现
                        if !wires.is_empty()
                            && tx.pending() >= 8
                            && now.duration_since(last_rtx_warn) >= Duration::from_secs(2)
                        {
                            last_rtx_warn = now;
                            eprintln!(
                                "[kulua] ctrl 重传压力：pending={} 本轮重传 {} 片",
                                tx.pending(),
                                wires.len()
                            );
                        }
                        for wire in wires {
                            let _ = socket.send(&wire);
                        }
                        if tx.is_stalled() {
                            eprintln!(
                                "[kulua] ctrl 判死：pending={} 分片持续无 ACK（对端卡死或链路中断）",
                                tx.pending()
                            );
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
                    // 媒体 OPEN 注册：phone 收到任意合法头数据报即登记端点；
                    // 收到首个媒体包前每 500ms 重发（编码器尚未推流时靠它上线）
                    if now.duration_since(last_open) >= OPEN_INTERVAL {
                        last_open = now;
                        let cid = client_id.load(Ordering::SeqCst);
                        if cid != 0 {
                            let wire = encode_open(cid);
                            if !video_flowing.load(Ordering::SeqCst) {
                                if let Some(s) = &video_socket {
                                    let _ = s.send(&wire);
                                }
                            }
                            if !audio_flowing.load(Ordering::SeqCst) {
                                if let Some(s) = &audio_socket {
                                    let _ = s.send(&wire);
                                }
                            }
                        }
                    }
                    // 媒体残留清理
                    {
                        audio_rx.lock().sweep(MEDIA_SWEEP);
                        video_rx.lock().sweep(MEDIA_SWEEP);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }));
        }
    }

    /// 启动一个媒体端口 reader：解析 33B 头 → client_id 校验 → 重装 → 事件。
    ///
    /// 媒体 socket 错误不判死会话（判死只在 ctrl 路径）；媒体断流由上层
    /// 看门狗（viewer 首帧超时 / daemon 音频静默）兜底。
    fn spawn_media_reader(
        socket: UdpSocket,
        is_video: bool,
        client_id: Arc<AtomicU32>,
        flowing: Arc<AtomicBool>,
        rx: Arc<Mutex<MediaReassembler>>,
        ev_tx: tokio::sync::mpsc::UnboundedSender<Event>,
        closed: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf = vec![0u8; MEDIA_HDR_LEN + 1400];
            // 非致命 socket 错误的重试计数与日志限频（收到数据后清零）
            let mut err_count = 0u64;
            let mut last_err_log = Instant::now();
            while !closed.load(Ordering::SeqCst) {
                match socket.recv(&mut buf) {
                    Ok(n) => {
                        err_count = 0;
                        let Some(f) = decode_media_datagram(&buf[..n]) else {
                            continue; // 非法/截断 datagram
                        };
                        let cid = client_id.load(Ordering::SeqCst);
                        if cid == 0 || f.client_id != cid {
                            continue; // 未握手 / 非本会话
                        }
                        if f.frag_total == MEDIA_OPEN_TOTAL {
                            continue; // OPEN 只出现在 PC→phone 方向，防御
                        }
                        flowing.store(true, Ordering::SeqCst);
                        if let Some(m) = rx.lock().push(&f) {
                            let evt = if is_video {
                                Event::Video(m)
                            } else {
                                Event::Audio(m)
                            };
                            let _ = ev_tx.send(evt);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                    // 握手期 server 未绑媒体端口 → ICMP 不可达（Windows 表现为
                    // ConnectionReset），与 ctrl socket 同样忽略
                    Err(e)
                        if e.kind() == std::io::ErrorKind::ConnectionReset
                            || e.kind() == std::io::ErrorKind::ConnectionRefused => {}
                    // 其它错误（网卡重配 / 路由切换 / WSAENETRESET 等）不再直接退出
                    // reader：退出会让该流永久断掉，而 PC 侧只剩「没声音 + 缓冲数字
                    // 不动」，ctrl 链路照常 → 看起来会话还活着。抖动过去后 socket 可恢复。
                    Err(e) => {
                        if err_count == 0 || last_err_log.elapsed() >= Duration::from_secs(5) {
                            last_err_log = Instant::now();
                            let stream = if is_video { "video" } else { "audio" };
                            eprintln!("[kulua] {stream} recv error: {e}（继续重试）");
                        }
                        err_count += 1;
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        })
    }

    /// 分发一个收到的 Frame，返回要发出的事件列表（可能为空）。
    ///
    /// 媒体 DATA 已迁到独立端口（媒体 reader 线程处理），ctrl 端口只收控制流。
    fn dispatch(
        frame: &Frame,
        state: &Arc<Mutex<State>>,
        ctrl_tx: &Arc<Mutex<ReliableSender>>,
        ctrl_rx: &Arc<Mutex<ReliableReceiver>>,
        client_id: &Arc<AtomicU32>,
    ) -> Vec<Event> {
        match frame::Type::try_from(frame.r#type) {
            Ok(frame::Type::Ack) => {
                if let Some(frame::Payload::AckSeq(ack)) = &frame.payload {
                    ctrl_tx.lock().on_ack(*ack);
                }
                Vec::new()
            }
            Ok(frame::Type::HelloAck) => {
                if let Some(frame::Payload::HelloAck(ack)) = &frame.payload {
                    // client_id 供媒体 reader 校验数据报归属（媒体头携带）
                    client_id.store(ack.client_id, Ordering::SeqCst);
                    let mut st = state.lock();
                    st.connected = true;
                    st.hello_ack = Some(ack.clone());
                    vec![Event::Connected]
                } else {
                    Vec::new()
                }
            }
            Ok(frame::Type::Data) => match frame::Stream::try_from(frame.stream) {
                Ok(frame::Stream::Ctrl) => {
                    let mut rx = ctrl_rx.lock();
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
                Err(_) => Vec::new(), // 未知流：忽略（兼容误发）
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
        let frames = self.ctrl_tx.lock().send_ctrl_bytes(id, &bytes);
        for f in frames {
            let mut wire = Vec::with_capacity(f.encoded_len() + 8);
            prost::Message::encode(&f, &mut wire).map_err(|e| format!("encode: {e}"))?;
            self.socket
                .send(&wire)
                .map_err(|e| format!("udp send: {e}"))?;
        }
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
        self.state.lock().hello_ack.clone()
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
        for h in self.media_readers.drain(..) {
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
        payload: None,
    };
    let mut wire = Vec::with_capacity(16);
    prost::Message::encode(&f, &mut wire).map_err(|e| format!("encode bye: {e}"))?;
    Ok(wire)
}

/// 构造媒体端点 OPEN 注册包（33B 头，frag_total=OPEN 标记，无负载）。
///
/// phone 收到任意合法头数据报即把源地址登记为媒体发送目标；编码器尚未
/// 推流时靠本包周期重发完成注册。
///
/// WHY 每个会话都开媒体端口并注册端点：音频可在会话中途热开启，端点必须先
/// 就位（否则开启后 phone 无处发送，且它不会再从 PC 收到任何数据报来登记）。
fn encode_open(client_id: u32) -> Vec<u8> {
    let mut wire = Vec::with_capacity(MEDIA_HDR_LEN);
    encode_media_datagram(
        &MediaFragment {
            client_id,
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
    wire
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

    /// 音频开关 + 编码 + 代次一起下发（唯一音频配置命令）。
    ///
    /// `codec` 是索引（0=raw 1=opus 2=aac 3=flac）；关闭靠 `enabled=false` 表达
    /// （codec=0 表示 raw，不能当"关闭"用）。
    pub fn set_audio(enabled: bool, codec: u32, revision: u64) -> CtrlMsg {
        CtrlMsg {
            msg: Some(ctrl_msg::Msg::SetAudio(crate::generated::SetAudio {
                enabled,
                codec,
                revision,
            })),
        }
    }
}
