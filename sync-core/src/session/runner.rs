use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, Ordering};
use std::time::Duration;

use kulua_proto::codec::AssembledMedia;
use kulua_proto::generated::{HelloAck, ctrl_msg};
use kulua_proto::session::{Event, UdpSession};

use crate::adb_cmd::AdbOps;
use crate::audio_player::AUDIO_BUFFER_MAX_MS;
use crate::notification::{self, NotifInfo};
use crate::scrcpy;
use crate::types::Device;

use super::handle::{SESSION_STATE_CONNECTING, SESSION_STATE_FAILED, SESSION_STATE_RUNNING};
use super::proto::audio_codec_name;

/// 剪贴板主循环退出原因。
enum SessionExit {
    Stop,
}

/// 音频任务输入事件。
enum AudioEvent {
    /// 音频编码切换（HelloAck 初始 codec / SetAudioCodec 后的 AudioReady）。
    Codec(u32),
    /// AAC AudioSpecificConfig / FLAC STREAMINFO（可靠 MediaConfig）。
    Config(Vec<u8>),
    /// 一帧编码音频。
    Frame(AssembledMedia),
}

/// 单个设备的完整 session（UDP 直连版）。
///
/// 部署 kulua-server（phone 绑定网络 UDP 端口）→ 局域网直连（IP:port）→
/// 双向剪贴板 I/O + 通知轮询 + 音频播放（复用 same UDP 会话的多路流）。
pub struct Session {
    adb: Arc<dyn AdbOps>,
    device: Device,
    jar_path: String,
    port: u16,
    clip_sub: tokio::sync::broadcast::Receiver<String>,
    phone_clip_tx: tokio::sync::mpsc::Sender<String>,
    notif_tx: tokio::sync::mpsc::Sender<NotifInfo>,
    stop_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    clipboard_enabled: Arc<AtomicBool>,
    notification_enabled: Arc<AtomicBool>,
    audio_enabled: Arc<AtomicBool>,
    volume: Arc<AtomicU16>,
    session_state: Arc<AtomicU8>,
    audio_latency: Arc<AtomicU64>,
    audio_codec: Arc<AtomicU8>,
    audio_restart_rx: Option<tokio::sync::mpsc::Receiver<()>>,
    device_name_tx: Option<tokio::sync::mpsc::Sender<(String, String)>>,
    scrcpy_params: crate::settings::ScrcpyParams,
}
impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        adb: Arc<dyn AdbOps>,
        device: Device,
        jar_path: String,
        port: u16,
        clip_sub: tokio::sync::broadcast::Receiver<String>,
        phone_clip_tx: tokio::sync::mpsc::Sender<String>,
        notif_tx: tokio::sync::mpsc::Sender<NotifInfo>,
        stop_rx: tokio::sync::oneshot::Receiver<()>,
        clipboard_enabled: Arc<AtomicBool>,
        notification_enabled: Arc<AtomicBool>,
        audio_enabled: Arc<AtomicBool>,
        volume: Arc<AtomicU16>,
        device_name_tx: tokio::sync::mpsc::Sender<(String, String)>,
        session_state: Arc<AtomicU8>,
        audio_latency: Arc<AtomicU64>,
        audio_codec: Arc<AtomicU8>,
        audio_restart_rx: tokio::sync::mpsc::Receiver<()>,
        scrcpy_params: crate::settings::ScrcpyParams,
    ) -> Self {
        session_state.store(SESSION_STATE_CONNECTING, Ordering::SeqCst);
        Self {
            adb,
            device,
            jar_path,
            port,
            clip_sub,
            phone_clip_tx,
            notif_tx,
            stop_rx: Some(stop_rx),
            clipboard_enabled,
            notification_enabled,
            audio_enabled,
            volume,
            session_state,
            audio_latency,
            audio_codec,
            audio_restart_rx: Some(audio_restart_rx),
            device_name_tx: Some(device_name_tx),
            scrcpy_params,
        }
    }

    /// 运行 session 主循环：部署 kulua-server → UDP 直连 → 双向剪贴板 + 音频。
    pub async fn run(&mut self) {
        let timing = std::env::var("KULUA_TIMING")
            .map(|v| v == "1")
            .unwrap_or(false);
        let t0 = std::time::Instant::now();
        let mark = |label: &str| {
            if timing {
                eprintln!("[timing] {}: {}ms", label, t0.elapsed().as_millis());
            }
        };

        let mut stop_rx = self.stop_rx.take().expect("run can only be called once");
        let audio_enabled = self.audio_enabled.load(Ordering::SeqCst);
        let port = self.port;
        let serial = self.device.serial.clone();

        // 1. 部署 kulua-server（phone 绑定网络 UDP 端口）
        let adb = self.adb.clone();
        let device = self.device.clone();
        let jar_path = self.jar_path.clone();
        let scrcpy_params = self.scrcpy_params.clone();
        let server = tokio::task::spawn_blocking(move || {
            scrcpy::ScrcpyServer::deploy_scrcpy(
                adb.as_ref(),
                &device,
                &jar_path,
                port,
                scrcpy_params,
            )
        })
        .await;
        mark("deploy（adb push/spawn app_process）");

        let mut server = match server {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                eprintln!("scrcpy deploy failed for {}: {:?}", self.device.serial, e);
                self.session_state
                    .store(SESSION_STATE_FAILED, Ordering::SeqCst);
                return;
            }
            Err(e) => {
                eprintln!("spawn_blocking panic: {}", e);
                self.session_state
                    .store(SESSION_STATE_FAILED, Ordering::SeqCst);
                return;
            }
        };
        println!("kulua-server launching on {} (udp port={})", serial, port);

        // 2. 设备名（无 adb forward 后 server 不再发元数据；getprop 获取）
        if self.device.name.is_empty() {
            let adb = self.adb.clone();
            let serial_for_closure = serial.clone();
            let tx = self.device_name_tx.take();
            if tx.is_some() {
                let result = tokio::task::spawn_blocking(move || {
                    adb.run(&[
                        "-s",
                        &serial_for_closure,
                        "shell",
                        "getprop",
                        "ro.product.model",
                    ])
                })
                .await;
                if let Ok(Ok(output)) = result {
                    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !name.is_empty() {
                        println!("Device name: {} ({})", name, serial);
                        let _ = tx.unwrap().send((serial.clone(), name)).await;
                    }
                }
            }
        }
        mark("设备名 getprop");

        // 3. 解析 phone 直连地址（serial IP 优先，USB 走 ip route）
        let addr_string = match scrcpy::resolve_device_ip(self.adb.as_ref(), &serial, port) {
            Some(a) => a,
            None => {
                eprintln!(
                    "[{}] 无法解析设备 UDP 直连地址（需要 Wi-Fi；serial/IP 均无），判死",
                    serial
                );
                self.session_state
                    .store(SESSION_STATE_FAILED, Ordering::SeqCst);
                server.stop(self.adb.as_ref());
                return;
            }
        };
        let peer: std::net::SocketAddr = match addr_string.parse() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[{}] 非法直连地址 {addr_string}: {e}", serial);
                self.session_state
                    .store(SESSION_STATE_FAILED, Ordering::SeqCst);
                server.stop(self.adb.as_ref());
                return;
            }
        };
        let scid = scrcpy::scid_hex(port);

        // 4. UDP 握手（含冷启动重试，同旧 TCP 重试语义）
        let want_audio = audio_enabled;
        let peer2 = peer;
        let scid2 = scid.clone();
        let udp =
            tokio::task::spawn_blocking(move || UdpSession::connect(peer2, &scid2, want_audio))
                .await;
        let mut udp = match udp {
            Ok(Ok(s)) => s,
            _ => {
                eprintln!("[{}] UDP 直连 {peer} 失败（握手超时）", serial);
                self.session_state
                    .store(SESSION_STATE_FAILED, Ordering::SeqCst);
                server.stop(self.adb.as_ref());
                return;
            }
        };
        mark("UDP 握手（HELLO/HELLO_ACK）");
        println!("[{}] UDP 直连已就绪 @ {peer}", serial);

        // HELLO_ACK 携带初始 codec 状态
        let hello_ack: HelloAck = udp.hello_ack().unwrap_or_default();
        let initial_codec_id = hello_ack.audio_codec;
        if want_audio {
            if initial_codec_id == 0 {
                println!("Audio stream disabled by device, continuing without audio");
            } else {
                println!(
                    "Audio codec: {} (0x{:08x}) from {}",
                    audio_codec_name(initial_codec_id),
                    initial_codec_id,
                    serial
                );
            }
        }
        mark("codec 初始信息");

        // 启动通知轮询
        let notif_stop = Arc::new(AtomicBool::new(false));
        notification::spawn_notification_poller_tokio(
            self.adb.clone(),
            self.device.clone(),
            self.notif_tx.clone(),
            notif_stop.clone(),
            self.notification_enabled.clone(),
        );

        // 主循环：control（可靠双向）+ audio（尽力而为）+ 心跳 + 音频热切换
        let (audio_tx, audio_rx) = tokio::sync::mpsc::channel::<AudioEvent>(64);
        let audio_task = spawn_audio_task(
            audio_rx,
            serial.clone(),
            self.volume.clone(),
            self.audio_latency.clone(),
        );
        if want_audio && initial_codec_id != 0 {
            let _ = audio_tx.send(AudioEvent::Codec(initial_codec_id)).await;
        }

        // 握手全部完成 → running
        self.session_state
            .store(SESSION_STATE_RUNNING, Ordering::SeqCst);

        let mut audio_restart_rx = self.audio_restart_rx.take().expect("restart rx");
        let audio_codec = self.audio_codec.clone();
        let clipboard_enabled = self.clipboard_enabled.clone();
        let phone_clip_tx = self.phone_clip_tx.clone();

        let exit_reason = loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => {
                    break SessionExit::Stop;
                }
                _ = audio_restart_rx.recv() => {
                    // 音频编码热切换：Core 改 audio_codec 原子并触发本信号。
                    // 发送 SetAudioCodec，phone 重起捕获后回 AudioReady（主循环会转发给 audio_task）。
                    if audio_enabled {
                        let codec_byte = audio_codec.load(Ordering::SeqCst);
                        println!("[audio] codec hot-swap request (index {})", codec_byte);
                        if udp.send_ctrl(&kulua_proto::session::msgs::set_audio_codec(codec_byte as u32)).is_ok() {
                            continue;
                        } else {
                            eprintln!("[audio] SetAudioCodec 发送失败，会话终止");
                            break SessionExit::Stop;
                        }
                    } else {
                        continue;
                    }
                }
                evt = udp.recv() => {
                    match evt {
                        Some(Event::Control(msg)) => match msg.msg {
                            Some(ctrl_msg::Msg::ClipboardChanged(c)) => {
                                if clipboard_enabled.load(Ordering::SeqCst) {
                                    println!("Phone clipboard from {}: {}", serial, c.text);
                                    if phone_clip_tx.send(c.text).await.is_err() {
                                        break SessionExit::Stop;
                                    }
                                }
                            }
                            Some(ctrl_msg::Msg::MediaConfig(cfg)) => {
                                // cfg.stream: 0=CTRL 1=AUDIO 2=VIDEO（AUDIO=1）
                                if want_audio && cfg.stream == 1 {
                                    let _ = audio_tx.send(AudioEvent::Config(cfg.data)).await;
                                }
                            }
                            Some(ctrl_msg::Msg::AudioReady(ar)) => {
                                if want_audio && ar.codec_id != 0 {
                                    println!("Audio codec: {} (0x{:08x})", audio_codec_name(ar.codec_id), ar.codec_id);
                                    let _ = audio_tx.send(AudioEvent::Codec(ar.codec_id)).await;
                                }
                            }
                            _ => {}
                        },
                        Some(Event::Audio(frame)) => {
                            let _ = audio_tx.send(AudioEvent::Frame(frame)).await;
                        }
                        Some(Event::Video(_)) => {}
                        Some(Event::Error(e)) => {
                            eprintln!("[{}] UDP 会话错误: {}", serial, e);
                            break SessionExit::Stop;
                        }
                        Some(Event::Closed) | None => {
                            eprintln!("[{}] UDP 会话已关闭（对端 BYE/消失）", serial);
                            break SessionExit::Stop;
                        }
                        Some(Event::Connected) => {}
                    }
                }
                result = self.clip_sub.recv() => {
                    match result {
                        Ok(text) if clipboard_enabled.load(Ordering::SeqCst) => {
                            if let Err(e) = udp.send_ctrl(&kulua_proto::session::msgs::set_clipboard(0, false, &text)) {
                                eprintln!("[{}] set_clipboard 发送失败: {}，终止", serial, e);
                                break SessionExit::Stop;
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break SessionExit::Stop;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("[{}] clip_sub lagged by {}", serial, n);
                        }
                    }
                }
            }
        };

        // ── 清理 ──
        audio_task.abort();
        notif_stop.store(true, Ordering::SeqCst);

        match exit_reason {
            SessionExit::Stop => {
                println!("[{}] 关闭 UDP 会话（BYE）", serial);
                udp.close();
            }
        }

        // 强制清理远端进程（kill_by_scid，可靠收尾）
        server.stop(self.adb.as_ref());
        println!("Session {} cleaned up", serial);
    }
}

/// 启动音频播放任务（读通道，config/codec/frame 事件驱动）。
///
/// 与旧实现逻辑一致：Codec 切换重置解码器；Config（可靠 MediaConfig）先于
/// 首帧捕获；opus/aac/flac 走各自解码器，未知 codec 只读丢弃。
fn spawn_audio_task(
    mut rx: tokio::sync::mpsc::Receiver<AudioEvent>,
    serial: String,
    volume: Arc<AtomicU16>,
    latency: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut codec_id: u32 = 0;
        let mut codec_config: Option<Vec<u8>> = None;
        let mut player: Option<crate::audio_player::AudioPlayer> = None;
        let mut last_vol = volume.load(Ordering::Relaxed);
        let mut log_timer = tokio::time::Instant::now();

        while let Some(event) = rx.recv().await {
            match event {
                AudioEvent::Codec(cid) => {
                    codec_id = cid;
                    player = None;
                    if let Some(ok) = crate::audio_player::AudioCodec::from_codec_id(cid) {
                        if ok.name() != "OPUS" && ok.name() != "RAW" {
                            // 需要 config 的 codec：保留最近 config（新 config 即将到来或已在）
                            // 保持不变即可；旧 config 会在首个新配置帧到达时被替换。
                        } else {
                            codec_config = None;
                        }
                    } else {
                        codec_config = None;
                    }
                }
                AudioEvent::Config(data) => {
                    codec_config = Some(data);
                }
                AudioEvent::Frame(frame) => {
                    // 会话元数据帧（预留）忽略
                    if (frame.flags >> 2) & 1 != 0 {
                        continue;
                    }
                    // 帧内 config 标志（防御：某些实现可能仍内联 config）
                    if frame.flags & 1 != 0 {
                        codec_config = Some(frame.data.clone());
                        continue;
                    }
                    // 首个正常音频帧 → 惰性创建解码器（此时 config 已就绪，如需要）
                    if player.is_none() {
                        let Some(codec) = crate::audio_player::AudioCodec::from_codec_id(codec_id)
                        else {
                            eprintln!("[audio] unknown codec id 0x{:08x} on {serial}", codec_id);
                            continue;
                        };
                        let p = match crate::audio_player::AudioPlayer::new(
                            codec,
                            codec_config.as_deref(),
                        ) {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!(
                                    "[audio] failed to init {} player on {serial}: {e}",
                                    codec.name()
                                );
                                continue;
                            }
                        };
                        p.set_volume(volume.load(Ordering::Relaxed) as f32 / 100.0);
                        player = Some(p);
                    }
                    let player = player.as_mut().unwrap();

                    // 同步音量
                    let cur = volume.load(Ordering::Relaxed);
                    if cur != last_vol {
                        player.set_volume(cur as f32 / 100.0);
                        last_vol = cur;
                    }

                    // 积压超阈值 → 丢帧清空
                    if player.buffer_ms() > AUDIO_BUFFER_MAX_MS {
                        eprintln!(
                            "[audio] {serial} buffer {}ms exceeded limit, flushing backlog",
                            player.buffer_ms()
                        );
                        player.clear();
                    }
                    if let Err(e) = player.feed_frame(&frame.data) {
                        eprintln!("[audio] decode error on {serial}: {e}");
                    } else {
                        latency.store(player.buffer_ms(), Ordering::Relaxed);
                    }

                    if log_timer.elapsed() >= Duration::from_secs(5) {
                        eprintln!("[audio] {serial} buffer: {} ms", player.buffer_ms());
                        log_timer = tokio::time::Instant::now();
                    }
                }
            }
        }
    })
}
