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

/// 解析 `adb shell cmd notification list` 的输出，提取所有键名。
///
/// 输入示例（每行一个键名）：
/// ```text
/// 0|com.example.app|12345|null|10001
/// 0|com.other.app|67890|null|10002
/// ```
pub fn parse_notification_list(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                None
            } else {
                Some(line.to_string())
            }
        })
        .collect()
}

/// 从 `"类型 (值)"` 或 `"(值)"` 格式中提取括号内的值。
///
/// 例如 `String (测试通知)` → `测试通知`，`Boolean (true)` → `true`。
/// 如果值没有括号包裹则原样返回，括号内为空则返回 None。
fn extract_string_value(line: &str) -> Option<String> {
    // 找最后一个 '='
    let eq_pos = line.find('=')?;
    let after_eq = line[eq_pos + 1..].trim();

    if after_eq.is_empty() {
        return None;
    }

    // 检查末尾是否有 )
    if after_eq.ends_with(')') {
        // 找最内层 '('
        if let Some(start) = after_eq.rfind('(') {
            let inner = after_eq[start + 1..after_eq.len() - 1].trim();
            if inner.is_empty() {
                return None;
            }
            return Some(inner.to_string());
        }
    }

    // 没有括号包裹，返回原始值
    Some(after_eq.to_string())
}

/// 解析 `adb shell "cmd notification get '<key>'"` 的输出。
///
/// 实际输出以 `NotificationRecord(0x...: pkg=...` 开头，`pkg=` 在第一行中间，
/// extras 段包含 `android.title=String (...)`, `android.text=String (...)`` 等。
pub fn parse_notification_detail(output: &str, key: &str) -> Option<NotifInfo> {
    let mut pkg = None;
    let mut title = None;
    let mut body = None;

    for line in output.lines() {
        let line = line.trim();

        // 从整行中找 pkg=...（可能在行首，也可能在 NotificationRecord(...: pkg=...) 中间）
        if pkg.is_none() {
            if let Some(pos) = line.find("pkg=") {
                let val = line[pos + 4..]
                    .split(|c: char| c.is_whitespace())
                    .next()
                    .unwrap_or("");
                if !val.is_empty() {
                    pkg = Some(val.to_string());
                }
            }
        }

        if title.is_none() && line.contains("android.title=") {
            title = extract_string_value(line);
        } else if body.is_none() && line.contains("android.text=") {
            body = extract_string_value(line);
        } else if body.is_none() && line.contains("android.bigText=") {
            body = extract_string_value(line);
        }
    }

    let package = pkg?;
    Some(NotifInfo {
        key: key.to_string(),
        package,
        title,
        body,
    })
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_notification_list_normal() {
        let output = "0|com.example.app|12345|null|10001
0|com.other.app|67890|null|10002
";
        let keys = parse_notification_list(output);
        assert_eq!(keys.len(), 2);
        assert!(keys.contains("0|com.example.app|12345|null|10001"));
        assert!(keys.contains("0|com.other.app|67890|null|10002"));
    }

    #[test]
    fn test_parse_notification_list_empty() {
        let output = "";
        let keys = parse_notification_list(output);
        assert!(keys.is_empty());
    }

    #[test]
    fn test_parse_notification_list_all_lines_as_keys() {
        // 新格式：每行非空行都是键名
        let output = "0|com.a|1|null|100\n0|com.b|2|null|101\n";
        let keys = parse_notification_list(output);
        assert_eq!(keys.len(), 2);
        assert!(keys.contains("0|com.a|1|null|100"));
        assert!(keys.contains("0|com.b|2|null|101"));
    }

    #[test]
    fn test_parse_notification_detail_full() {
        let output = "pkg=com.example.app
opts=0
key=0|com.example.app|12345|null|10001
extras={
    android.title=String (My Title)
    android.text=String (My text content)
}
";
        let info = parse_notification_detail(output, "0|com.example.app|12345|null|10001");
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.package, "com.example.app");
        assert_eq!(info.title.as_deref(), Some("My Title"));
        assert_eq!(info.body.as_deref(), Some("My text content"));
    }

    #[test]
    fn test_parse_notification_detail_missing_fields() {
        let output =
            "NotificationRecord(0x123: pkg=com.example.app user=UserHandle{0} id=999 tag=null)
  extras={
    android.title=String (Title Only)
  }
";
        let info = parse_notification_detail(output, "0|com.example.app|999|null|10001");
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.package, "com.example.app");
        assert_eq!(info.title.as_deref(), Some("Title Only"));
        assert!(info.body.is_none());
    }

    #[test]
    fn test_parse_notification_detail_fallback_bigtext() {
        let output = "pkg=com.example.app
opts=0
key=0|com.example.app|12345|null|10001
extras={
    android.title=String (Title)
    android.bigText=String (Long body text here)
}
";
        let info = parse_notification_detail(output, "0|com.example.app|12345|null|10001");
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.package, "com.example.app");
        assert_eq!(info.title.as_deref(), Some("Title"));
        assert_eq!(info.body.as_deref(), Some("Long body text here"));
    }

    #[test]
    fn test_parse_notification_detail_actual_format() {
        // 真实输出格式：第一行 pkg= 在 NotificationRecord(...:) 中间
        let output = "NotificationRecord(0x07401e30: pkg=com.fantasky.sync.phone.server user=UserHandle{0} id=999 tag=null importance=3 key=0|com.fantasky.sync.phone.server|999|null|10470: ...)
  extras={
        android.title=String (测试通知)
        android.text=String (这条通知来自测试按钮)
  }
";
        let info =
            parse_notification_detail(output, "0|com.fantasky.sync.phone.server|999|null|10470");
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.package, "com.fantasky.sync.phone.server");
        assert_eq!(info.title.as_deref(), Some("测试通知"));
        assert_eq!(info.body.as_deref(), Some("这条通知来自测试按钮"));
    }

    #[test]
    fn test_parse_notification_detail_no_package() {
        let output = "opts=0\nkey=0|key|null|10001\nextras={}\n";
        let info = parse_notification_detail(output, "0|key|null|10001");
        assert!(info.is_none());
    }

    #[test]
    fn test_extract_string_value_normal() {
        let result = extract_string_value("    android.title=String (Hello World)");
        assert_eq!(result.as_deref(), Some("Hello World"));
    }

    #[test]
    fn test_extract_string_value_no_marker() {
        // 没有括号的值也正常返回
        let result = extract_string_value("android.title=null");
        assert_eq!(result.as_deref(), Some("null"));
    }

    #[test]
    fn test_extract_string_value_empty() {
        // 空括号内容返回 None
        let result = extract_string_value("android.title=String ()");
        assert!(result.is_none());
    }
}
