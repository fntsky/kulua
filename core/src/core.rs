use crate::adb_cmd::{AdbCmd, AdbOps};
use crate::notification;
use crate::session::Session;
use crate::types::{AdbError, Device, DeviceState};
use crate::{
    tray,
    wireless_pair,
};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Core {
    pub adb_cmd: Arc<dyn AdbOps>,
    pub devices: Arc<Mutex<HashMap<String, Device>>>,
    pub sessions: HashMap<String, Session>,
    /// 全局停止信号，任一来源（Ctrl+C / 正常退出）触发
    stop_flag: Arc<AtomicBool>,
    refresh_thread_stop: Option<std::thread::JoinHandle<()>>,
}

impl Core {
    pub fn new() -> Self {
        Self {
            adb_cmd: Arc::new(AdbCmd::new()),
            devices: Arc::new(Mutex::new(HashMap::new())),
            sessions: HashMap::new(),
            stop_flag: Arc::new(AtomicBool::new(false)),
            refresh_thread_stop: None,
        }
    }

    /// 返回停止信号接收端，供外部（主线程 Ctrl+C handler）触发停止
    pub fn get_stop_flag(&self) -> Arc<AtomicBool> {
        self.stop_flag.clone()
    }

    /// 停止所有 session 并等待后台线程结束
    pub fn stop_all(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);

        // 停止所有 session（会 kill adb shell + 移除端口转发）
        let serials: Vec<String> = self.sessions.keys().cloned().collect();
        for serial in &serials {
            if let Some(mut session) = self.sessions.remove(serial) {
                session.stop(self.adb_cmd.as_ref());
                println!("Stopped session for {}", serial);
            }
        }

        // 等待后台刷新线程退出
        if let Some(handle) = self.refresh_thread_stop.take() {
            let _ = handle.join();
        }
    }

    /// 该调用从 core.run() 循环返回的唯一方式是：
    /// 1. mDNS channel 断开（remote end dropped）
    /// 2. 收到外部停止信号（Ctrl+C 或手动调用 stop_all）
    fn should_stop(&self) -> bool {
        self.stop_flag.load(Ordering::SeqCst)
    }

    /// 刷新设备列表（线程安全），由后台刷新线程和测试共用
    #[allow(dead_code)]
    pub fn refresh_devices(&self) {
        let mut map = self.devices.lock().unwrap();
        map.clear();
        if let Ok(list) = self.adb_cmd.devices() {
            for d in list {
                if d.state == DeviceState::Device {
                    map.insert(d.serial.clone(), d);
                }
            }
        }
        // adb 失败时 map 保持清空状态
    }

    pub fn wireless_pair(
        &self,
        info: &wireless_pair::WirelessPairing,
        ip: &str,
        port: u16,
    ) -> Result<(), AdbError> {
        let addr = format!("{}:{}", ip, port);
        self.adb_cmd.wireless_pair(&addr, info)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn connect(&self, ip: &str, port: u16) -> Result<(), AdbError> {
        let addr = format!("{}:{}", ip, port);
        self.adb_cmd.connect(&addr)?;
        Ok(())
    }

    pub fn run(
        &mut self,
        rx: Receiver<wireless_pair::MdnsEvent>,
        pair_info: &wireless_pair::WirelessPairing,
        jar_path: &str,
    ) {
        let jar_path = std::path::Path::new(jar_path);
        if !jar_path.exists() {
            eprintln!("scrcpy-server not found at: {}", jar_path.display());
            return;
        }
        let jar_path = jar_path.to_string_lossy().to_string();

        // 独立线程：每 1s 刷新一次设备列表，不阻塞主循环
        let adb = self.adb_cmd.clone();
        let devices = self.devices.clone();
        let stop = self.stop_flag.clone();
        let handle = std::thread::spawn(move || {
            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let mut map = devices.lock().unwrap();
                map.clear();
                if let Ok(list) = adb.devices() {
                    for d in list {
                        if d.state == DeviceState::Device {
                            map.insert(d.serial.clone(), d);
                        }
                    }
                }
                drop(map);
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        self.refresh_thread_stop = Some(handle);

        let mut clipboard_last_seen: Option<String> = None;
        // 最近一次从手机收到的剪贴板文本，用于防回环
        let mut last_received_from_phone: Option<String> = None;
        let mut last_clipboard_error_print = Instant::now();

        let mut clipboard_port = 27183u16;
        loop {
            if self.should_stop() {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(wireless_pair::MdnsEvent::PairingDiscovered { host, port }) => {
                    println!("Discovered device at {}:{}", host, port);
                    let _ = self.wireless_pair(pair_info, &host, port);
                    // std::thread::sleep(Duration::from_secs(10));
                    // 设备会被后台刷新线程自动发现
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(e) => {
                    eprintln!("Error receiving mDNS event: {}", e);
                    break;
                }
                _ => {}
            }

            // 0. 检查托盘菜单事件（非阻塞）
            if let Some(tray::TrayEvent::Exit) = tray::check_event() {
                println!("Tray: exit requested");
                self.stop_flag.store(true, Ordering::SeqCst);
                break;
            }

            // 1. 收集 Phone→PC 剪贴板事件并写入系统剪贴板
            for session in self.sessions.values_mut() {
                while let Ok(text) = session.phone_clipboard_rx.try_recv() {
                    // 跳过重复（同一台手机连续发来相同文本）
                    if Some(&text) == last_received_from_phone.as_ref() {
                        continue;
                    }
                    last_received_from_phone = Some(text.clone());

                    // 内容与 PC 剪贴板当前内容相同，不重复写入（避免回环）
                    if Some(&text) == clipboard_last_seen.as_ref() {
                        continue;
                    }

                    println!("System clipboard (from phone): {}", text);
                    if let Err(e) = clipboard_win::set_clipboard_string(&text) {
                        eprintln!("Clipboard: failed to write from phone: {}", e);
                    }
                }
            }

            // 2. 收集 Phone→PC 通知事件并显示桌面通知
            for session in self.sessions.values_mut() {
                while let Ok(notif) = session.notification_rx.try_recv() {
                    if let Some(title) = &notif.title {
                        println!("通知: [{}] {}", notif.package, title);
                    } else {
                        println!("通知: [{}]", notif.package);
                    }
                    notification::show_desktop_notification(&notif);
                }
            }

            // 3. 轮询系统剪贴板变化并分发给所有设备
            match clipboard_win::get_clipboard_string() {
                Ok(ref text) if Some(text.as_str()) != clipboard_last_seen.as_deref() => {
                    clipboard_last_seen = Some(text.clone());
                    last_clipboard_error_print = Instant::now();
                    // 只分发不是从手机收来的内容（防回环）
                    if Some(text) != last_received_from_phone.as_ref() {
                        println!(
                            "Clipboard (PC→{} devices): {}",
                            self.sessions.len(),
                            text
                        );
                        for session in self.sessions.values() {
                            if let Err(e) = session.write_clipboard(text) {
                                eprintln!(
                                    "Clipboard: failed to write to {}: {:?}",
                                    session.device.serial, e
                                );
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    if last_clipboard_error_print.elapsed() >= Duration::from_secs(30) {
                        eprintln!("Clipboard: failed to read system clipboard: {}", e);
                        last_clipboard_error_print = Instant::now();
                    }
                }
            }

            // 为新设备启动 session
            let serials: Vec<String> = {
                let map = self.devices.lock().unwrap();
                map.keys().cloned().collect()
            };
            for serial in &serials {
                if !self.sessions.contains_key(serial) {
                    let device = {
                        let map = self.devices.lock().unwrap();
                        map.get(serial).cloned()
                    };
                    if let Some(device) = device {
                        self.start_session(&device, &mut clipboard_port, &jar_path);
                    }
                }
            }

            // 清理已断开设备的 session
            let session_serials: Vec<String> = self.sessions.keys().cloned().collect();
            for serial in &session_serials {
                let still_present = {
                    let map = self.devices.lock().unwrap();
                    map.contains_key(serial)
                };
                if !still_present {
                    if let Some(mut session) = self.sessions.remove(serial) {
                        session.stop(self.adb_cmd.as_ref());
                        println!("Stopped session for {}", serial);
                    }
                }
            }
        }
    }

    fn start_session(&mut self, device: &Device, port: &mut u16, jar_path: &str) {
        let mut session = Session::new(device.clone());
        match session.start_clipboard_service(self.adb_cmd.as_ref(), jar_path, *port) {
            Ok(()) => {
                println!("Started clipboard service for {}", device.serial);
                *port += 1;
            }
            Err(e) => {
                eprintln!("Failed to start clipboard for {}: {:?}", device.serial, e);
            }
        }
        // 无论剪贴板服务是否成功，都启动通知轮询（不依赖 scrcpy）
        session.start_notification_polling(self.adb_cmd.clone());
        self.sessions.insert(device.serial.clone(), session);
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb_cmd::mock::MockAdb;
    use crate::types::DeviceState;

    fn make_core(adb: MockAdb) -> Core {
        Core {
            adb_cmd: Arc::new(adb),
            devices: Arc::new(Mutex::new(HashMap::new())),
            sessions: HashMap::new(),
            stop_flag: Arc::new(AtomicBool::new(false)),
            refresh_thread_stop: None,
        }
    }

    #[test]
    fn test_refresh_devices_filters_device_state() {
        let mut mock = MockAdb::new();
        mock.devices_result = vec![
            Device {
                serial: "device1".into(),
                state: DeviceState::Device,
            },
            Device {
                serial: "offline1".into(),
                state: DeviceState::Offline,
            },
            Device {
                serial: "unauth1".into(),
                state: DeviceState::Unauthorized,
            },
            Device {
                serial: "device2".into(),
                state: DeviceState::Device,
            },
        ];
        let core = make_core(mock);
        core.refresh_devices();
        let map = core.devices.lock().unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("device1"));
        assert!(map.contains_key("device2"));
        assert!(!map.contains_key("offline1"));
    }

    #[test]
    fn test_refresh_devices_empty() {
        let mock = MockAdb::new();
        let core = make_core(mock);
        core.refresh_devices();
        let map = core.devices.lock().unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn test_refresh_devices_adb_error() {
        let mut mock = MockAdb::new();
        mock.fail_devices = true;
        let devices = Arc::new(Mutex::new(HashMap::new()));
        devices.lock().unwrap().insert(
            "stale".into(),
            Device {
                serial: "stale".into(),
                state: DeviceState::Device,
            },
        );
        let core = Core {
            adb_cmd: Arc::new(mock),
            devices,
            sessions: HashMap::new(),
            stop_flag: Arc::new(AtomicBool::new(false)),
            refresh_thread_stop: None,
        };
        core.refresh_devices();
        let map = core.devices.lock().unwrap();
        assert!(
            map.is_empty(),
            "on adb error, devices map should be cleared, got {} items",
            map.len()
        );
    }
}
