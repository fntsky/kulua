use crate::adb_cmd::AdbOps;
use crate::notification::{self, NotifInfo};
use crate::scrcpy;
use crate::types::Device;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;
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
            audio_enabled: false,
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
    pub async fn stop(&mut self, adb: &dyn AdbOps) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }

        // 给 session 一点时间自行清理（正常路径会调用 server.stop）
        if tokio::time::timeout(Duration::from_secs(3), &mut self.task).await.is_err() {
            // 超时：session 可能卡住，强制 abort + 直接清理远程进程
            self.task.abort();
            let kill_cmd = "kill -9 $(ps 2>/dev/null | grep com.genymobile.scrcpy | grep -v grep | awk '{print $2}') 2>/dev/null; true";
            let _ = adb.run(&["-s", &self.device.serial, "shell", kill_cmd]);
            let _ = adb.run(&[
                "-s",
                &self.device.serial,
                "forward",
                "--remove",
                &format!("tcp:{}", self.port),
            ]);
        }
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
        let port = self.port; // copy before closure
        let audio_enabled = self.audio_enabled.load(Ordering::SeqCst);

        // 1. 部署 scrcpy-server（带音频开关）
        let adb = self.adb.clone();
        let device = self.device.clone();
        let jar_path = self.jar_path.clone();
        let server = tokio::task::spawn_blocking(move || {
            scrcpy::ScrcpyServer::deploy_scrcpy(adb.as_ref(), &device, &jar_path, port, audio_enabled)
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

        // 2. 连接 scrcpy TCP 控制通道
        let mut stream = loop {
            tokio::select! {
                _ = &mut stop_rx => {
                    server.stop(self.adb.as_ref());
                    return;
                }
                r = TcpStream::connect(format!("127.0.0.1:{}", self.port)) => {
                    if let Ok(mut s) = r {
                        let mut dummy = [0u8; 1];
                        tokio::select! {
                            _ = &mut stop_rx => {
                                server.stop(self.adb.as_ref());
                                return;
                            }
                            rr = s.read_exact(&mut dummy) => {
                                if rr.is_ok() { break s; }
                            }
                        }
                    }
                }
            }
            tokio::select! {
                _ = &mut stop_rx => {
                    server.stop(self.adb.as_ref());
                    return;
                }
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        };

        // 3. 读取设备名称（64 字节，scrcpy 协议设备信息交换）
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

        // 4. 启动通知轮询
        let notif_stop = Arc::new(AtomicBool::new(false));
        notification::spawn_notification_poller_tokio(
            self.adb.clone(),
            self.device.clone(),
            self.notif_tx.clone(),
            notif_stop.clone(),
            self.notification_enabled.clone(),
        );

        // 5. 双向设备 I/O（剪贴板 + 音频）
        self.run_device_io(stream, stop_rx).await;

        // 6. 清理
        notif_stop.store(true, Ordering::SeqCst);
        server.stop(self.adb.as_ref());
        println!("Session {} cleaned up", self.device.serial);
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
    /// 双向设备 I/O：剪贴板（PC←→Phone）+ 音频帧（Phone→PC 本地播放）。
    async fn run_device_io(
        &mut self,
        stream: TcpStream,
        mut stop_rx: oneshot::Receiver<()>,
    ) {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (clip_tx, mut clip_rx) = mpsc::channel::<String>(16);

        let clipboard_enabled = self.clipboard_enabled.clone();
        let phone_clip_tx = self.phone_clip_tx.clone();
        let audio_enabled = self.audio_enabled.clone();
        let serial = self.device.serial.clone();

        let reader_task = tokio::spawn(async move {
            loop {
                let mut type_buf = [0u8; 1];
                if reader.read_exact(&mut type_buf).await.is_err() { break; }
                match type_buf[0] {
                    0x00 => {
                        let mut len_buf = [0u8; 4];
                        if reader.read_exact(&mut len_buf).await.is_err() { break; }
                        let text_len = u32::from_be_bytes(len_buf) as usize;
                        let mut text = vec![0u8; text_len];
                        if reader.read_exact(&mut text).await.is_err() { break; }
                        if clipboard_enabled.load(Ordering::SeqCst) {
                            if let Ok(t) = String::from_utf8(text) {
                                let _ = clip_tx.send(t).await;
                            }
                        }
                    }
                    0x0a | 0x0b => {
                        let mut pts_buf = [0u8; 8];
                        if reader.read_exact(&mut pts_buf).await.is_err() { break; }
                        let mut size_buf = [0u8; 4];
                        if reader.read_exact(&mut size_buf).await.is_err() { break; }
                        let frame_size = u32::from_le_bytes(size_buf) as usize;
                        if frame_size > 524288 { break; }
                        let mut audio_data = vec![0u8; frame_size];
                        if reader.read_exact(&mut audio_data).await.is_err() { break; }
                        if audio_enabled.load(Ordering::SeqCst) {
                            println!("audio frame: {} bytes from {}", frame_size, serial);
                            let _ = audio_data;
                        }
                    }
                    _ => {
                        let mut dump = [0u8; 8192];
                        if reader.read(&mut dump).await.unwrap_or(0) == 0 { break; }
                    }
                }
            }
        });

        loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => break,
                result = self.clip_sub.recv() => {
                    match result {
                        Ok(text) if self.clipboard_enabled.load(Ordering::SeqCst) => {
                            use tokio::io::AsyncWriteExt;
                            let msg = build_clipboard_frame(&text);
                            if writer.write_all(&msg).await.is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("clipboard broadcast lagged by {}", n);
                        }
                    }
                }
                Some(text) = clip_rx.recv() => {
                    println!("Phone clipboard: {}", text);
                    if phone_clip_tx.send(text).await.is_err() { break; }
                }
            }
        }

        // 通知退出 reader 任务，释放 TCP 读半通道
        reader_task.abort();
    }
}

fn build_clipboard_frame(text: &str) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let mut buf = Vec::with_capacity(14 + text_bytes.len());
    buf.push(0x09);
    buf.extend_from_slice(&[0u8; 8]);
    buf.push(0);
    buf.extend_from_slice(&(text_bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(text_bytes);
    buf
}
