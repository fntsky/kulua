use crate::adb_cmd::AdbOps;
use crate::notification::{self, NotifInfo};
use crate::scrcpy;
use crate::types::{AdbError, Device};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            clipboard_sync: true,
            notification_sync: true,
        }
    }
}
/// 轻量 session 句柄，主循环用来发停止信号 + 等待任务结束
pub struct Handle {
    #[allow(dead_code)]
    pub device: Device,
    pub stop_tx: Option<oneshot::Sender<()>>,
    pub task: tokio::task::JoinHandle<()>,
    /// Core 通过此标志动态控制剪贴板同步
    pub clipboard_enabled: Arc<AtomicBool>,
    /// Core 通过此标志动态控制通知同步
    pub notification_enabled: Arc<AtomicBool>,
}

impl Handle {
    pub async fn stop(&mut self, _adb: &dyn AdbOps) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        // 给 session 一点时间自行清理
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut self.task).await;
    }
}

/// 单个设备的完整 session。
///
/// 包含部署 scrcpy → TCP 连接 → 双向剪贴板 I/O + 通知轮询的完整生命周期。
/// 配置选项（`clipboard_enabled` / `notification_enabled`）是共享原子标志，
/// Core 可通过 Handle 随时调整，无需重启 session。
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
    /// 发送设备名称回 Core（serial, name）
    device_name_tx: Option<mpsc::Sender<(String, String)>>,
    notification_enabled: Arc<AtomicBool>,
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
            device_name_tx: Some(device_name_tx),
        }
    }

    /// 运行 session 主循环：部署 scrcpy → TCP 连接 → 双向剪贴板 I/O + 通知轮询。
    pub async fn run(&mut self) {
        let mut stop_rx = self.stop_rx.take().expect("run can only be called once");
        let port = self.port; // copy before closure

        // 1. 部署 scrcpy-server
        let adb = self.adb.clone();
        let device = self.device.clone();
        let jar_path = self.jar_path.clone();
        let server = tokio::task::spawn_blocking(move || {
            scrcpy::ScrcpyServer::deploy_clipboard_only(adb.as_ref(), &device, &jar_path, port)
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
        println!("scrcpy-server alive on {} (port {})", self.device.serial, self.port);

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
            // 找 null 终止符，截断有效部分
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

        // 5. 双向剪贴板 I/O
        self.run_clipboard_io(stream, stop_rx).await;

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

    /// 双向剪贴板 I/O。
    ///
    /// 内部通过 `self.clipboard_enabled` 控制是否实际转发数据：
    /// - 发往手机（PC→Phone）：仅 enabled 时写入 scrcpy 控制连接
    /// - 来自手机（Phone→PC）：仅 enabled 时发送给 Core
    async fn run_clipboard_io(
        &mut self,
        mut stream: TcpStream,
        mut stop_rx: oneshot::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                biased;
                _ = &mut stop_rx => break,

                // PC→Phone
                result = self.clip_sub.recv() => {
                    match result {
                        Ok(text) if self.clipboard_enabled.load(Ordering::SeqCst) => {
                            if let Err(e) = scrcpy::send_clipboard_async(&mut stream, &text).await {
                                eprintln!("send clipboard: {}", e);
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

                // Phone→PC
                result = read_phone_clipboard(&mut stream) => {
                    match result {
                        Ok(Some(text)) if self.clipboard_enabled.load(Ordering::SeqCst) => {
                            if self.phone_clip_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            }
        }
    }
}

/// 从 scrcpy 控制连接读取一条设备剪贴板消息（200ms 超时返回 None）。
async fn read_phone_clipboard(stream: &mut TcpStream) -> Result<Option<String>, AdbError> {
    let mut type_buf = [0u8; 1];
    let result = tokio::time::timeout(
        Duration::from_millis(200),
        stream.read_exact(&mut type_buf),
    )
    .await;
    match result {
        Err(_timeout) => return Ok(None),
        Ok(Err(e)) => return Err(AdbError::Io(e)),
        Ok(Ok(_)) => {}
    }

    if type_buf[0] != 0x00 {
        let mut extra = [0u8; 64];
        let n = stream.read(&mut extra).await.unwrap_or(0);
        println!(
            "scrcpy msg: type=0x{:02X}, payload ({} bytes): {:02X?}",
            type_buf[0], n, &extra[..n]
        );
        return Ok(None);
    }

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.map_err(AdbError::Io)?;
    let text_len = u32::from_be_bytes(len_buf) as usize;

    let mut text = vec![0u8; text_len];
    stream.read_exact(&mut text).await.map_err(AdbError::Io)?;

    let clip_text = String::from_utf8(text).map_err(|e| AdbError::Other(format!("{}", e)))?;
    println!("Phone clipboard: {}", clip_text);
    Ok(Some(clip_text))
}

