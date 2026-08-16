//! 设备应用枚举（kulua-server 一次性模式 `list_apps=true`）。
//!
//! 不依赖 `scrcpy.exe --list-apps`（每次都要初始化 SDL + push jar，更重）：
//! 直接复用已有的 adb + jar 部署逻辑，`adb shell` 启动 server 一次性模式
//! 打印可启动应用列表后退出，结果带 60s TTL 缓存（按设备 UUID）。
//! kulua-server 的 AppLister 输出格式与官方 scrcpy 一致（30 列补位）。

use crate::adb_cmd::AdbOps;
use crate::types::{AdbError, Device};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// 远程 jar 路径（与 session 部署共用，保证只 push 一次）。
pub const REMOTE_JAR: &str = "/data/local/tmp/kulua-server.jar";

/// 应用列表缓存有效期。
pub const APP_CACHE_TTL: Duration = Duration::from_secs(60);

/// 枚举应用超时（server 首次冷启动 + 拉取包列表可能较慢）。
pub const LIST_APPS_TIMEOUT: Duration = Duration::from_secs(20);

/// 一个可启动应用。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppInfo {
    pub package_name: String,
    /// 应用名（可能为空，解析失败时退回包名）
    pub label: String,
    /// true = 系统应用（server 输出 `*` 前缀），false = 普通应用（`-` 前缀）
    pub system: bool,
}

/// 解析 kulua-server `list_apps=true` 的一次性输出。
///
/// 输出格式（AppLister.java，对照官方 LogUtils.buildAppListMessage）：
/// ```text
/// List of apps:
///  * Settings                             com.android.settings
///  - Google Chrome                        com.android.chrome
/// ```
/// - ` * ` / ` - ` 前缀行 = 系统 / 普通应用
/// - 名称列固定补位到 30 列（按 Java 字符串长度），补位后跟 ` <包名>`
/// - 名称超过 30 字符时包名换行输出（`    <30空格> <包名>`），即非标记行 = 续行
pub fn parse_apps_output(output: &str) -> Vec<AppInfo> {
    let mut apps: Vec<AppInfo> = Vec::new();
    let mut in_list = false;
    // 上一个未收尾的应用（长名续行场景）
    let mut current: Option<AppInfo> = None;

    for raw_line in output.lines() {
        let line = raw_line.trim_end_matches('\r');
        if !in_list {
            if line.contains("List of apps:") {
                in_list = true;
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(" * ") {
            finish_app(&mut apps, &mut current);
            current = Some(AppInfo {
                system: true,
                ..Default::default()
            });
            parse_app_line(&mut current, rest);
        } else if let Some(rest) = line.strip_prefix(" - ") {
            finish_app(&mut apps, &mut current);
            current = Some(AppInfo {
                system: false,
                ..Default::default()
            });
            parse_app_line(&mut current, rest);
        } else if let Some(app) = &mut current {
            // 长名应用的包名续行（非标记行）
            if app.package_name.is_empty() {
                app.package_name = line.trim().to_string();
            }
        }
    }
    finish_app(&mut apps, &mut current);
    apps
}

/// 解析单行应用条目：前 30 列（按字符计，近似 Java 的 UTF-16 长度）为补位后的名称。
fn parse_app_line(app: &mut Option<AppInfo>, rest: &str) {
    let Some(app) = app else { return };
    if rest.chars().count() < 30 {
        // 防御：短行（正常格式不会出现），整行视为名称
        app.label = rest.trim().to_string();
        return;
    }
    // 按第 30 个字符处切分（必须按字符边界，中文等多字节字符不能按字节切）
    let boundary = rest
        .char_indices()
        .nth(30)
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    let (label_part, package_part) = rest.split_at(boundary);
    if package_part.starts_with(' ') {
        // 名称 ≤30 字符：`<30列名称><空格><包名>`
        app.label = label_part.trim_end().to_string();
        let pkg = package_part.trim();
        if !pkg.is_empty() {
            app.package_name = pkg.to_string();
        }
    } else {
        // 名称 >30 字符：整行都是名称（无补位、无包名），包名在下一行续行
        app.label = rest.to_string();
    }
}

/// 收尾当前应用：包名缺失（解析异常）则丢弃。
fn finish_app(apps: &mut Vec<AppInfo>, current: &mut Option<AppInfo>) {
    if let Some(app) = current.take() {
        if !app.package_name.is_empty() {
            apps.push(app);
        }
    }
}

/// 校验 Android 包名合法性：段以字母开头，含字母数字下划线，总长 ≤ 255。
pub fn is_valid_package_name(pkg: &str) -> bool {
    if pkg.is_empty() || pkg.len() > 255 {
        return false;
    }
    pkg.split('.').all(|seg| {
        let mut chars = seg.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// 枚举设备可启动应用（kulua-server 一次性模式）。
///
/// 1. 确保 `/data/local/tmp/kulua-server.jar` 已部署（缺失才 push）
/// 2. `adb shell` 运行 `list_apps=true`，合并 stdout/stderr
/// 3. `timeout` 内未返回 → 超时错误
///
/// 注意：`AdbOps::run` 是阻塞调用且无法取消，超时后 adb 进程在后台自然结束，
/// 结果被丢弃（一次性枚举场景可接受）。
pub fn list_apps(
    adb: Arc<dyn AdbOps>,
    device: &Device,
    local_jar: &str,
    timeout: Duration,
) -> Result<Vec<AppInfo>, AdbError> {
    let needs_push = adb
        .run(&["-s", &device.serial, "shell", "test", "-f", REMOTE_JAR])
        .is_err();
    if needs_push {
        adb.push(device, local_jar, REMOTE_JAR)?;
    }

    let classpath = format!("CLASSPATH={}", REMOTE_JAR);
    let args: Vec<String> = [
        "-s",
        &device.serial,
        "shell",
        &classpath,
        "app_process",
        "/",
        "com.kulua.server.Server",
        "list_apps=true",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let (tx, rx) = std::sync::mpsc::channel();
    let adb = adb.clone();
    std::thread::spawn(move || {
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = tx.send(adb.run(&arg_refs));
    });

    let output = match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(AdbError::Other(format!(
                "list_apps timed out after {}s",
                timeout.as_secs()
            )));
        }
    };

    // 合并 stdout/stderr：错误信息（如 "No such file"）可能出现在 stderr
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(parse_apps_output(&text))
}

/// 应用列表缓存（按设备 UUID，TTL 60s）。
///
/// 设备断连 / 用户手动刷新（force）时失效。缓存不跨 Core 生命周期。
pub struct AppCache {
    entries: HashMap<Uuid, CacheEntry>,
}

struct CacheEntry {
    apps: Vec<AppInfo>,
    fetched_at: Instant,
}

impl AppCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// 返回未过期的缓存（若有）。
    pub fn get(&self, uuid: Uuid) -> Option<&[AppInfo]> {
        self.entries.get(&uuid).and_then(|e| {
            if e.fetched_at.elapsed() <= APP_CACHE_TTL {
                Some(e.apps.as_slice())
            } else {
                None
            }
        })
    }

    /// 写入缓存。
    pub fn put(&mut self, uuid: Uuid, apps: Vec<AppInfo>) {
        self.entries.insert(
            uuid,
            CacheEntry {
                apps,
                fetched_at: Instant::now(),
            },
        );
    }

    /// 失效指定设备的缓存（断连 / 强制刷新时调用）。
    pub fn invalidate(&mut self, uuid: Uuid) {
        self.entries.remove(&uuid);
    }

    /// 清理全部缓存（设备列表整体变化时使用）。
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for AppCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_system_and_regular_apps() {
        let output = "\
[server] INFO: Device: [Xiaomi] Redmi (Android 13)
[server] INFO: Processing Android apps... (this may take some time)
[server] INFO: List of apps:
 * Settings                             com.android.settings
 - Google Chrome                        com.android.chrome
";
        let apps = parse_apps_output(output);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].package_name, "com.android.settings");
        assert_eq!(apps[0].label, "Settings");
        assert!(apps[0].system);
        assert_eq!(apps[1].package_name, "com.android.chrome");
        assert_eq!(apps[1].label, "Google Chrome");
        assert!(!apps[1].system);
    }

    #[test]
    fn parse_chinese_labels() {
        // 中文标签按 UTF-16 长度补位，解析不依赖字节数
        let output = "\
List of apps:
 * 设置                                 com.android.settings
 - 微信                                 com.tencent.mm
";
        let apps = parse_apps_output(output);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].label, "设置");
        assert_eq!(apps[0].package_name, "com.android.settings");
        assert_eq!(apps[1].label, "微信");
        assert_eq!(apps[1].package_name, "com.tencent.mm");
    }

    #[test]
    fn parse_long_name_with_continuation_line() {
        // 名称 >30 字符时包名换行输出（3 空格 + 30 空格 + 包名）
        let output = "\
List of apps:
 * ThisIsAReallyLongApplicationNameThatExceedsThirtyCharacters
    com.example.longname
 - Short                              com.example.short
";
        let apps = parse_apps_output(output);
        assert_eq!(apps.len(), 2);
        assert_eq!(
            apps[0].label,
            "ThisIsAReallyLongApplicationNameThatExceedsThirtyCharacters"
        );
        assert_eq!(apps[0].package_name, "com.example.longname");
        assert_eq!(apps[1].package_name, "com.example.short");
    }

    #[test]
    fn parse_crlf_output() {
        let output =
            "List of apps:\r\n * Settings                             com.android.settings\r\n";
        let apps = parse_apps_output(output);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].package_name, "com.android.settings");
    }

    #[test]
    fn parse_empty_or_missing_header() {
        assert!(parse_apps_output("").is_empty());
        assert!(parse_apps_output("[server] INFO: Device: [X] Y (Android 13)").is_empty());
    }

    #[test]
    fn parse_drops_app_without_package() {
        // 异常输出：有标记无包名 → 丢弃而不是 panic
        let output = "List of apps:\n * Settings                             \n";
        assert!(parse_apps_output(output).is_empty());
    }

    #[test]
    fn package_name_validation() {
        assert!(is_valid_package_name("com.android.settings"));
        assert!(is_valid_package_name("org.videolan.vlc"));
        assert!(is_valid_package_name("a"));
        assert!(!is_valid_package_name(""));
        assert!(!is_valid_package_name("com..settings"));
        assert!(!is_valid_package_name("1com.android"));
        assert!(!is_valid_package_name("com.android settings"));
        assert!(!is_valid_package_name("com.android/evil"));
        let long = format!("{}.x", "a".repeat(300));
        assert!(!is_valid_package_name(&long));
    }

    #[test]
    fn cache_ttl_and_invalidation() {
        let uuid = Uuid::new_v4();
        let mut cache = AppCache::new();
        let apps = vec![AppInfo {
            package_name: "com.android.settings".into(),
            label: "Settings".into(),
            system: true,
        }];
        cache.put(uuid, apps.clone());
        assert_eq!(cache.get(uuid), Some(apps.as_slice()));

        cache.invalidate(uuid);
        assert_eq!(cache.get(uuid), None, "失效后应返回 None");

        cache.put(uuid, apps.clone());
        cache.clear();
        assert_eq!(cache.get(uuid), None, "clear 后应返回 None");
    }
}
