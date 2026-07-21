use crate::adb_cmd::{AdbCmd, AdbOps};
use crate::notification::{self, NotifInfo};
use crate::session;
use crate::types::{Device, DeviceState};
use crate::wireless_pair;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

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
    pending_serials: HashSet<String>,
    /// 上次对每个 pending 设备发起 adb connect 的时间（用于 5s 重试间隔）
    last_connect_attempt: HashMap<String, Instant>,
    port_counter: u16,
    stop_flag: Arc<AtomicBool>,

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

        // 启动 mDNS 发现（同步线程 → 桥接到 tokio mpsc）
        let (mdns_tx, mdns_rx) = mpsc::channel(16);
        let mdns_handle = if let Ok((handle, std_rx)) =
            wireless_pair::start_discovery(&pair_info)
        {
            tokio::task::spawn_blocking(move || {
                while let Ok(event) = std_rx.recv() {
                    if mdns_tx.blocking_send(event).is_err() {
                        break;
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
        Self::spawn_device_refresh(device_tx);

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
            pending_serials: HashSet::new(),
            last_connect_attempt: HashMap::new(),
            port_counter: 27183,
            stop_flag: Arc::new(AtomicBool::new(false)),
            clipboard_last_seen: None,
            last_received_from_phone: None,
            last_clipboard_error_print: Instant::now(),
        }
    }

    pub fn get_stop_flag(&self) -> Arc<AtomicBool> {
        self.stop_flag.clone()
    }

    /// 后台任务：每 1s 执行 `adb devices`，通过 watch channel 推送最新设备列表
    fn spawn_device_refresh(device_tx: watch::Sender<HashMap<String, Device>>) {
        tokio::task::spawn_blocking(move || {
            let adb = AdbCmd::new();
            loop {
                let mut map = HashMap::new();
                if let Ok(list) = adb.devices() {
                    for d in list {
                        if d.state == DeviceState::Device {
                            map.insert(d.serial.clone(), d);
                        }
                    }
                }
                let _ = device_tx.send(map);
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    // ─── 主循环 ──────────────────────────────────────────

    pub async fn run(&mut self) {
        loop {
            if self.should_stop() {
                break;
            }

            // 1. 处理 mDNS 发现事件
            self.handle_mdns_events().await;

            // 2. 收集 Phone→PC 事件
            self.drain_phone_clipboard().await;
            self.drain_notifications().await;

            // 3. 轮询 PC 系统剪贴板
            self.poll_system_clipboard();

            // 4. 管理设备连接 & session
            let devices = self.device_watch.borrow().clone();
            self.try_connect_pending(&devices);
            self.sync_sessions(devices).await;

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        self.stop_all().await;
    }

    fn should_stop(&self) -> bool {
        self.stop_flag.load(Ordering::SeqCst)
    }

    // ─── 各步骤 ──────────────────────────────────────────

    async fn handle_mdns_events(&mut self) {
        while let Ok(event) = self.mdns_rx.try_recv() {
            match event {
                wireless_pair::MdnsEvent::PairingDiscovered { host, port } => {
                    println!("Discovered device at {}:{}", host, port);
                    let addr = format!("{}:{}", host, port);
                    if let Err(e) = self.adb_cmd.wireless_pair(&addr, &self.pair_info) {
                        eprintln!("Wireless pair failed: {}", e);
                        continue;
                    }
                    self.pending_serials.insert(format!("{}:5555", host));
                }
                wireless_pair::MdnsEvent::Error(e) => {
                    eprintln!("mDNS error: {}", e);
                }
            }
        }
    }

    async fn drain_phone_clipboard(&mut self) {
        while let Ok(text) = self.phone_clip_rx.try_recv() {
            if Some(&text) == self.last_received_from_phone.as_ref() {
                continue;
            }
            if Some(&text) == self.clipboard_last_seen.as_ref() {
                continue;
            }
            self.last_received_from_phone = Some(text.clone());

            println!("System clipboard (from phone): {}", text);
            if let Err(e) = clipboard_win::set_clipboard_string(&text) {
                eprintln!("Clipboard: failed to write from phone: {}", e);
            }
        }
    }

    async fn drain_notifications(&mut self) {
        while let Ok(notif) = self.notif_rx.try_recv() {
            if let Some(title) = &notif.title {
                println!("通知: [{}] {}", notif.package, title);
            } else {
                println!("通知: [{}]", notif.package);
            }
            notification::show_desktop_notification(&notif);
        }
    }

    fn poll_system_clipboard(&mut self) {
        match clipboard_win::get_clipboard_string() {
            Ok(ref text) if Some(text.as_str()) != self.clipboard_last_seen.as_deref() => {
                self.clipboard_last_seen = Some(text.clone());
                self.last_clipboard_error_print = Instant::now();
                if Some(text) != self.last_received_from_phone.as_ref() {
                    println!(
                        "Clipboard (PC→{} devices): {}",
                        self.sessions.len(),
                        text
                    );
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
    fn try_connect_pending(&mut self, devices: &HashMap<String, Device>) {
        let now = Instant::now();
        let serials: Vec<String> = self.pending_serials.iter().cloned().collect();
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
            println!("Connecting to {}...", serial);
            let _ = self.adb_cmd.connect(serial);
            self.last_connect_attempt.insert(serial.clone(), now);
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
            adb, device.clone(), jar_path, port,
            clip_sub, phone_clip_tx, notif_tx, stop_rx,
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
        self.stop_flag.store(true, Ordering::SeqCst);
        for (_, mut handle) in self.sessions.drain() {
            handle.stop(self.adb_cmd.as_ref()).await;
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DeviceState;

    #[test]
    fn test_device_filter() {
        let raw = vec![
            Device { serial: "ok".into(), state: DeviceState::Device },
            Device { serial: "off".into(), state: DeviceState::Offline },
            Device { serial: "unauth".into(), state: DeviceState::Unauthorized },
            Device { serial: "ok2".into(), state: DeviceState::Device },
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
            pending_serials: ["192.168.1.100:5555".into()].into(),
            last_connect_attempt: HashMap::new(),
            port_counter: 27183,
            stop_flag: Arc::new(AtomicBool::new(false)),
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
