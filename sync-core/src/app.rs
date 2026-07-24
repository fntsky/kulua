use crate::adb_cmd::{AdbCmd, AdbOps};
use crate::notification::{self, NotifInfo};
use crate::session;
use crate::types::{Device, DeviceAddrKind, DeviceId, DeviceIdentity, DeviceState, PendingDevice};
use crate::ipc::types::SessionSummary;
use crate::wireless_pair;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

#[allow(dead_code)]
/// 外部命令：UI 或其他组件通过此枚举向主循环发送请求
pub enum Command {
    Connect(String),
    Disconnect(String),
    UpdateConfig {
        serial: String,
        clipboard_sync: bool,
        notification_sync: bool,
        audio_sync: bool,
    },
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
    /// 广播通知给 IPC 等订阅者
    notif_broadcast: broadcast::Sender<NotifInfo>,

    // ── IPC 命令通道 ──
    cmd_tx: mpsc::Sender<Command>,
    cmd_rx: Option<mpsc::Receiver<Command>>,

    // ── 状态 ──
    sessions: HashMap<String, session::Handle>,
    /// 每个连接的设备 session 配置
    session_configs: HashMap<String, session::SessionConfig>,
    /// 按 DeviceId 索引，含三层地址（USB/mDNS/IP）
    pending_serials: HashMap<DeviceId, PendingDevice>,
    /// 按规范 DeviceId 去重的设备映射（权威数据源）
    devices_by_id: HashMap<DeviceId, Device>,
    port_counter: u16,
    token: CancellationToken,
    /// 用于从 session 接收设备名称并更新 device_watch
    device_name_rx: mpsc::Receiver<(String, String)>,
    /// sender 副本，传给每个新 session
    device_name_tx: mpsc::Sender<(String, String)>,
    /// device_watch 的 sender 副本（用于更新设备名称）
    /// session 列表的 watch channel（IPC 推送用）
    session_tx: watch::Sender<Vec<SessionSummary>>,
    session_watch: watch::Receiver<Vec<SessionSummary>>,
    device_tx: watch::Sender<HashMap<String, Device>>,

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
        let (notif_broadcast_tx, _) = broadcast::channel(64);
        let (device_tx, device_watch) = watch::channel(HashMap::new());
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

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

        // 启动设备刷新后台任务（移走 device_tx，Core 保留 clone 以更新名称）
        let core_device_tx = device_tx.clone();
        crate::device_refresh::spawn(device_tx, token.clone());

        let (device_name_tx, device_name_rx) = mpsc::channel::<(String, String)>(32);

        let (session_tx, session_watch) = watch::channel(Vec::new());

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
            notif_broadcast: notif_broadcast_tx,
            sessions: HashMap::new(),
            session_configs: HashMap::new(),
            pending_serials: HashMap::new(),
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            devices_by_id: HashMap::new(),
            port_counter: 27183,
            device_name_tx,
            token,
            device_name_rx,
            clipboard_last_seen: None,
            last_received_from_phone: None,
            last_clipboard_error_print: Instant::now(),
            session_tx,
            session_watch,
            device_tx: core_device_tx,
        }
    }

    pub fn get_token(&self) -> CancellationToken {
        self.token.clone()
    }

    // ─── 主循环 ──────────────────────────────────────────

    pub async fn run(&mut self) {
        // 取出内部命令通道（仅能调用一次）
        let mut cmd_rx = self.cmd_rx
            .take()
            .expect("Core::run can only be called once");
        if let Ok(server) = crate::ipc::server::IpcServer::bind().await {
            let token = self.token.clone();
            crate::ipc::server::serve(
                server,
                token,
                self.cmd_tx.clone(),
                self.device_watch.clone(),
                self.session_watch.clone(),
                self.clip_broadcast.clone(),
                self.notif_broadcast.clone(),
                self.pair_info.clone(),
            );
        } else {
            eprintln!("IPC server failed to bind, continuing without IPC");
        }

        let mut tick = tokio::time::interval(Duration::from_millis(150));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = self.token.cancelled() => break,
                Some(ev)   = self.mdns_rx.recv()       => self.handle_mdns_event(ev).await,
                Some(text) = self.phone_clip_rx.recv()  => self.on_phone_clipboard(text),
                Some(n)    = self.notif_rx.recv()       => self.on_notification(n),
                Some(cmd)  = cmd_rx.recv()              => self.on_command(cmd).await,
                Some((serial, name)) = self.device_name_rx.recv() => {
                    // 更新 devices_by_id
                    if let Some(device) = self.devices_by_id.get_mut(&serial) {
                        device.name = name.clone();
                    }
                    // 也更新 device_tx 用于 IPC
                    self.device_tx.send_modify(|devices| {
                        if let Some(device) = devices.get_mut(&serial) {
                            device.name = name;
                        }
                    });
                }
                _ = tick.tick() => {
                    self.poll_system_clipboard();
                    let from_track = self.device_watch.borrow_and_update().clone();
                    // 将 device_refresh 上报的设备合并到 devices_by_id
                    // 并清理已连接的 pending/discovered 条目
                    for (serial, dev) in &from_track {
                        let kind = DeviceAddrKind::classify(serial);

                        // ── 判定归属（Step 1 → Step 2 → Step 3）──
                        let device_id = if kind == DeviceAddrKind::Usb {
                            // USB serial 即为 DeviceId
                            serial.clone()
                        } else if let Some(matched) = self
                            .devices_by_id
                            .values()
                            .find(|d| d.identity.contains_addr(serial))
                        {
                            // Step 1: Identity 命中
                            matched.id.clone()
                        } else if dev.state == DeviceState::Device {
                            // Step 2: 设备已连接 → 调 get-serialno 解析真实 DeviceId
                            match self.adb_cmd.get_serialno(serial) {
                                Ok(real_id) => {
                                    println!("  get-serialno {} -> {}", serial, real_id);
                                    real_id
                                }
                                Err(_) => {
                                    // Step 3: 解析失败 → 临时条目（以地址为 DeviceId）
                                    serial.clone()
                                }
                            }
                        } else {
                            // 未连接 → 无法解析，临时条目
                            serial.clone()
                        };

                        let mut identity = DeviceIdentity::default();
                        identity.set_by_kind(serial.clone(), kind);

                        if let Some(existing) = self.devices_by_id.get_mut(&device_id) {
                            // 归入已有设备
                            existing.identity.merge(&identity);
                            if kind.priority()
                                > DeviceAddrKind::classify(&existing.serial).priority()
                                // mDNS fullname（含 ._tcp）不能作为 ADB target，不提升
                                && !serial.contains("._tcp")
                            {
                                println!(
                                    "Device {} serial promoted: {} -> {}",
                                    existing.id, existing.serial, serial
                                );
                                existing.serial = serial.clone();
                            }
                        } else {
                            self.devices_by_id.insert(device_id.clone(), Device {
                                id: device_id.clone(),
                                serial: serial.clone(),
                                state: dev.state.clone(),
                                name: String::new(),
                                identity,
                            });
                        }

                        // 清理 pending_serials（尝试原始 serial 和解析后 device_id）
                        if dev.state == DeviceState::Device {
                            self.pending_serials.remove(serial);
                            self.pending_serials.remove(&device_id);
                        }
                    }
                    self.try_connect_pending(&from_track);
                    // 用 devices_by_id 的值同步到 device_tx
                    let watch_devices: HashMap<String, Device> = self.devices_by_id
                        .values()
                        .map(|d| (d.serial.clone(), d.clone()))
                        .collect();
                    // 注意：这个不会产生无限循环，因为 device_tx 的改变不会回传到 device_watch (watch 是广播)
                    let _ = self.device_tx.send(watch_devices);
                    // sync_sessions 使用 devices_by_id 的值
                    let current_devices: HashMap<String, Device> = self.devices_by_id
                        .values()
                        .map(|d| (d.serial.clone(), d.clone()))
                        .collect();
                    self.sync_sessions(current_devices).await;
                    // 推送 session 列表给 IPC
                    self.push_session_list();
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
            wireless_pair::MdnsEvent::ConnectDiscovered { host, port, fullname } => {
                let addr = format!("{}:{}", host, port);
                // fullname 格式: "<serial>._adb-tls-connect._tcp.local."
                let device_id = fullname.split('.').next().unwrap_or(&fullname).to_string();
                println!(
                    "[CONNECT] Discovered {} at {} (serial={})",
                    fullname, addr, device_id
                );

                // mDNS 发现的地址始终标记为 Mdns
                let kind = DeviceAddrKind::Mdns;
                let mut identity = DeviceIdentity::default();
                identity.set_by_kind(addr.clone(), kind);

                // Merge into devices_by_id
                if let Some(existing) = self.devices_by_id.get_mut(&device_id) {
                    existing.identity.merge(&identity);
                    existing.state = DeviceState::Device;
                    if kind.priority()
                        > DeviceAddrKind::classify(&existing.serial).priority()
                    {
                        existing.serial = addr.clone();
                    }
                } else {
                    self.devices_by_id.insert(device_id.clone(), Device {
                        id: device_id.clone(),
                        serial: addr.clone(),
                        state: DeviceState::Device,
                        name: String::new(),
                        identity,
                    });
                }
                self.sync_device_watch();

                // Add to pending_serials
                self.pending_serials
                    .entry(device_id.clone())
                    .or_insert_with(|| PendingDevice::new(addr.clone(), kind))
                    .set_addr(addr.clone(), kind);

                // Step 3: 合并已有临时条目（DeviceId 为 IP:port 且 identity 含相同地址）
                let temp_ids: Vec<String> = self
                    .devices_by_id
                    .iter()
                    .filter(|(id, d)| {
                        DeviceAddrKind::classify(id) == DeviceAddrKind::Ip
                            && d.identity.contains_addr(&addr)
                    })
                    .map(|(id, _)| id.clone())
                    .collect();

                for temp_id in &temp_ids {
                    if *temp_id == device_id {
                        continue;
                    }
                    if let Some(temp) = self.devices_by_id.remove(temp_id) {
                        println!(
                            "[MERGE] Merging temp entry {} into {}",
                            temp_id, device_id
                        );
                        if let Some(auth) = self.devices_by_id.get_mut(&device_id) {
                            auth.identity.merge(&temp.identity);
                        }
                        // 合并 pending_serials 地址层
                        if let Some(temp_pending) = self.pending_serials.remove(temp_id) {
                            let entry = self
                                .pending_serials
                                .entry(device_id.clone())
                                .or_insert_with(|| {
                                    PendingDevice::new(addr.clone(), DeviceAddrKind::Mdns)
                                });
                            if let Some(a) = temp_pending.mdns_addr {
                                entry.set_addr(a, DeviceAddrKind::Mdns);
                            }
                            if let Some(a) = temp_pending.ip_addr {
                                entry.set_addr(a, DeviceAddrKind::Ip);
                            }
                            if let Some(a) = temp_pending.usb_addr {
                                entry.set_addr(a, DeviceAddrKind::Usb);
                            }
                        }
                    }
                }
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

    async fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Connect(addr) => {
                let kind = DeviceAddrKind::classify(&addr);
                if kind == DeviceAddrKind::Usb {
                    // USB serial is the DeviceId, add to pending_serials directly
                    let device_id = addr.clone();
                    self.pending_serials
                        .entry(device_id)
                        .or_insert_with(|| PendingDevice::new(addr, kind));
                } else if let Some(device) = self
                    .devices_by_id
                    .values()
                    .find(|d| d.identity.contains_addr(&addr))
                {
                    // Known device: add address tier to existing pending entry
                    self.pending_serials
                        .entry(device.id.clone())
                        .or_insert_with(|| PendingDevice::new(addr.clone(), kind))
                        .set_addr(addr, kind);
                // Unknown network address: no-op, only mDNS connection supported
                }
            }
            Command::Disconnect(serial) => {
                // Try direct session lookup by serial (current address)
                let found = if let Some(mut handle) = self.sessions.remove(&serial) {
                    handle.stop(self.adb_cmd.as_ref()).await;
                    true
                } else {
                    false
                };
                // Fallback: look up by DeviceId or identity
                if !found {
                    let key = self
                        .sessions
                        .iter()
                        .find(|(_, h)| {
                            h.device.id == serial || h.device.identity.contains_addr(&serial)
                        })
                        .map(|(k, _)| k.clone());
                    if let Some(key) = key {
                        if let Some(mut handle) = self.sessions.remove(&key) {
                            handle.stop(self.adb_cmd.as_ref()).await;
                        }
                    }
                }
                self.pending_serials.remove(&serial);
                self.session_configs.remove(&serial);
            }
            Command::UpdateConfig {
                serial,
                clipboard_sync,
                notification_sync,
                audio_sync,
            } => {
                // 先写配置记录，确保 start_session（音频重启时）读到最新值
                self.session_configs.insert(
                    serial.clone(),
                    session::SessionConfig {
                        clipboard_sync,
                        notification_sync,
                        audio_enabled: audio_sync,
                    },
                );
                // 剪贴板和通知可直接切换原子标志
                if let Some(handle) = self.sessions.get(&serial) {
                    handle
                        .clipboard_enabled
                        .store(clipboard_sync, Ordering::SeqCst);
                    handle
                        .notification_enabled
                        .store(notification_sync, Ordering::SeqCst);
                    let prev_audio = handle.audio_enabled.load(Ordering::SeqCst);
                    if prev_audio != audio_sync {
                        // 音频切换需要重启 scrcpy → 重启 session
                        handle.audio_enabled.store(audio_sync, Ordering::SeqCst);
                        let device_opt = self.device_watch.borrow().get(&serial).cloned();
                        if let Some(mut handle) = self.sessions.remove(&serial) {
                            handle.stop(self.adb_cmd.as_ref()).await;
                            println!(
                                "Audio {} for {}, restarting session",
                                if audio_sync { "enabled" } else { "disabled" },
                                serial
                            );
                            if let Some(device) = device_opt {
                                self.start_session(device).await;
                            }
                        }
                    }
                }
            }
        }
    }

    fn on_notification(&mut self, notif: NotifInfo) {
        if let Some(title) = &notif.title {
            println!("通知: [{}] {}", notif.package, title);
        } else {
            println!("通知: [{}]", notif.package);
        }
        notification::show_desktop_notification(&notif);
        let _ = self.notif_broadcast.send(notif);
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

    /// 连接已在 `devices`（adb track-devices）中的设备自动从待处理队列移除。
    /// 30 秒超时的 pending 设备自动丢弃。
    /// 每设备 5 秒内不会重复尝试连接。

    fn try_connect_pending(&mut self, devices: &HashMap<String, Device>) {
        let now = Instant::now();

        // ── Process pending_serials（已知 DeviceId）──

        // Clean up 30s timeout entries
        let timed_out: Vec<DeviceId> = self
            .pending_serials
            .iter()
            .filter(|(_, pd)| now.duration_since(pd.added_at) > Duration::from_secs(30))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &timed_out {
            self.pending_serials.remove(id);
        }

        let ids: Vec<DeviceId> = self.pending_serials.keys().cloned().collect();
        for device_id in &ids {
            let pending = match self.pending_serials.get(device_id) {
                Some(p) => p.clone(),
                None => continue,
            };

            // Check if any known address is already connected (in track-devices)
            let is_connected = pending
                .usb_addr
                .as_ref()
                .map_or(false, |a| devices.contains_key(a))
                || pending
                    .mdns_addr
                    .as_ref()
                    .map_or(false, |a| devices.contains_key(a))
                || pending
                    .ip_addr
                    .as_ref()
                    .map_or(false, |a| devices.contains_key(a));

            if is_connected {
                self.pending_serials.remove(device_id);
                continue;
            }

            // Per-device 5s cooldown
            if let Some(last) = pending.last_attempt {
                if now.duration_since(last) < Duration::from_secs(5) {
                    continue;
                }
            }

            // Try best connectable address: mDNS > IP (USB is auto-connected)
            if let Some(best_addr) = pending.best_connect_addr() {
                match self.adb_cmd.connect(best_addr) {
                    Ok(()) => {
                        println!("  connect {}: connected", best_addr);
                        if let Some(p) = self.pending_serials.get_mut(device_id) {
                            p.last_attempt = None;
                        }
                    }
                    Err(e) => {
                        eprintln!("  connect {} failed: {}", best_addr, e);
                        if let Some(p) = self.pending_serials.get_mut(device_id) {
                            p.last_attempt = Some(now);
                        }
                    }
                }
            }
        }
    }

    async fn sync_sessions(&mut self, devices: HashMap<String, Device>) {
        for (serial, device) in &devices {
            if self.sessions.contains_key(serial) {
                continue;
            }

            // 检查是否有相同 DeviceId 的 session（serial 可能因优先级变化而改变）
            if let Some(old_serial) = self
                .sessions
                .iter()
                .find(|(_, h)| h.device.id == device.id)
                .map(|(s, _)| s.clone())
            {
                // serial 变了，rekey session 而不是启动新的
                let handle = self.sessions.remove(&old_serial).unwrap();
                self.sessions.insert(serial.clone(), handle);
                println!("Session rekeyed: {} -> {}", old_serial, serial);
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

    /// 推送当前 session 列表到 `session_tx`，IPC 会将其转发给 UI。
    fn push_session_list(&self) {
        use std::sync::atomic::Ordering;
        let sessions: Vec<SessionSummary> = self
            .sessions
            .values()
            .map(|h| SessionSummary {
                id: h.device.id.clone(),
                serial: h.device.serial.clone(),
                name: h.device.name.clone(),
                state: format!("{:?}", h.device.state),
                audio_enabled: h.audio_enabled.load(Ordering::SeqCst),
            })
            .collect();
        let _ = self.session_tx.send(sessions);
    }

    async fn start_session(&mut self, device: Device) {

        // 跳过 IP:port 格式标识的设备（没有有效 serial，session 无法正常工作）
        if DeviceAddrKind::classify(&device.id) == DeviceAddrKind::Ip {
            println!("Skip starting session for {} (IP:port identifier)", device.id);
            return;
        }
        // Guard: 相同 DeviceId 的 session 已在运行则不重复启动

        if self.sessions.iter().any(|(_, h)| h.device.id == device.id) {
            println!(
                "Session already running for device {}, skipping start_session",
                device.id
            );
            return;
        }
        let port = self.port_counter;
        self.port_counter += 1;

        let (stop_tx, stop_rx) = oneshot::channel();
        let clip_sub = self.clip_broadcast.subscribe();
        let phone_clip_tx = self.phone_clip_tx.clone();
        let notif_tx = self.notif_tx.clone();
        let adb = self.adb_cmd.clone();
        let jar_path = self.jar_path.clone();

        // 从配置记录中读取开关状态，不存在时使用默认值
        let cfg = self
            .session_configs
            .get(&device.serial)
            .copied()
            .unwrap_or_default();

        // 共享原子标志，Core 后续可随时切换（音频除外，需要重启 session）
        let clipboard_enabled = Arc::new(AtomicBool::new(cfg.clipboard_sync));
        let notification_enabled = Arc::new(AtomicBool::new(cfg.notification_sync));
        let audio_enabled = Arc::new(AtomicBool::new(cfg.audio_enabled));

        let mut sess = session::Session::new(
            adb,
            device.clone(),
            jar_path,
            port,
            clip_sub,
            phone_clip_tx,
            notif_tx,
            stop_rx,
            clipboard_enabled.clone(),
            notification_enabled.clone(),
            audio_enabled.clone(),
            self.device_name_tx.clone(),
        );

        let task = tokio::spawn(async move { sess.run().await });

        self.sessions.insert(
            device.serial.clone(),
            session::Handle {
                device,
                port,
                stop_tx: Some(stop_tx),
                task,
                clipboard_enabled,
                notification_enabled,
                audio_enabled,
            },
        );
    }

    async fn stop_all(&mut self) {
        self.token.cancel();
        println!("Stopping all sessions...");
        //输出Session列表
        for (serial, handle) in &self.sessions {
            println!("Stopping session for {} on port {}", serial, handle.port);
        }
        for (_, mut handle) in self.sessions.drain() {
            handle.stop(self.adb_cmd.as_ref()).await;
        }
    }

    /// 将 devices_by_id 同步到 device_tx（IPC 使用的 watch channel）
    fn sync_device_watch(&self) {
        let watch_devices: HashMap<String, Device> = self
            .devices_by_id
            .values()
            .map(|d| (d.serial.clone(), d.clone()))
            .collect();
        let _ = self.device_tx.send(watch_devices);
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
                id: "ok".into(),
                serial: "ok".into(),
                state: DeviceState::Device,
                name: String::new(),
                identity: DeviceIdentity::default(),
            },
            Device {
                id: "off".into(),
                serial: "off".into(),
                state: DeviceState::Offline,
                name: String::new(),
                identity: DeviceIdentity::default(),
            },
            Device {
                id: "unauth".into(),
                serial: "unauth".into(),
                state: DeviceState::Unauthorized,
                name: String::new(),
                identity: DeviceIdentity::default(),
            },
            Device {
                id: "ok2".into(),
                serial: "ok2".into(),
                state: DeviceState::Device,
                name: String::new(),
                identity: DeviceIdentity::default(),
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
        let (_session_tx, session_watch) = watch::channel(Vec::new());
        let (session_tx, _) = watch::channel(Vec::new());

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
            notif_broadcast: broadcast::channel(64).0,
            cmd_tx: mpsc::channel(32).0,
            cmd_rx: Some(mpsc::channel(32).1),
            sessions: HashMap::new(),
            session_configs: HashMap::new(),
            pending_serials: HashMap::new(),
            devices_by_id: HashMap::new(),
            port_counter: 27183,
            token: CancellationToken::new(),
            device_name_rx: mpsc::channel::<(String, String)>(32).1,
            device_name_tx: mpsc::channel::<(String, String)>(32).0,
            session_tx,
            clipboard_last_seen: None,
            session_watch,
            device_tx: watch::channel(HashMap::new()).0,
            last_received_from_phone: None,
            last_clipboard_error_print: Instant::now(),
        };

        let addr = "192.168.1.100:5555";
        let serial = "R58N1234567";
        // Add device to pending_serials with serial as DeviceId
        core.pending_serials.insert(
            serial.into(),
            PendingDevice::new(addr.into(), DeviceAddrKind::Mdns),
        );

        // devices (track-devices) 以地址为 key
        let devices = HashMap::from([(
            addr.into(),
            Device {
                id: serial.into(),
                serial: addr.into(),
                state: DeviceState::Device,
                name: String::new(),
                identity: DeviceIdentity::default(),
            },
        )]);

        core.try_connect_pending(&devices);
        // pending_serials 应被清空（设备已连上，地址匹配）
        assert!(core.pending_serials.is_empty());
    }
}
