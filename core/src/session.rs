use crate::adb_cmd::AdbOps;
use crate::notification::{self, NotifInfo};
use crate::scrcpy::{self, ScrcpyServer};
use crate::types::{AdbError, Device};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub struct Session {
    pub device: Device,
    scrcpy: Option<ScrcpyServer>,
    clipboard_listener: Option<JoinHandle<()>>,
    notification_poller: Option<JoinHandle<()>>,
    notification_stop: Arc<AtomicBool>,
    /// Core→Phone: Core 通过此 channel 发送剪贴板文本给 listener 线程写入设备
    pub ctrl_tx: Sender<String>,
    /// Phone→Core: listener 收到设备剪贴板文本后通过此 channel 转发给 Core
    pub phone_clipboard_rx: Receiver<String>,
    /// Phone→Core: 通知轮询线程收到新通知后通过此 channel 转发给 Core
    pub notification_rx: Receiver<NotifInfo>,
}

impl Session {
    pub fn new(device: Device) -> Self {
        let (ctrl_tx, _ctrl_rx) = mpsc::channel();
        let (_phone_tx, phone_clipboard_rx) = mpsc::channel();
        let (_notif_tx, notification_rx) = mpsc::channel();
        // 默认 channel 会在 listener/poller 创建时被替换
        Self {
            device,
            scrcpy: None,
            clipboard_listener: None,
            notification_poller: None,
            notification_stop: Arc::new(AtomicBool::new(false)),
            ctrl_tx,
            phone_clipboard_rx,
            notification_rx,
        }
    }

    /// 部署 scrcpy-server + 启动剪贴板双向服务。
    ///
    /// scrcpy 控制连接是双向的：在同一个 TcpStream 上接收设备消息
    /// （Phone→PC 剪贴板）和发送控制指令（PC→Phone 剪贴板）。
    pub fn start_clipboard_service(
        &mut self,
        adb: &dyn AdbOps,
        local_jar: &str,
        port: u16,
    ) -> Result<(), AdbError> {
        println!(
            "Deploying scrcpy-server to {} on port {}...",
            self.device.serial, port
        );

        // 部署 server + forward（只 forward 一个端口，数据和控制共用）
        let server = ScrcpyServer::deploy_clipboard_only(adb, &self.device, local_jar, port)?;
        self.scrcpy = Some(server);

        // 创建双向 channel
        let (ctrl_tx, ctrl_rx) = mpsc::channel();
        let (phone_tx, phone_rx) = mpsc::channel();

        // 启动监听线程（内部会重试连接），等待它连上 server
        let (handle, alive) =
            scrcpy::spawn_clipboard_listener(port, ctrl_rx, phone_tx);
        match alive.recv_timeout(Duration::from_secs(10)) {
            Ok(()) => println!(
                "✓ scrcpy-server alive on {} (port {})",
                self.device.serial, port
            ),
            Err(_) => {
                // 监听线程连不上，清理已部署的资源
                self.stop(adb);
                return Err(AdbError::Other(format!(
                    "clipboard listener on port {}: server not responding within 10s",
                    port
                )));
            }
        }
        self.clipboard_listener = Some(handle);
        self.ctrl_tx = ctrl_tx;
        self.phone_clipboard_rx = phone_rx;

        Ok(())
    }

    /// 通过 ctrl channel 向设备发送剪贴板文本（PC→Phone）。
    pub fn write_clipboard(&self, text: &str) -> Result<(), AdbError> {
        self.ctrl_tx
            .send(text.to_string())
            .map_err(|e| AdbError::Other(format!("ctrl channel send: {}", e)))
    }

    /// 启动通知轮询线程。启动后独立于 scrcpy 服务运行，
    /// 周期性地通过 adb shell 拉取通知列表并 diff key 集合。
    pub fn start_notification_polling(&mut self, adb: Arc<dyn AdbOps>) {
        let (notif_tx, notif_rx) = mpsc::channel::<NotifInfo>();
        let (handle, stop) =
            notification::spawn_notification_poller(adb, self.device.clone(), notif_tx);
        self.notification_poller = Some(handle);
        self.notification_stop = stop;
        self.notification_rx = notif_rx;
    }

    #[allow(dead_code)]
    pub fn is_clipboard_running(&self) -> bool {
        self.scrcpy.is_some()
    }

    pub fn stop(&mut self, adb: &dyn AdbOps) {
        // 先停 scrcpy server（kill 进程 + 移除端口转发）
        if let Some(mut server) = self.scrcpy.take() {
            server.stop(adb);
        }

        // 等待剪贴板监听线程结束（TcpStream 读到错误时自行退出）
        if let Some(handle) = self.clipboard_listener.take() {
            let _ = handle.join();
        }

        // 停止通知轮询线程
        self.notification_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.notification_poller.take() {
            let _ = handle.join();
        }
    }
}
