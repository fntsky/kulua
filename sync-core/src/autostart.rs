//! 应用「开机自启动」设置管理。
//!
//! 职责划分：
//! - **config 文件是唯一真源**（见 [`crate::settings`]），记录用户意图 `autostart_enabled: bool`；
//! - **Windows 注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`** 是机制实现，
//!   写入的启动项指向 daemon 可执行文件（daemon 启动后再拉起 UI）；
//! - **Linux XDG autostart**：`~/.config/autostart/kulua-daemon.desktop`（尊重
//!   `$XDG_CONFIG_HOME`），Exec 指向 daemon 可执行文件；
//! - **reconcile（启动时调用）**：根据 config 意图修复系统自启动条目使其一致，实现自愈
//!   （用户手动增删条目都会被下次启动纠正回 config 的状态）。
//!
//! 平台：Windows 走注册表，Linux 走 .desktop 文件；其余平台 `supported=false`，
//! 读写均为无害 no-op，相关权限/写盘逻辑统一走 `Result`，不 panic。

use std::path::Path;

/// Linux XDG autostart 目录（$XDG_CONFIG_HOME 未设置时回退 ~/.config/autostart）。
#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// 自启动查询 / 设置后的状态快照，供 IPC 返回给 GUI。
#[derive(Debug, Clone)]
pub struct AutostartState {
    /// 是否已开启（config 真源的值）
    pub enabled: bool,
    /// 当前平台是否支持自启动（Windows / Linux 为 true，其余平台 false）
    pub supported: bool,
}

/// 注册表 Run 键下本应用使用的值名称。
#[cfg(windows)]
const RUN_KEY_VALUE_NAME: &str = "Kulua";
/// 注册表 Run 键父路径（HKCU 根下）。
#[cfg(windows)]
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Linux XDG autostart 目录下的 .desktop 文件名。
#[cfg(target_os = "linux")]
const DESKTOP_FILE_NAME: &str = "kulua-daemon.desktop";

/// Linux XDG autostart 目录（遵循 freedesktop 规范：$XDG_CONFIG_HOME/autostart，
/// 未设置时用 ~/.config/autostart；连 HOME 都没有时退回当前目录下的 .config/autostart）。
#[cfg(target_os = "linux")]
fn autostart_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("autostart")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config").join("autostart")
    } else {
        PathBuf::from(".config").join("autostart")
    }
}

/// 当前平台是否支持系统自启动（Windows 注册表 / Linux XDG autostart）。
#[cfg(windows)]
fn platform_supported() -> bool {
    true
}

/// 当前平台是否支持系统自启动（Windows 注册表 / Linux XDG autostart）。
#[cfg(target_os = "linux")]
fn platform_supported() -> bool {
    true
}

/// 当前平台是否支持系统自启动（其余平台恒为 false）。
#[cfg(not(any(windows, target_os = "linux")))]
fn platform_supported() -> bool {
    false
}

/// 把 daemon 可执行文件写进（或移出）HKCU Run 键，使其匹配 `enabled`。
#[cfg(windows)]
fn set_platform_entry(exe_path: &Path, enabled: bool) -> std::io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE};

    let run_key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY_PATH, KEY_QUERY_VALUE | KEY_SET_VALUE)?;
    if enabled {
        // Run 键值为 REG_SZ，记录 daemon 可执行文件的完整路径
        run_key.set_value(RUN_KEY_VALUE_NAME, &exe_path.to_string_lossy().to_string())?;
    } else {
        // 删除我们的值；不存在时忽略 NotFound 错误
        match run_key.delete_value(RUN_KEY_VALUE_NAME) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 把 daemon 可执行文件写进（或移出）XDG autostart 目录的 .desktop 文件，
/// 使其匹配 `enabled`。
///
/// Exec 值按 freedesktop 规范转义（双引号包裹、`"` `\` `` ` `` `$` 前加反斜杠），
/// 保证含空格的安装路径也能被桌面环境正确解析。
#[cfg(target_os = "linux")]
fn set_platform_entry(exe_path: &Path, enabled: bool) -> std::io::Result<()> {
    let dir = autostart_dir();
    let path = dir.join(DESKTOP_FILE_NAME);
    if enabled {
        std::fs::create_dir_all(&dir)?;
        let escaped: String = exe_path
            .to_string_lossy()
            .chars()
            .flat_map(|c| match c {
                '"' | '\\' | '`' | '$' => vec!['\\', c],
                _ => vec![c],
            })
            .collect();
        let content = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Kulua Daemon\n\
             Exec=\"{escaped}\"\n\
             X-GNOME-Autostart-enabled=true\n"
        );
        std::fs::write(&path, content)
    } else {
        // 删除 .desktop 文件；不存在时忽略 NotFound 错误
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// 查询 HKCU Run 键中本应用是否已注册（检测 real 状态）。
///
/// 当前设计以 config 为唯一真源、reconcile 纠正注册表，因此这里并不参与
/// 状态判断；保留仅用于手动排查 / 未来调试自启动注册用的公开诊断接口。
#[allow(dead_code)]
#[cfg(windows)]
fn read_platform_entry() -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE};

    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY_PATH, KEY_QUERY_VALUE)
        .ok()
        .and_then(|run_key| run_key.get_value::<String, _>(RUN_KEY_VALUE_NAME).ok())
}

/// 读取 XDG autostart .desktop 文件内容（检测 real 状态，诊断用）。
#[allow(dead_code)]
#[cfg(target_os = "linux")]
fn read_platform_entry() -> Option<String> {
    let path = autostart_dir().join(DESKTOP_FILE_NAME);
    std::fs::read_to_string(&path).ok()
}

// 其余平台：读写为 no-op。
#[cfg(not(any(windows, target_os = "linux")))]
fn set_platform_entry(_exe_path: &Path, _enabled: bool) -> Result<(), std::io::Error> {
    Ok(())
}

// 其余平台：读写为 no-op。
#[cfg(not(any(windows, target_os = "linux")))]
fn read_platform_entry() -> Option<String> {
    None
}

/// 把系统自启动条目调整为与 config 意图一致。仅 Windows / Linux 实际生效。
fn reconcile_platform_entry(exe_path: &Path) -> Result<(), std::io::Error> {
    if !platform_supported() {
        return Ok(());
    }
    set_platform_entry(exe_path, crate::settings::read().autostart_enabled)
}

/// 查询当前自启动状态（以 config 真源为准）。
pub fn current_state(exe_path: &Path) -> AutostartState {
    let _ = exe_path;
    AutostartState {
        enabled: crate::settings::read().autostart_enabled,
        supported: platform_supported(),
    }
}

/// 设置自启动开关：更新 config（真源）并同步写 / 删系统自启动条目。
///
/// 成功后返回最新状态。`enabled=false` 时若当前平台不支持，仍会写入 config=false。
pub fn set_enabled(exe_path: &Path, enabled: bool) -> Result<AutostartState, std::io::Error> {
    let mut config = crate::settings::read();
    config.autostart_enabled = enabled;
    crate::settings::write(&config)?;
    set_platform_entry(exe_path, enabled)?;
    Ok(AutostartState {
        enabled,
        supported: platform_supported(),
    })
}

/// 启动时调用：根据 config 修复系统自启动条目，使其与用户意图一致（自愈）。
pub fn reconcile(exe_path: &Path) {
    if platform_supported() {
        if let Err(e) = reconcile_platform_entry(exe_path) {
            eprintln!("autostart reconcile failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_supported_matches_platform() {
        #[cfg(windows)]
        assert!(platform_supported());
        #[cfg(target_os = "linux")]
        assert!(platform_supported());
        #[cfg(not(any(windows, target_os = "linux")))]
        assert!(!platform_supported());
    }

    /// Linux：开启写 .desktop 文件（含空格路径正确转义），关闭删除文件。
    #[test]
    #[cfg(target_os = "linux")]
    fn linux_desktop_entry_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "kulua-autostart-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // XDG_CONFIG_HOME 指向临时目录，测试不污染真实 autostart
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };

        let exe = std::path::Path::new("/opt/kulua dir/daemon");
        set_platform_entry(exe, true).unwrap();
        assert!(platform_supported());
        let content = read_platform_entry().expect("desktop file written");
        assert!(content.contains("Exec=\"/opt/kulua dir/daemon\""));
        assert!(content.contains("X-GNOME-Autostart-enabled=true"));

        set_platform_entry(exe, false).unwrap();
        assert!(read_platform_entry().is_none(), "desktop file removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
