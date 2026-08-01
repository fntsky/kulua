use crate::adb_cmd::{AdbCmd, AdbOps};
use crate::ipc::types::SessionSummary;
use crate::notification::{self, NotifInfo};
use crate::session;
use crate::session::{SESSION_STATE_CONNECTING, SESSION_STATE_FAILED, SESSION_STATE_STOPPED};
use crate::types::{Device, DeviceAddrKind, DeviceIdentity, DeviceState};
use crate::wireless_pair;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[allow(dead_code)]
#[derive(Debug)]
pub enum Command {
    Connect(String),
    Disconnect(String),
    /// 重试 failed 墓碑 session（UI 重试按钮）
    Retry(Uuid),
    UpdateConfig {
        uuid: Uuid,
        clipboard_sync: bool,
        notification_sync: bool,
        audio_sync: bool,
        volume: u16,
    },
}

/// 设备条目，合并设备信息、可选的 session 及其配置。
/// 一个 Device 至多对应一个 session。
struct DeviceEntry {
    device: Device,
    session: Option<session::Handle>,
    config: session::SessionConfig,
}

impl DeviceEntry {
    fn new(device: Device) -> Self {
        Self {
            config: session::SessionConfig::default(),
            device,
            session: None,
        }
    }
}

/// 待连接地址环条目
struct PendingEntry {
    addr: String,
    attempts: u8,
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
    /// 规范设备索引（主键 = UUID，权威数据源）
    devices: HashMap<Uuid, DeviceEntry>,
    /// 待连接地址环（round-robin，失败 5 次自动丢弃）
    pending_serials: VecDeque<PendingEntry>,
    port_counter: u16,
    token: CancellationToken,
    /// 用于从 session 接收设备名称并更新 device_watch
    device_name_rx: mpsc::Receiver<(String, String)>,
    /// sender 副本，传给每个新 session
    device_name_tx: mpsc::Sender<(String, String)>,
    /// session 列表的 watch channel（IPC 推送用）
    session_tx: watch::Sender<Vec<SessionSummary>>,
    session_watch: watch::Receiver<Vec<SessionSummary>>,
    /// 合并后设备列表（IPC device.list / device.updated 事件用）
    merged_tx: watch::Sender<Vec<Device>>,
    merged_watch: watch::Receiver<Vec<Device>>,

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
        let (merged_tx, merged_watch) = watch::channel(Vec::new());
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

        // 启动设备刷新后台任务（原始 adb 列表，Core 只读，不再写回覆盖）
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
            merged_tx,
            merged_watch,
            clip_broadcast: clip_tx,
            phone_clip_rx,
            phone_clip_tx,
            notif_rx,
            notif_tx,
            notif_broadcast: notif_broadcast_tx,
            devices: HashMap::new(),
            pending_serials: VecDeque::new(),
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            port_counter: 27183,
            device_name_tx,
            token,
            device_name_rx,
            clipboard_last_seen: None,
            last_received_from_phone: None,
            last_clipboard_error_print: Instant::now(),
            session_tx,
            session_watch,
        }
    }

    pub fn get_token(&self) -> CancellationToken {
        self.token.clone()
    }

    // ─── 主循环 ──────────────────────────────────────────

    pub async fn run(&mut self) {
        // 取出内部命令通道（仅能调用一次）
        let mut cmd_rx = self
            .cmd_rx
            .take()
            .expect("Core::run can only be called once");
        if let Ok(server) = crate::ipc::server::IpcServer::bind().await {
            let token = self.token.clone();
            crate::ipc::server::serve(
                server,
                token,
                self.cmd_tx.clone(),
                self.device_watch.clone(),
                self.merged_watch.clone(),
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
                    // 更新 devices 中匹配的设备名称
                    if let Some(entry) = self.devices.values_mut().find(|e| e.device.serial == serial) {
                        entry.device.name = name.clone();
                    }
                }
                _ = tick.tick() => {
                    self.poll_system_clipboard();
                    let from_track = self.device_watch.borrow_and_update().clone();
                    // 原始列表含 Offline/Unauthorized 等，仅 Device 状态进入设备合并（卡片语义不变）
                    let online: HashMap<String, Device> = from_track
                        .iter()
                        .filter(|(_, d)| d.state == DeviceState::Device)
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    for (serial, dev) in &online {
                        self.insert_device(serial, dev.state.clone());
                        if dev.state == DeviceState::Device {
                            self.pending_serials.retain(|e| e.addr != *serial);
                        }
                    }
                    self.try_connect_pending(&online);
                    self.push_merged_devices();
                    self.check_dead_sessions();
                    self.sync_sessions(&online).await;
                    self.push_session_list();
                }
            }
        }

        self.stop_all().await;
    }

    /// 将 adb 上报的设备插入/合并到 `devices` 映射。
    ///
    /// 1. 按地址分类构造 identity
    /// 2. 在现有设备中按 identity 匹配
    /// 3. 未匹配且状态为 Device 时调用 `adb shell getprop ro.serialno` 获取硬件序列号
    /// 4. 按该序列号再匹配一次
    /// 5. 仍未匹配则创建新 Device（UUID 主键）
    ///
    /// 返回该设备条目的 UUID。**对 devices 的写入操作必须经由此函数。**
    fn insert_device(&mut self, serial: &str, state: DeviceState) -> Uuid {
        let kind = DeviceAddrKind::classify(serial);
        let mut identity = DeviceIdentity::default();
        identity.set_by_kind(serial.to_string(), kind);

        // ── Step 1: 按 identity 匹配 ──
        if let Some(entry) = self
            .devices
            .values_mut()
            .find(|e| e.device.identity.contains_addr(serial))
        {
            entry.device.identity.merge(&identity);
            entry.device.state = state;
            if kind.priority() > DeviceAddrKind::classify(&entry.device.serial).priority()
                && !serial.contains("._tcp")
            {
                entry.device.serial = serial.to_string();
            }
            return entry.device.uuid;
        }

        // ── Step 2: 已连接的设备 → 解析真实 DeviceId ──
        let device_id = if state == DeviceState::Device {
            match self
                .adb_cmd
                .run(&["-s", serial, "shell", "getprop", "ro.serialno"])
            {
                Ok(output) => {
                    let real_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !real_id.is_empty() {
                        println!("  getprop ro.serialno {} -> {}", serial, real_id);
                        real_id
                    } else {
                        serial.to_string()
                    }
                }
                Err(_) => serial.to_string(),
            }
        } else {
            serial.to_string()
        };

        // ── Step 3: 按 device_id 再匹配一次 ──
        if let Some(entry) = self.devices.values_mut().find(|e| e.device.id == device_id) {
            entry.device.identity.merge(&identity);
            entry.device.state = state;
            if kind.priority() > DeviceAddrKind::classify(&entry.device.serial).priority()
                && !serial.contains("._tcp")
            {
                entry.device.serial = serial.to_string();
            }
            return entry.device.uuid;
        }

        // ── Step 4: 真正的新设备 ──
        let new = Device {
            uuid: Uuid::new_v4(),
            id: device_id,
            serial: serial.to_string(),
            state,
            name: String::new(),
            identity,
        };
        let uuid = new.uuid;
        self.devices.insert(uuid, DeviceEntry::new(new));
        uuid
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
            wireless_pair::MdnsEvent::ConnectDiscovered {
                host,
                port,
                fullname,
            } => {
                let addr = format!("{}:{}", host, port);
                // fullname 格式: "<serial>._adb-tls-connect._tcp.local."
                let device_id = fullname.split('.').next().unwrap_or(&fullname).to_string();
                println!(
                    "[CONNECT] Discovered {} at {} (serial={})",
                    fullname, addr, device_id
                );
                // 仅加入 pending 环，由 tick 循环负责 connect 后 insert_device
                if !self
                    .pending_serials
                    .iter()
                    .any(|e| e.addr == device_id || e.addr == addr)
                {
                    self.pending_serials
                        .push_back(PendingEntry { addr, attempts: 0 });
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
                // USB 设备自动连接，无需加入 pending 环
                if DeviceAddrKind::classify(&addr) != DeviceAddrKind::Usb {
                    // 去重后加入环尾
                    if !self.pending_serials.iter().any(|e| e.addr == addr) {
                        self.pending_serials
                            .push_back(PendingEntry { addr, attempts: 0 });
                    }
                }
            }
            Command::Disconnect(serial) => {
                // 寻找匹配设备并停止 session
                let found_handle = self.devices.values_mut().find_map(|entry| {
                    if entry.device.serial == serial
                        || entry.device.id == serial
                        || entry.device.identity.contains_addr(&serial)
                    {
                        entry.session.take()
                    } else {
                        None
                    }
                });

                if let Some(mut handle) = found_handle {
                    handle.stop(self.adb_cmd.as_ref()).await;
                }

                // 清理 pending_serials 中的对应地址
                self.pending_serials.retain(|e| e.addr != serial);
                // 放弃 Disconnect 中对 session_configs 的清理（config 随 DeviceEntry 保留）
                }
            Command::Retry(uuid) => {
                // 仅 failed 墓碑可重试；活跃 session / 无墓碑 → no-op
                let has_tombstone = self.devices.get(&uuid).is_some_and(|entry| {
                    entry.session.as_ref().is_some_and(|h| h.is_failed())
                });
                if !has_tombstone {
                    return;
                }
                // 拔墓碑（task 已结束，stop 立即返回并清理 adb forward）
                let (mut handle, device) = match self.devices.get_mut(&uuid).map(|entry| {
                    (entry.session.take(), entry.device.clone())
                }) {
                    Some((Some(h), d)) => (h, d),
                    _ => return,
                };
                handle.stop(self.adb_cmd.as_ref()).await;
                // 设备已不在 adb 列表 → 等重连后 sync_sessions 自动恢复
                let online = self
                    .device_watch
                    .borrow()
                    .get(&device.serial)
                    .is_some_and(|d| d.state == DeviceState::Device);
                if online {
                    self.start_session(device).await;
                }
            }
            Command::UpdateConfig {
                uuid,
                clipboard_sync,
                notification_sync,
                audio_sync,
                volume,
            } => {
                // 更新配置
                if let Some(entry) = self.devices.get_mut(&uuid) {
                    let prev_audio = entry.config.audio_enabled;
                    entry.config = session::SessionConfig {
                        clipboard_sync,
                        notification_sync,
                        audio_enabled: audio_sync,
                        volume,
                    };

                    // 剪贴板和通知可直接切换原子标志
                    if let Some(handle) = &entry.session {
                        handle
                            .clipboard_enabled
                            .store(clipboard_sync, Ordering::SeqCst);
                        handle
                            .notification_enabled
                            .store(notification_sync, Ordering::SeqCst);
                        // 音量实时生效（无需重启 session）
                        handle.volume.store(volume, Ordering::Relaxed);
                        if prev_audio != audio_sync {
                            handle.audio_enabled.store(audio_sync, Ordering::SeqCst);
                        }
                    }
                }

                // 音频开关变更需要重启 session
                let need_restart = self.devices.get(&uuid).is_some_and(|entry| {
                    entry.session.as_ref().is_some_and(|h| {
                        h.audio_enabled.load(Ordering::SeqCst) != audio_sync
                    })
                });
                if need_restart {
                    let device = self.devices.get(&uuid).unwrap().device.clone();
                    if let Some(mut handle) = self.devices.get_mut(&uuid).unwrap().session.take() {
                        handle.stop(self.adb_cmd.as_ref()).await;
                    }
                    println!(
                        "Audio {} for {}, restarting session",
                        if audio_sync { "enabled" } else { "disabled" },
                        uuid
                    );
                    self.start_session(device).await;
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
                    let session_count = self
                        .devices
                        .values()
                        .filter(|e| e.session.is_some())
                        .count();
                    println!("Clipboard (PC→{} devices): {}", session_count, text);
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
    /// 每 tick 处理环前端一个地址，失败后移回环尾（≤5 次）。

    fn try_connect_pending(&mut self, adb_devices: &HashMap<String, Device>) {
        if let Some(mut entry) = self.pending_serials.pop_front() {
            // 已在 adb 设备列表中 → 连接成功，移除
            if adb_devices.contains_key(&entry.addr) {
                return;
            }

            // USB 不应出现在 pending 中，跳过
            if DeviceAddrKind::classify(&entry.addr) == DeviceAddrKind::Usb {
                return;
            }

            match self.adb_cmd.connect(&entry.addr) {
                Ok(()) => {
                    println!("  connect {}: connected", entry.addr);
                }
                Err(e) => {
                    entry.attempts += 1;
                    if entry.attempts >= 5 {
                        eprintln!(
                            "  connect {} failed after {} attempts, removing",
                            entry.addr, entry.attempts
                        );
                    } else {
                        eprintln!(
                            "  connect {} failed (attempt {}): {}",
                            entry.addr, entry.attempts, e
                        );
                        self.pending_serials.push_back(entry);
                    }
                }
            }
        }
    }

    async fn sync_sessions(&mut self, adb_devices: &HashMap<String, Device>) {
        // 检查每个 device，按地址优先级（USB > mDNS > IP:port）启动 session
        let mut pending: Vec<Uuid> = self
            .devices
            .iter()
            .filter(|(_, entry)| entry.session.is_none())
            .map(|(uuid, _)| *uuid)
            .collect();
        pending.sort_by_key(|uuid| {
            let entry = self.devices.get(uuid).expect("uuid from iteration");
            std::cmp::Reverse(DeviceAddrKind::classify(&entry.device.serial).priority())
        });

        for uuid in pending {
            let device = match self.devices.get(&uuid) {
                Some(entry) => entry.device.clone(),
                None => continue,
            };

            // 仅在 adb 设备列表中且状态为 Device 时启动
            if !adb_devices.contains_key(&device.serial) {
                continue;
            }
            if let Some(adb_dev) = adb_devices.get(&device.serial) {
                if adb_dev.state != DeviceState::Device {
                    continue;
                }
            }

            self.start_session(device).await;
        }

        // 停止已断开设备的 session
        let to_stop: Vec<Uuid> = self
            .devices
            .iter()
            .filter(|(_, entry)| {
                entry.session.is_some() && !adb_devices.contains_key(&entry.device.serial)
            })
            .map(|(uuid, _)| *uuid)
            .collect();

        for uuid in to_stop {
            let (mut handle, serial) = match self.devices.get_mut(&uuid).and_then(|entry| {
                let serial = entry.device.serial.clone();
                entry.session.take().map(|h| (h, serial))
            }) {
                Some(v) => v,
                None => continue,
            };
            println!("Stopped session for {}", serial);
            handle.stop(self.adb_cmd.as_ref()).await;
        }
    }

    /// 检测 session task 异常结束（server 崩溃/panic）→ 强制写 failed 墓碑。
    ///
    /// 墓碑占着 `entry.session` 位：push_session_list 推送继续包含它（UI 可见 failed + 重试按钮），
    /// sync_sessions 因 `session.is_some()` 不自动重启（重启闸门，防崩溃风暴）。
    /// 正常停止由 Core 主动 take handle 触发，不会出现在这里；故 is_finished 且非 stopped 必为异常结束。
    fn check_dead_sessions(&mut self) {
        for (_, entry) in self.devices.iter_mut() {
            if let Some(handle) = &entry.session {
                if handle.task.is_finished() && handle.state() != SESSION_STATE_STOPPED {
                    handle.set_state(SESSION_STATE_FAILED);
                }
            }
        }
    }

    /// 推送当前 session 列表到 `session_tx`，IPC 会将其转发给 UI。
    fn push_session_list(&self) {
        use std::sync::atomic::Ordering;
        let sessions: Vec<SessionSummary> = self
            .devices
            .values()
            .filter_map(|entry| {
                entry.session.as_ref().map(|handle| SessionSummary {
                    uuid: entry.device.uuid,
                    id: entry.device.id.clone(),
                    serial: entry.device.serial.clone(),
                    name: entry.device.name.clone(),
                    state: format!("{:?}", entry.device.state),
                    session_state: handle.state_str().to_string(),
                    audio_buffer_ms: handle.audio_latency.load(Ordering::SeqCst),
                    clipboard_sync: entry.config.clipboard_sync,
                    notification_sync: entry.config.notification_sync,
                    audio_enabled: handle.audio_enabled.load(Ordering::SeqCst),
                    volume: entry.config.volume,
                })
            })
            .collect();
        let _ = self.session_tx.send(sessions);
    }

    async fn start_session(&mut self, device: Device) {
        // Guard: 相同 Device 的 session 已在运行则不重复启动
        if self
            .devices
            .iter()
            .any(|(_, entry)| entry.session.is_some() && entry.device.uuid == device.uuid)
        {
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

        // 从设备条目中读取配置开关
        let cfg = self
            .devices
            .get(&device.uuid)
            .map(|entry| entry.config)
            .unwrap_or_default();
        // 共享原子标志，Core 后续可随时切换
        let clipboard_enabled = Arc::new(AtomicBool::new(cfg.clipboard_sync));
        let notification_enabled = Arc::new(AtomicBool::new(cfg.notification_sync));
        let audio_enabled = Arc::new(AtomicBool::new(cfg.audio_enabled));
        let volume = Arc::new(AtomicU16::new(cfg.volume));
        // 生命周期状态：session 与 Handle 共享，阶段边界写入（docs/session-state-design.md）
        let session_state = Arc::new(AtomicU8::new(SESSION_STATE_CONNECTING));
        // 音频缓冲延迟（ms）：audio_task 写，Core tick 读推 UI
        let audio_latency = Arc::new(AtomicU64::new(0));

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
            volume.clone(),
            self.device_name_tx.clone(),
            session_state.clone(),
            audio_latency.clone(),
        );

        let task = tokio::spawn(async move { sess.run().await });

        let handle = session::Handle {
            device,
            port,
            stop_tx: Some(stop_tx),
            task,
            clipboard_enabled,
            notification_enabled,
            audio_enabled,
            volume,
            session_state,
            audio_latency,
        };

        let uuid = handle.device.uuid;
        if let Some(entry) = self.devices.get_mut(&uuid) {
            entry.session = Some(handle);
        }
    }


    async fn stop_all(&mut self) {
        self.token.cancel();
        println!("Stopping all sessions...");
        // 输出 Session 列表
        for entry in self.devices.values() {
            if let Some(handle) = &entry.session {
                println!(
                    "Stopping session for {} on port {}",
                    entry.device.serial, handle.port
                );
            }
        }
        for (_, entry) in self.devices.drain() {
            if let Some(mut handle) = entry.session {
                handle.stop(self.adb_cmd.as_ref()).await;
            }
        }
    }

    /// 推送合并后设备列表（IPC device.list / device.updated 事件用）
    fn push_merged_devices(&self) {
        let merged: Vec<Device> = self.devices.values().map(|e| e.device.clone()).collect();
        let _ = self.merged_tx.send(merged);
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
                uuid: Uuid::new_v4(),
                id: "ok".into(),
                serial: "ok".into(),
                state: DeviceState::Device,
                name: String::new(),
                identity: DeviceIdentity::default(),
            },
            Device {
                uuid: Uuid::new_v4(),
                id: "off".into(),
                serial: "off".into(),
                state: DeviceState::Offline,
                name: String::new(),
                identity: DeviceIdentity::default(),
            },
            Device {
                uuid: Uuid::new_v4(),
                id: "unauth".into(),
                serial: "unauth".into(),
                state: DeviceState::Unauthorized,
                name: String::new(),
                identity: DeviceIdentity::default(),
            },
            Device {
                uuid: Uuid::new_v4(),
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
            devices: HashMap::new(),
            pending_serials: VecDeque::new(),
            port_counter: 27183,
            token: CancellationToken::new(),
            device_name_rx: mpsc::channel::<(String, String)>(32).1,
            device_name_tx: mpsc::channel::<(String, String)>(32).0,
            session_tx,
            clipboard_last_seen: None,
            session_watch,
            merged_tx: watch::channel(Vec::new()).0,
            merged_watch: watch::channel(Vec::new()).1,
            last_received_from_phone: None,
            last_clipboard_error_print: Instant::now(),
        };

        let addr = "192.168.1.100:5555";
        // Add address to pending_serials ring
        core.pending_serials.push_back(PendingEntry {
            addr: addr.into(),
            attempts: 0,
        });

        // devices (track-devices) 以地址为 key
        let devices = HashMap::from([(
            addr.into(),
            Device {
                uuid: Uuid::new_v4(),
                id: "R58N1234567".into(),
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
