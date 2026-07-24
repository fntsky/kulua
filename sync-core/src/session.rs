use crate::adb_cmd::AdbOps;
use crate::notification::{self, NotifInfo};
use crate::scrcpy;
use crate::types::Device;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};

/// 每个 session 的独立配置。
#[derive(Debug, Clone, Copy)]
pub struct SessionConfig {
    /// 剪贴板同步开关
    pub clipboard_sync: bool,
    /// 通知同步开关
    pub notification_sync: bool,
    /// 音频同步开关
    pub audio_enabled: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            clipboard_sync: true,
            notification_sync: true,
            audio_enabled: true,
        }
    }
}

/// 轻量 session 句柄，主循环用来发停止信号 + 等待任务结束
pub struct Handle {
    #[allow(dead_code)]
    pub device: Device,
    /// session 占用的 ADB 转发端口，用于超时后的强制清理
    pub port: u16,
    pub stop_tx: Option<oneshot::Sender<()>>,
    pub task: tokio::task::JoinHandle<()>,
    /// Core 通过此标志动态控制剪贴板同步
    pub clipboard_enabled: Arc<AtomicBool>,
    /// Core 通过此标志动态控制通知同步
    pub notification_enabled: Arc<AtomicBool>,
    /// Core 通过此标志动态控制音频开关（重启 session 后生效）
    pub audio_enabled: Arc<AtomicBool>,
}
impl Handle {
    /// 停止 session（模仿官方关闭流程）。
    ///
    /// 1. 发停止信号 → session 主循环退出
    /// 2. session 内部 shutdown socket → server 收到 IO 错误 → 走 Java finally
    /// 3. 等待 task 退出（1s 看门狗）
    /// 4. 超时未退 → abort + ADB 强制清理
    pub async fn stop(&mut self, adb: &dyn AdbOps) {
        // (1) 发停止信号
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }

        // (2) 等待 session 自行清理（socket shutdown → server 应快速退出）
        //     官方 1s 看门狗，此处留 2s 给 Windows TCP 栈一点余量
        if tokio::time::timeout(Duration::from_secs(2), &mut self.task)
            .await
            .is_err()
        {
            // (3) 超时：task 可能卡住，强制 abort 后直接清理远程进程
            self.task.abort();
            eprintln!(
                "session {} stop timeout, force killing remote",
                self.device.serial
            );
            let kill_cmd = "kill -9 $(ps 2>/dev/null | grep com.genymobile.scrcpy | grep -v grep | awk '{print $2}') 2>/dev/null; true";
            let _ = adb.run(&["-s", &self.device.serial, "shell", kill_cmd]);
        }

        // (4) 清理 ADB 转发
        let _ = adb.run(&[
            "-s",
            &self.device.serial,
            "forward",
            "--remove",
            &format!("tcp:{}", self.port),
        ]);
    }
}

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
    device_name_tx: Option<mpsc::Sender<(String, String)>>,
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
        device_name_tx: mpsc::Sender<(String, String)>,
    ) -> Self {
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
            device_name_tx: Some(device_name_tx),
        }
    }

    /// 运行 session 主循环：部署 scrcpy → TCP 连接 → 双向剪贴板 I/O + 音频帧 + 通知轮询。
    pub async fn run(&mut self) {
        let mut stop_rx = self.stop_rx.take().expect("run can only be called once");
        let port = self.port;
        let audio_enabled = self.audio_enabled.load(Ordering::SeqCst);

        // 1. 部署 scrcpy-server（带音频开关）
        let adb = self.adb.clone();
        let device = self.device.clone();
        let jar_path = self.jar_path.clone();
        let server = tokio::task::spawn_blocking(move || {
            scrcpy::ScrcpyServer::deploy_scrcpy(
                adb.as_ref(),
                &device,
                &jar_path,
                port,
                audio_enabled,
            )
        })
        .await;

        let mut server = match server {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                eprintln!("scrcpy deploy failed for {}: {:?}", self.device.serial, e);
                self.run_notification_only(stop_rx).await;
                return;
            }
            Err(e) => {
                eprintln!("spawn_blocking panic: {}", e);
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
            server.stop(self.adb.as_ref());
            return;
        }
        let codec_id = u32::from_be_bytes(codec_buf);
        if codec_id == 0 {
            println!("Audio stream disabled by device, continuing without audio");
        } else if codec_id == 1 {
            eprintln!("Audio stream configuration error on {}", self.device.serial);
            server.stop(self.adb.as_ref());
            return;
        } else {
            let codec_name = audio_codec_name(codec_id);
            println!(
                "Audio codec: {} (0x{:08x}) from {}",
                codec_name, codec_id, self.device.serial
            );
        }
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
        } else {
            let serial = self.device.serial.clone();
            Some(tokio::spawn(async move {
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

                    if (pts_raw >> 62) & 1 == 0 {
                        // 不是 config packet → 正常音频帧
                        // TODO: 解码播放
                        println!("audio frame: {} bytes from {}", frame_size, serial);
                    }
                }
            }))
        };

        // 7. 双向剪贴板 I/O（control channel）
        self.run_clipboard_io(&mut control_reader, &mut control_writer, stop_rx, self.device.serial.clone())
            .await;

        // 8. 清理（模仿官方关闭流程）
        //    run_clipboard_io 返回 → control_reader/writer 已释放 → control socket 关闭
        //    → server 侧 ControlChannel.recv() 收到 IOException

        // (a) shutdown audio socket → server 侧 Streamer.writePacket() 收到 IO 错误
        drop(audio_writer);   // 释放我们的 WriteHalf 引用
        if let Some(task) = audio_task {
            task.abort();     // 释放 ReadHalf → Arc 归零 → socket 关闭
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
            eprintln!("server {} did not exit after socket shutdown, force killing", self.device.serial);
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
        self.run_clipboard_io(&mut control_reader, &mut control_writer, stop_rx, self.device.serial.clone())
            .await;

        notif_stop.store(true, Ordering::SeqCst);
        server.stop(self.adb.as_ref());
    }
    /// 剪贴板服务不可用时仅启动通知轮询。
    async fn run_notification_only(&mut self, stop_rx: oneshot::Receiver<()>) {
        let stop = Arc::new(AtomicBool::new(false));
        notification::spawn_notification_poller_tokio(
            self.adb.clone(),
            self.device.clone(),
            self.notif_tx.clone(),
            stop.clone(),
            self.notification_enabled.clone(),
        );
        let _ = stop_rx.await;
        stop.store(true, Ordering::SeqCst);
    }
}

// ── 协议常量 ──

const DEVICE_MSG_TYPE_CLIPBOARD: u8 = 0x00;
const DEVICE_MSG_TYPE_ACK_CLIPBOARD: u8 = 0x01;
const DEVICE_MSG_TYPE_UHID_OUTPUT: u8 = 0x02;

/// 从 control socket 读取一条设备消息。
///
/// 接受 `impl AsyncReadExt + Unpin`（tokio::io::ReadHalf 满足此约束）。
/// 返回:
/// - `Ok(Some(text))` — type 0x00 (CLIPBOARD)
/// - `Ok(None)` — 其他已知类型（内部已处理/静默忽略）
/// - `Err(())` — 连接断开或协议错误（未知 type 视为不可恢复）
async fn read_device_message(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> Result<Option<String>, ()> {
    let mut type_buf = [0u8; 1];
    reader.read_exact(&mut type_buf).await.map_err(|_| ())?;

    match type_buf[0] {
        DEVICE_MSG_TYPE_CLIPBOARD => {
            let mut len_buf = [0u8; 4];
            reader.read_exact(&mut len_buf).await.map_err(|_| ())?;
            let text_len = u32::from_be_bytes(len_buf) as usize;
            if text_len > 256 * 1024 {
                return Err(());
            }
            let mut text = vec![0u8; text_len];
            reader.read_exact(&mut text).await.map_err(|_| ())?;
            let clip_text = String::from_utf8(text).map_err(|_| ())?;
            Ok(Some(clip_text))
        }
        DEVICE_MSG_TYPE_ACK_CLIPBOARD => {
            // 8-byte sequence number, 转发逻辑中无需处理
            let mut seq_buf = [0u8; 8];
            reader.read_exact(&mut seq_buf).await.map_err(|_| ())?;
            Ok(None)
        }
        DEVICE_MSG_TYPE_UHID_OUTPUT => {
            // 2B id + 2B length + data
            let mut meta = [0u8; 4];
            reader.read_exact(&mut meta).await.map_err(|_| ())?;
            let _id = u16::from_be_bytes([meta[0], meta[1]]);
            let data_len = u16::from_be_bytes([meta[2], meta[3]]) as usize;
            if data_len > 4096 {
                return Err(());
            }
            let mut _data = vec![0u8; data_len];
            reader.read_exact(&mut _data).await.map_err(|_| ())?;
            // UHID 输出，当前不处理
            Ok(None)
        }
        _ => {
            // 未知 type → 协议不可恢复
            Err(())
        }
    }
}

/// 音频 codec ID → 可读名称
fn audio_codec_name(id: u32) -> &'static str {
    match id {
        0x6f707573 => "OPUS",
        0x00616163 => "AAC",
        0x666c6163 => "FLAC",
        0x00726177 => "RAW",
        _ => "UNKNOWN",
    }
}

fn build_clipboard_frame(text: &str) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let mut buf = Vec::with_capacity(14 + text_bytes.len());
    buf.push(0x09); // TYPE_SET_CLIPBOARD
    buf.extend_from_slice(&[0u8; 8]); // 序列号（固定 0）
    buf.push(0); // paste 标记（不自动粘贴）
    buf.extend_from_slice(&(text_bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(text_bytes);
    buf
}
