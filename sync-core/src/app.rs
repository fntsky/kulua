use crate::adb_cmd::{AdbCmd, AdbOps};
use crate::notification::{self, NotifInfo};
use crate::session;
use crate::types::Device;
use crate::wireless_pair;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

#[allow(dead_code)]
/// 外部命令：UI 或其他组件通过此枚举向主循环发送请求
pub enum Command {
    Connect(String),
    Disconnect(String),
}

pub struct Core {
    adb_cmd: Arc<dyn AdbOps>,
    jar_path: String,
    pair_info: wireless_pair::WirelessPairing,

    // ── 后台任务句柄（hold 住防止 drop） ──
    _mdns_handle: wireless_pair::MdnsHandle,
    _refresh_handle: bool, // 后台线程不阻塞，放在这里只是个标记

    // ── 事件流 ──
    mdns_rx: mpsc::Receiver<wireless_pair::MdnsEvent>,
    device_watch: watch::Receiver<HashMap<String, Device>>,

    // ── 事件总线 ──
    clip_broadcast: broadcast::Sender<String>,
    phone_clip_rx: mpsc::Receiver<String>,
    phone_clip_tx: mpsc::Sender<String>,
    notif_rx: mpsc::Receiver<NotifInfo>,
    notif_tx: mpsc::Sender<NotifInfo>,

    // ── 状态 ──
    sessions: HashMap<String, session::Handle>,
    pending_serials: HashMap<String, Instant>,
    /// 上次对每个 pending 设备发起 adb connect 的时间（用于 5s 重试间隔）
    last_connect_attempt: HashMap<String, Instant>,
    port_counter: u16,
    token: CancellationToken,

    // ── 剪贴板防回环 ──
    clipboard_last_seen: Option<String>,
    last_received_from_phone: Option<String>,
    last_clipboard_error_print: Instant,
}

impl Core {
    pub fn new(jar_path: String, pair_info: wireless_pair::WirelessPairing) -> Self {
        let (clip_tx, _) = broadcast::channel(64);
        let (phone_clip_tx, phone_clip_rx) = mpsc::channel(256);
        let (notif_tx, notif_rx) = mpsc::channel(64);
        let (device_tx, device_watch) = watch::channel(HashMap::new());

        let token = CancellationToken::new();

        // 启动 mDNS 发现（同步线程 → 桥接到 tokio mpsc）
        let (mdns_tx, mdns_rx) = mpsc::channel(16);
        let mdns_handle = if let Ok((handle, std_rx)) = wireless_pair::start_discovery(&pair_info) {
            let token = token.clone();
            tokio::task::spawn_blocking(move || {
                while !token.is_cancelled() {
                    match std_rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(event) => {
                            if mdns_tx.blocking_send(event).is_err() {
                                break;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });
            handle
        } else {
            eprintln!("mDNS discovery failed to start");
            // 空 handle 占位，防止 Core::new 失败
            wireless_pair::MdnsHandle::empty()
        };

        // 启动设备刷新后台任务
        crate::device_refresh::spawn(device_tx, token.clone());

        Self {
            adb_cmd: Arc::new(AdbCmd::new()),
            jar_path,
            pair_info,
            _mdns_handle: mdns_handle,
            _refresh_handle: true,
            mdns_rx,
            device_watch,
            clip_broadcast: clip_tx,
            phone_clip_rx,
            phone_clip_tx,
            notif_rx,
            notif_tx,
            sessions: HashMap::new(),
            pending_serials: HashMap::new(),
            last_connect_attempt: HashMap::new(),
            port_counter: 27183,
            token,
            clipboard_last_seen: None,
            last_received_from_phone: None,
            last_clipboard_error_print: Instant::now(),
        }
    }

    pub fn get_token(&self) -> CancellationToken {
        self.token.clone()
    }


    // ─── 主循环 ──────────────────────────────────────────

    pub async fn run(&mut self, mut cmd_rx: mpsc::Receiver<Command>) {
        let mut tick = tokio::time::interval(Duration::from_millis(150));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = self.token.cancelled() => break,
                Some(ev)   = self.mdns_rx.recv()       => self.handle_mdns_event(ev).await,
                Some(text) = self.phone_clip_rx.recv()  => self.on_phone_clipboard(text),
                Some(n)    = self.notif_rx.recv()       => self.on_notification(n),
                Some(cmd)  = cmd_rx.recv()              => self.on_command(cmd).await,
                _ = tick.tick() => {
                    self.poll_system_clipboard();
                    let devices = self.device_watch.borrow_and_update().clone();
                    self.try_connect_pending(&devices);
                    self.sync_sessions(devices).await;
                }
            }
        }
        self.stop_all().await;
    }

    // ─── 事件处理 ────────────────────────────────────────

    async fn handle_mdns_event(&mut self, event: wireless_pair::MdnsEvent) {
        match event {
            wireless_pair::MdnsEvent::PairingDiscovered { host, port } => {
                println!("[PAIRING]Discovered device at {}:{}", host, port);
                let addr = format!("{}:{}", host, port);
                if let Err(e) = self.adb_cmd.wireless_pair(&addr, &self.pair_info) {
                    eprintln!("Wireless pair failed: {}", e);
                    return;
                }
            }
            wireless_pair::MdnsEvent::ConnectDiscovered { host, port } => {
                println!("[CONNECT] Discovered device at {}:{}", host, port);
                self.pending_serials.insert(format!("{}:{}", host, port), Instant::now());
            }
            wireless_pair::MdnsEvent::Error(e) => {
                eprintln!("mDNS error: {}", e);
            }
        }
    }

    fn on_phone_clipboard(&mut self, text: String) {
        if Some(&text) == self.last_received_from_phone.as_ref() {
            return;
        }
        if Some(&text) == self.clipboard_last_seen.as_ref() {
            return;
        }
        self.last_received_from_phone = Some(text.clone());

        println!("System clipboard (from phone): {}", text);
        if let Err(e) = clipboard_win::set_clipboard_string(&text) {
            eprintln!("Clipboard: failed to write from phone: {}", e);
        }
        self.clipboard_last_seen = Some(text);
    }

    fn on_notification(&mut self, notif: NotifInfo) {
        if let Some(title) = &notif.title {
            println!("通知: [{}] {}", notif.package, title);
        } else {
            println!("通知: [{}]", notif.package);
        }
        notification::show_desktop_notification(&notif);
    }

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Connect(serial) => {
                self.pending_serials.insert(serial, Instant::now());
            }
            Command::Disconnect(serial) => {
                if let Some(mut handle) = self.sessions.remove(&serial) {
                    handle.stop(self.adb_cmd.as_ref()).await;
                }
                self.pending_serials.remove(&serial);
            }
        }
    }

    fn poll_system_clipboard(&mut self) {
        match &clipboard_win::get_clipboard_string() {
            Ok(text) if Some(text.as_str()) != self.clipboard_last_seen.as_deref() => {
                self.clipboard_last_seen = Some(text.clone());
                self.last_clipboard_error_print = Instant::now();
                if Some(text.as_str()) != self.last_received_from_phone.as_deref() {
                    println!("Clipboard (PC→{} devices): {}", self.sessions.len(), text);
                    let _ = self.clip_broadcast.send(text.to_string());
                }
            }
            Ok(_) => {}
            Err(e) => {
                if self.last_clipboard_error_print.elapsed() >= Duration::from_secs(30) {
                    eprintln!("Clipboard: failed to read system clipboard: {}", e);
                    self.last_clipboard_error_print = Instant::now();
                }
            }
        }
    }

    /// 对 pending 队列中的设备发起 adb connect。
    ///
    /// - 设备已在 `devices` 中（刷新线程确认已连接）→ 从 pending 移除
    /// - 5 秒内已尝试过 → 跳过（避免高频重试）
    /// - 否则发起 adb connect（无论成功失败，等刷新线程确认）
    /// - pending 超过 30 秒未连接成功 → 自动丢弃
    fn try_connect_pending(&mut self, devices: &HashMap<String, Device>) {
        let now = Instant::now();

        // 清理超时条目（30 秒未连接成功则放弃）
        let timed_out: Vec<String> = self
            .pending_serials
            .iter()
            .filter(|(_, added)| now.duration_since(**added) > Duration::from_secs(30))
            .map(|(serial, _)| serial.clone())
            .collect();
        for serial in &timed_out {
            self.pending_serials.remove(serial);
            self.last_connect_attempt.remove(serial);
        }

        let serials: Vec<String> = self.pending_serials.keys().cloned().collect();
        for serial in &serials {
            if devices.contains_key(serial) {
                // 刷新线程已确认设备在线，从 pending 移除
                self.pending_serials.remove(serial);
                self.last_connect_attempt.remove(serial);
                continue;
            }
            // 5 秒内已试过，跳过
            if let Some(last) = self.last_connect_attempt.get(serial) {
                if now.duration_since(*last) < Duration::from_secs(5) {
                    continue;
                }
            }
            match self.adb_cmd.connect(serial) {
                Ok(()) => {
                    println!("  connect {}: connected", serial);
                    self.pending_serials.remove(serial);
                    self.last_connect_attempt.remove(serial);
                }
                Err(e) => {
                    eprintln!("  connect {} failed: {}", serial, e);
                    self.last_connect_attempt.insert(serial.clone(), now);
                }
            }
        }
    }

    async fn sync_sessions(&mut self, devices: HashMap<String, Device>) {
        for (serial, device) in &devices {
            if self.sessions.contains_key(serial) {
                continue;
            }
            self.start_session(device.clone()).await;
        }

        let active: Vec<String> = self.sessions.keys().cloned().collect();
        for serial in &active {
            if !devices.contains_key(serial) {
                if let Some(mut handle) = self.sessions.remove(serial) {
                    println!("Stopped session for {}", serial);
                    handle.stop(self.adb_cmd.as_ref()).await;
                }
            }
        }
    }

    async fn start_session(&mut self, device: Device) {
        let port = self.port_counter;
        self.port_counter += 1;

        let (stop_tx, stop_rx) = oneshot::channel();
        let clip_sub = self.clip_broadcast.subscribe();
        let phone_clip_tx = self.phone_clip_tx.clone();
        let notif_tx = self.notif_tx.clone();
        let adb = self.adb_cmd.clone();
        let jar_path = self.jar_path.clone();

        let task = tokio::spawn(session::run(
            adb,
            device.clone(),
            jar_path,
            port,
            clip_sub,
            phone_clip_tx,
            notif_tx,
            stop_rx,
        ));

        self.sessions.insert(
            device.serial.clone(),
            session::Handle {
                device,
                stop_tx: Some(stop_tx),
                task,
            },
        );
    }

    async fn stop_all(&mut self) {
        self.token.cancel();
        for (_, mut handle) in self.sessions.drain() {
            handle.stop(self.adb_cmd.as_ref()).await;
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DeviceState;

    #[test]
    fn test_device_filter() {
        let raw = vec![
            Device {
                serial: "ok".into(),
                state: DeviceState::Device,
            },
            Device {
                serial: "off".into(),
                state: DeviceState::Offline,
            },
            Device {
                serial: "unauth".into(),
                state: DeviceState::Unauthorized,
            },
            Device {
                serial: "ok2".into(),
                state: DeviceState::Device,
            },
        ];
        let mut map = HashMap::new();
        for d in raw {
            if d.state == DeviceState::Device {
                map.insert(d.serial.clone(), d);
            }
        }
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("ok"));
        assert!(map.contains_key("ok2"));
    }

    #[test]
    fn test_try_connect_pending_removes_when_device_in_map() {
        let (phone_clip_tx, _) = mpsc::channel(256);
        let (notif_tx, _) = mpsc::channel(64);
        let (clip_tx, _) = broadcast::channel(64);
        let (_device_tx, device_watch) = watch::channel(HashMap::new());
        let (_mdns_tx, mdns_rx) = mpsc::channel(16);

        // 模拟：pending 中有设备，但 devices 是空的
        let mut core = Core {
            adb_cmd: Arc::new(AdbCmd::new()),
            jar_path: "test".into(),
            pair_info: wireless_pair::WirelessPairing::new(),
            _mdns_handle: wireless_pair::MdnsHandle::empty(),
            _refresh_handle: true,
            mdns_rx,
            device_watch: device_watch.clone(),
            clip_broadcast: clip_tx,
            phone_clip_rx: mpsc::channel(256).1,
            phone_clip_tx,
            notif_rx: mpsc::channel(64).1,
            notif_tx,
            sessions: HashMap::new(),
            pending_serials: HashMap::from([("192.168.1.100:5555".into(), Instant::now())]),
            last_connect_attempt: HashMap::new(),
            port_counter: 27183,
            token: CancellationToken::new(),
            clipboard_last_seen: None,
            last_received_from_phone: None,
            last_clipboard_error_print: Instant::now(),
        };

        // devices map 已有该设备，try_connect_pending 应将其从 pending 移除
        let devices = HashMap::from([(
            "192.168.1.100:5555".into(),
            Device {
                serial: "192.168.1.100:5555".into(),
                state: DeviceState::Device,
            },
        )]);

        core.try_connect_pending(&devices);
        assert!(core.pending_serials.is_empty());
        assert!(devices.contains_key("192.168.1.100:5555"));
    }
}
