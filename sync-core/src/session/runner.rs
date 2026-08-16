use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::adb_cmd::AdbOps;
use crate::notification::{self, NotifInfo};
use crate::scrcpy;
use crate::types::Device;

use super::handle::{SESSION_STATE_CONNECTING, SESSION_STATE_FAILED, SESSION_STATE_RUNNING};
use super::proto::{audio_codec_name, build_clipboard_frame, read_device_message};
use crate::audio_player::AUDIO_BUFFER_MAX_MS;

/// 单个设备的完整 session。
///
/// 包含部署 scrcpy → TCP 连接 → 双向剪贴板 I/O + 通知轮询 + 音频播放的完整生命周期。
/// 配置选项（`clipboard_enabled` / `notification_enabled` / `audio_enabled`）是共享原子标志，
/// Core 可通过 Handle 随时调整。音频切换需要重启 session（redeploy scrcpy）。
pub struct Session {
    adb: Arc<dyn AdbOps>,
    device: Device,
    jar_path: String,
    port: u16,
    clip_sub: broadcast::Receiver<String>,
    phone_clip_tx: mpsc::Sender<String>,
    notif_tx: mpsc::Sender<NotifInfo>,
    stop_rx: Option<oneshot::Receiver<()>>,
    clipboard_enabled: Arc<AtomicBool>,
    notification_enabled: Arc<AtomicBool>,
    audio_enabled: Arc<AtomicBool>,
    /// 音量百分比（0-100）
    volume: Arc<AtomicU16>,
    /// session 生命周期状态（与 Handle 共享，阶段边界写入）
    session_state: Arc<AtomicU8>,
    /// 音频缓冲延迟（ms），audio_task 写，Core 读推 UI
    audio_latency: Arc<AtomicU64>,
    device_name_tx: Option<mpsc::Sender<(String, String)>>,
    /// scrcpy 编码参数（部署 server 时使用，配置变更后新会话生效）
    scrcpy_params: crate::settings::ScrcpyParams,
}
impl Session {
    pub fn new(
        adb: Arc<dyn AdbOps>,
        device: Device,
        jar_path: String,
        port: u16,
        clip_sub: broadcast::Receiver<String>,
        phone_clip_tx: mpsc::Sender<String>,
        notif_tx: mpsc::Sender<NotifInfo>,
        stop_rx: oneshot::Receiver<()>,
        clipboard_enabled: Arc<AtomicBool>,
        notification_enabled: Arc<AtomicBool>,
        audio_enabled: Arc<AtomicBool>,
        volume: Arc<AtomicU16>,
        device_name_tx: mpsc::Sender<(String, String)>,
        session_state: Arc<AtomicU8>,
        audio_latency: Arc<AtomicU64>,
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
            device_name_tx: Some(device_name_tx),
            scrcpy_params,
        }
    }

    /// 运行 session 主循环：部署 scrcpy → TCP 连接 → 双向剪贴板 I/O + 音频帧 + 通知轮询。
    pub async fn run(&mut self) {
        let mut stop_rx = self.stop_rx.take().expect("run can only be called once");
        let port = self.port;
        let audio_enabled = self.audio_enabled.load(Ordering::SeqCst);

        // 1. 部署 scrcpy-server（带音频开关 + 编码参数）
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
                audio_enabled,
                scrcpy_params,
            )
        })
        .await;

        let mut server = match server {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                eprintln!("scrcpy deploy failed for {}: {:?}", self.device.serial, e);
                // 判死：不再降级 notification-only（见 docs/session-state-design.md）
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
        println!(
            "scrcpy-server alive on {} (port {}, audio={})",
            self.device.serial, self.port, audio_enabled
        );

        if !audio_enabled {
            // 音频未启用：单连接 control-only
            self.run_control_only(&mut server, stop_rx).await;
            println!("Session {} cleaned up", self.device.serial);
            return;
        }

        // ── 音频启用：双连接架构 ──

        let audio_stream = self
            .connect_socket(port, &mut server, &mut stop_rx, true)
            .await;
        if audio_stream.is_none() {
            return;
        }
        let mut audio_stream = audio_stream.unwrap();
        // 启用 TCP_NODELAY 及时检测断连
        let _ = audio_stream.set_nodelay(true);
        // 先连 control socket，让 server 的 open() 能 accept 完所有 socket 后返回
        let control_stream = self
            .connect_socket(port, &mut server, &mut stop_rx, false)
            .await;
        if control_stream.is_none() {
            return;
        }
        let control_stream = control_stream.unwrap();
        // 控制通道禁用 Nagle（官方默认行为）
        let _ = control_stream.set_nodelay(true);

        let mut name_buf = [0u8; 64];
        // 读设备名称（64 字节，scrcpy 协议设备信息交换）
        if audio_stream.read_exact(&mut name_buf).await.is_ok() {
            let name_end = name_buf.iter().position(|&b| b == 0).unwrap_or(64);
            let device_name = String::from_utf8_lossy(&name_buf[..name_end]).to_string();
            if !device_name.is_empty() {
                println!("Device name: {} ({})", device_name, self.device.serial);
                self.device.name = device_name.clone();
                if let Some(tx) = self.device_name_tx.take() {
                    let _ = tx.send((self.device.serial.clone(), device_name)).await;
                }
            }
        }

        // 读 codec ID header（4 字节大端序）
        let mut codec_buf = [0u8; 4];
        if audio_stream.read_exact(&mut codec_buf).await.is_err() {
            eprintln!(
                "failed to read audio codec header from {}",
                self.device.serial
            );
            self.session_state
                .store(SESSION_STATE_FAILED, Ordering::SeqCst);
            server.stop(self.adb.as_ref());
            return;
        }
        let codec_id = u32::from_be_bytes(codec_buf);
        if codec_id == 0 {
            println!("Audio stream disabled by device, continuing without audio");
        } else if codec_id == 1 {
            eprintln!("Audio stream configuration error on {}", self.device.serial);
            self.session_state
                .store(SESSION_STATE_FAILED, Ordering::SeqCst);
            server.stop(self.adb.as_ref());
            return;
        } else {
            let codec_name = audio_codec_name(codec_id);
            println!(
                "Audio codec: {} (0x{:08x}) from {}",
                codec_name, codec_id, self.device.serial
            );
        }
        // 握手完成（双 socket + 设备名 + codec header）→ running
        self.session_state
            .store(SESSION_STATE_RUNNING, Ordering::SeqCst);
        // 4. 启动通知轮询
        let notif_stop = Arc::new(AtomicBool::new(false));
        notification::spawn_notification_poller_tokio(
            self.adb.clone(),
            self.device.clone(),
            self.notif_tx.clone(),
            notif_stop.clone(),
            self.notification_enabled.clone(),
        );

        let (mut audio_reader, audio_writer) = tokio::io::split(audio_stream);
        let (mut control_reader, mut control_writer) = tokio::io::split(control_stream);

        // 6. 音频读取任务（独立于控制通道）
        let audio_task = if codec_id == 0 {
            // 设备禁用了音频流，无需读取
            None
        } else if crate::audio_player::AudioCodec::from_codec_id(codec_id).is_none() {
            // 未知 codec → 只读取丢弃（无法播放）
            Some(tokio::spawn(async move {
                let mut header = [0u8; 12];
                while audio_reader.read_exact(&mut header).await.is_ok() {
                    let frame_size =
                        u32::from_be_bytes(<[u8; 4]>::try_from(&header[8..12]).unwrap()) as usize;
                    if frame_size == 0 || frame_size > 10_000_000 {
                        break;
                    }
                    let mut _frame = vec![0u8; frame_size];
                    if audio_reader.read_exact(&mut _frame).await.is_err() {
                        break;
                    }
                }
            }))
        } else {
            // 已知 codec → 解码播放
            let codec = crate::audio_player::AudioCodec::from_codec_id(codec_id).unwrap();
            let serial = self.device.serial.clone();
            let volume = self.volume.clone();
            let latency = self.audio_latency.clone();
            Some(tokio::spawn(async move {
                // codec config 包（bit 62）：AAC 的 AudioSpecificConfig / FLAC 的 STREAMINFO，
                // 必须先于首帧捕获；OPUS/RAW 无配置包
                let mut codec_config: Option<Vec<u8>> = None;
                let mut player: Option<crate::audio_player::AudioPlayer> = None;
                let mut last_vol = volume.load(Ordering::Relaxed);
                let mut log_timer = tokio::time::Instant::now();
                loop {
                    // 12-byte frame header: 8B PTS/flags + 4B size (big-endian)
                    let mut header = [0u8; 12];
                    if audio_reader.read_exact(&mut header).await.is_err() {
                        break;
                    }

                    let pts_raw = u64::from_be_bytes(<[u8; 8]>::try_from(&header[..8]).unwrap());
                    let frame_size =
                        u32::from_be_bytes(<[u8; 4]>::try_from(&header[8..12]).unwrap()) as usize;

                    // 检查 session 元数据标记（bit 63）
                    if (pts_raw >> 63) & 1 != 0 {
                        continue;
                    }

                    // 安全检查
                    if frame_size == 0 || frame_size > 10_000_000 {
                        break;
                    }

                    let mut frame_data = vec![0u8; frame_size];
                    if audio_reader.read_exact(&mut frame_data).await.is_err() {
                        break;
                    }

                    // codec config 包（bit 62）：仅在解码器创建前捕获一次
                    if (pts_raw >> 62) & 1 != 0 {
                        if player.is_none() {
                            codec_config = Some(frame_data);
                        }
                        continue;
                    }

                    // 首个正常音频帧 → 惰性创建解码器（此时 config 已就绪）
                    if player.is_none() {
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
                                return;
                            }
                        };
                        // 初始音量
                        p.set_volume(volume.load(Ordering::Relaxed) as f32 / 100.0);
                        player = Some(p);
                    }
                    let player = player.as_mut().unwrap();

                    // 同步音量变更
                    let cur = volume.load(Ordering::Relaxed);
                    if cur != last_vol {
                        player.set_volume(cur as f32 / 100.0);
                        last_vol = cur;
                    }

                    // 正常音频帧 → 解码播放
                    // 积压超过阈值 → 丢帧清空，防止延迟永久累积（rodio 队列无上限）
                    if player.buffer_ms() > AUDIO_BUFFER_MAX_MS {
                        eprintln!(
                            "[audio] {serial} buffer {}ms exceeded limit, flushing backlog",
                            player.buffer_ms()
                        );
                        player.clear();
                    }
                    if let Err(e) = player.feed_frame(&frame_data) {
                        eprintln!("[audio] {} decode error on {serial}: {e}", codec.name());
                    } else {
                        // 上报播放队列积压（缓冲延迟，ms）
                        latency.store(player.buffer_ms(), Ordering::Relaxed);
                    }

                    // 诊断打点：每 5s 打印一次缓冲延迟
                    if log_timer.elapsed() >= Duration::from_secs(5) {
                        eprintln!("[audio] {serial} buffer: {} ms", player.buffer_ms());
                        log_timer = tokio::time::Instant::now();
                    }
                }
            }))
        };

        // 7. 双向剪贴板 I/O（control channel）
        self.run_clipboard_io(
            &mut control_reader,
            &mut control_writer,
            stop_rx,
            self.device.serial.clone(),
        )
        .await;

        // 8. 清理（模仿官方关闭流程）
        //    run_clipboard_io 返回 → control_reader/writer 已释放 → control socket 关闭
        //    → server 侧 ControlChannel.recv() 收到 IOException

        // (a) shutdown audio socket → server 侧 Streamer.writePacket() 收到 IO 错误
        drop(audio_writer); // 释放我们的 WriteHalf 引用
        if let Some(task) = audio_task {
            task.abort(); // 释放 ReadHalf → Arc 归零 → socket 关闭
        }

        // (b) 停通知轮询
        notif_stop.store(true, Ordering::SeqCst);

        // (c) 1s 看门狗：给 server 时间检测 socket 断开 → 走 Java finally → 进程退出
        let mut exited = server.try_wait();
        if exited.is_none() {
            // 等 1s，每 100ms 检查一次
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                exited = server.try_wait();
                if exited.is_some() {
                    break;
                }
            }
        }

        // (d) 看门狗超时 → force kill（同官方 watchdog 超时后 kill(SIGKILL)）
        if exited.is_none() {
            eprintln!(
                "server {} did not exit after socket shutdown, force killing",
                self.device.serial
            );
            server.stop(self.adb.as_ref());
        }

        println!("Session {} cleaned up", self.device.serial);
    }

    /// 连接到 scrcpy ADB forward 端口。
    ///
    /// `read_dummy=true` 时连上后读一个 dummy byte（第一个 socket），
    /// 读失败说明 server 未就绪，重试整个流程。
    async fn connect_socket(
        &self,
        port: u16,
        server: &mut scrcpy::ScrcpyServer,
        stop_rx: &mut oneshot::Receiver<()>,
        read_dummy: bool,
    ) -> Option<TcpStream> {
        // 最多重试 20 次（500ms 间隔 = 10s），超时判死写 failed（docs/session-state-design.md §3.3）
        let mut attempts: u8 = 0;
        loop {
            match TcpStream::connect(format!("127.0.0.1:{}", port)).await {
                Ok(mut s) => {
                    if read_dummy {
                        let mut dummy = [0u8; 1];
                        // 异步读 dummy byte，server 在 DesktopConnection.accept() 后发送
                        // 用 select! 同时监听 stop 信号，避免死等
                        let read_result = tokio::select! {
                            _ = &mut *stop_rx => {
                                server.stop(self.adb.as_ref());
                                return None;
                            }
                            r = s.read_exact(&mut dummy) => r,
                        };
                        if read_result.is_err() {
                            // 读失败（极少见：server 在写完前就挂了），drop 重试
                            drop(s);
                        } else {
                            return Some(s);
                        }
                    } else {
                        return Some(s);
                    }
                }
                Err(_) => {}
            }
            attempts += 1;
            if attempts >= 20 {
                eprintln!(
                    "[{}] connect timeout after {} attempts (10s), giving up",
                    self.device.serial, attempts
                );
                self.session_state
                    .store(SESSION_STATE_FAILED, Ordering::SeqCst);
                server.stop(self.adb.as_ref());
                return None;
            }
            // 连接失败或 dummy 读失败，等 500ms 或 stop 信号
            tokio::select! {
                _ = &mut *stop_rx => {
                    server.stop(self.adb.as_ref());
                    return None;
                }
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    }

    /// 控制通道的剪贴板双向 I/O
    async fn run_clipboard_io(
        &mut self,
        control_reader: &mut (impl AsyncReadExt + Unpin),
        control_writer: &mut (impl AsyncWriteExt + Unpin),
        mut stop_rx: oneshot::Receiver<()>,
        serial: String,
    ) {
        eprintln!("[{}] run_clipboard_io ENTER", serial);
        let clipboard_enabled = self.clipboard_enabled.clone();
        let phone_clip_tx = self.phone_clip_tx.clone();

        loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => {
                    eprintln!("[{}] BREAK: stop_rx fired", serial);
                    break;
                }
                result = self.clip_sub.recv() => {
                    match result {
                        Ok(text) if clipboard_enabled.load(Ordering::SeqCst) => {
                            let msg = build_clipboard_frame(&text);
                            if control_writer.write_all(&msg).await.is_err() {
                                eprintln!("[{}] BREAK: write_all error", serial);
                                break;
                            }
                        }
                        Ok(_) => {
                            eprintln!("[{}] clip_sub.recv Ok(_) clipboard disabled, continue", serial);
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            eprintln!("[{}] BREAK: clip_sub Closed", serial);
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("[{}] clip_sub lagged by {}", serial, n);
                        }
                    }
                }
                result = read_device_message(control_reader) => {
                    match result {
                        Ok(Some(text)) => {
                            if clipboard_enabled.load(Ordering::SeqCst) {
                                println!("Phone clipboard from {}: {}", serial, text);
                                if phone_clip_tx.send(text).await.is_err() {
                                    eprintln!("[{}] BREAK: phone_clip_tx send error", serial);
                                    break;
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(()) => {
                            eprintln!("[{}] BREAK: read_device_message error", serial);
                            break;
                        }
                    }
                }
            }
        }
        eprintln!("[{}] run_clipboard_io EXIT", serial);
    }

    /// 音频未启用时：单连接 control-only
    async fn run_control_only(
        &mut self,
        server: &mut scrcpy::ScrcpyServer,
        mut stop_rx: oneshot::Receiver<()>,
    ) {
        // 只需要 1 个 socket（control）
        let mut stream = match self
            .connect_socket(self.port, server, &mut stop_rx, true)
            .await
        {
            Some(s) => s,
            None => return,
        };
        // 握手成功（dummy byte 就绪）→ running
        self.session_state
            .store(SESSION_STATE_RUNNING, Ordering::SeqCst);

        // 读设备名
        let mut name_buf = [0u8; 64];
        if stream.read_exact(&mut name_buf).await.is_ok() {
            let name_end = name_buf.iter().position(|&b| b == 0).unwrap_or(64);
            let device_name = String::from_utf8_lossy(&name_buf[..name_end]).to_string();
            if !device_name.is_empty() {
                println!("Device name: {} ({})", device_name, self.device.serial);
                self.device.name = device_name.clone();
                if let Some(tx) = self.device_name_tx.take() {
                    let _ = tx.send((self.device.serial.clone(), device_name)).await;
                }
            }
        }

        // 启动通知轮询
        let notif_stop = Arc::new(AtomicBool::new(false));
        notification::spawn_notification_poller_tokio(
            self.adb.clone(),
            self.device.clone(),
            self.notif_tx.clone(),
            notif_stop.clone(),
            self.notification_enabled.clone(),
        );
        let (mut control_reader, mut control_writer) = tokio::io::split(stream);
        self.run_clipboard_io(
            &mut control_reader,
            &mut control_writer,
            stop_rx,
            self.device.serial.clone(),
        )
        .await;

        notif_stop.store(true, Ordering::SeqCst);
        server.stop(self.adb.as_ref());
    }
}
