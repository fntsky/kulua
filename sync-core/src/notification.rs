use crate::adb_cmd::AdbOps;
use crate::types::Device;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// 来自手机的一条通知
#[derive(Debug, Clone)]
pub struct NotifInfo {
    /// 通知标识键：0|package|id|null|tag
    #[allow(dead_code)]
    pub key: String,
    /// 发送通知的应用包名
    pub package: String,
    /// 标题 (android.title)
    pub title: Option<String>,
    /// 正文 (android.text 或 android.bigText)
    pub body: Option<String>,
}

pub use crate::protocol::notification::{parse_notification_list, parse_notification_detail};

/// 在 Windows 上显示桌面通知。
pub fn show_desktop_notification(info: &NotifInfo) {
    let title = info.title.as_deref().unwrap_or(&info.package);
    let body = info.body.as_deref().unwrap_or("");

    match notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .appname("PhoneSync")
        .show()
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!("桌面通知显示失败: {}", e);
        }
    }
}

/// 为单个设备启动通知轮询线程。
///
/// 线程每 2 秒执行 `adb shell cmd notification list`，对比前后 key 集合，
/// 对新增的 key 执行 `cmd notification get` 获取详情并通过 channel 发送。
///
/// 首次轮询仅初始化 key 集合，不发送通知（避免历史通知刷屏）。
pub fn spawn_notification_poller(
    adb: Arc<dyn AdbOps>,
    device: Device,
    notif_tx: std_mpsc::Sender<NotifInfo>,
) -> (JoinHandle<()>, Arc<AtomicBool>) {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();

    let handle = std::thread::Builder::new()
        .name(format!("notif-poller-{}", device.serial))
        .spawn(move || {
            let serial = &device.serial;
            let mut old_keys: HashSet<String> = HashSet::new();
            let mut first_poll = true;

            loop {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }

                // 1) 拉取通知列表
                let output = match adb.run(&["-s", serial, "shell", "cmd", "notification", "list"])
                {
                    Ok(out) => out,
                    Err(e) => {
                        eprintln!("通知列表获取失败 ({}): {}", serial, e);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };

                let stdout = String::from_utf8_lossy(&output.stdout);
                let new_keys = parse_notification_list(&stdout);

                // 2) 首次轮询：只初始化，不弹通知
                if first_poll {
                    old_keys = new_keys;
                    first_poll = false;
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }

                // 3) 计算新增 key 并获取详情
                let added: Vec<&str> = new_keys.difference(&old_keys).map(|s| s.as_str()).collect();

                for key in &added {
                    // 用单引号包裹 key，避免 shell 把 | 当成管道
                    let shell_cmd =
                        format!("cmd notification get '{}'", key.replace('\'', "'\\''"));
                    let detail_output = match adb.run(&["-s", serial, "shell", &shell_cmd]) {
                        Ok(out) => out,
                        Err(e) => {
                            eprintln!("通知详情获取失败 ({}): {}", serial, e);
                            continue;
                        }
                    };

                    let detail_stdout = String::from_utf8_lossy(&detail_output.stdout);
                    if let Some(info) = parse_notification_detail(&detail_stdout, key) {
                        if notif_tx.send(info).is_err() {
                            // receiver dropped（Core 退出了）
                            break;
                        }
                    }
                }

                old_keys = new_keys;
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawn notification poller thread");

    (handle, stop)
}

/// 桥接版本：启动 std 线程通知轮询器，通过 bridging 转发到 tokio mpsc channel
pub fn spawn_notification_poller_tokio(
    adb: Arc<dyn AdbOps>,
    device: Device,
    notif_tx: tokio::sync::mpsc::Sender<NotifInfo>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let (bridge_tx, bridge_rx) = std_mpsc::channel::<NotifInfo>();
    let (poller_handle, _) = spawn_notification_poller(adb, device, bridge_tx);
    // 后台转发：std mpsc → tokio mpsc
    tokio::task::spawn_blocking(move || {
        while !stop.load(Ordering::SeqCst) {
            match bridge_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(info) => {
                    if notif_tx.blocking_send(info).is_err() {
                        break;
                    }
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    poller_handle
}

