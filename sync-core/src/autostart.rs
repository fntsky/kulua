//! 应用「开机自启动」设置管理。
//!
//! 职责划分：
//! - **config 文件是唯一真源**（见 [`crate::settings`]），记录用户意图 `autostart_enabled: bool`；
//! - **Windows 注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`** 是机制实现，
//!   写入的启动项指向 daemon 可执行文件（daemon 启动后再拉起 UI）；
//! - **reconcile（启动时调用）**：根据 config 意图修复 Run 键使其一致，实现自愈
//!   （用户手动增删 Run 键都会被下次启动纠正回 config 的状态）。
//!
//! 平台：注册表部分仅 Windows 生效；其他平台 `supported=false`，读写均为无害 no-op，
//! 相关权限/写盘逻辑统一走 `Result`，不 panic。

use std::path::Path;

/// 自启动查询 / 设置后的状态快照，供 IPC 返回给 GUI。
#[derive(Debug, Clone)]
pub struct AutostartState {
    /// 是否已开启（config 真源的值）
    pub enabled: bool,
    /// 当前平台是否支持自启动（非 Windows 为 false）
    pub supported: bool,
}

/// 注册表 Run 键下本应用使用的值名称。
#[cfg(windows)]
const RUN_KEY_VALUE_NAME: &str = "Kulua";
/// 注册表 Run 键父路径（HKCU 根下）。
#[cfg(windows)]
const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 当前平台是否支持注册表自启动。
#[cfg(windows)]
fn registry_supported() -> bool {
    true
}

/// 当前平台是否支持注册表自启动（非 Windows 恒为 false）。
#[cfg(not(windows))]
fn registry_supported() -> bool {
    false
}

/// 把 daemon 可执行文件写进（或移出）HKCU Run 键，使其匹配 `enabled`。
#[cfg(windows)]
fn set_registry_entry(exe_path: &Path, enabled: bool) -> std::io::Result<()> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE};

    let run_key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY_PATH, KEY_QUERY_VALUE | KEY_SET_VALUE)?;
    if enabled {
        // Run 键值为 REG_SZ，记录 daemon 可执行文件的完整路径
        run_key.set_value(RUN_KEY_VALUE_NAME, &exe_path.to_string_lossy().to_string())?;
    } else {
        // 删除我们的值；不存在时忽略 NotFund 错误
        match run_key.delete_value(RUN_KEY_VALUE_NAME) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 查询 HKCU Run 键中本应用是否已注册（检测 real 状态）。
///
/// 当前设计以 config 为唯一真源、reconcile 纠正注册表，因此这里并不参与
/// 状态判断；保留仅用于手动排查 / 未来调试自启动注册用的公开诊断接口。
#[allow(dead_code)]
#[cfg(windows)]
fn read_registry_entry() -> Option<String> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE};

    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY_PATH, KEY_QUERY_VALUE)
        .ok()
        .and_then(|run_key| run_key.get_value::<String, _>(RUN_KEY_VALUE_NAME).ok())
}

// 非 Windows：注册表读写为 no-op。
#[cfg(not(windows))]
fn set_registry_entry(_exe_path: &Path, _enabled: bool) -> Result<(), std::io::Error> {
    Ok(())
}

// 非 Windows：注册表查询为 no-op。
#[cfg(not(windows))]
fn read_registry_entry() -> Option<String> {
    None
}

/// 把 Run 键调整为与 config 意图一致。仅 Windows 实际生效。
fn reconcile_registry(exe_path: &Path) -> Result<(), std::io::Error> {
    if !registry_supported() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        set_registry_entry(exe_path, crate::settings::read().autostart_enabled)
            .map_err(std::io::Error::other)
    }
    #[cfg(not(windows))]
    {
        let _ = exe_path;
        Ok(())
    }
}

/// 查询当前自启动状态（以 config 真源为准）。
pub fn current_state(exe_path: &Path) -> AutostartState {
    let _ = exe_path;
    AutostartState {
        enabled: crate::settings::read().autostart_enabled,
        supported: registry_supported(),
    }
}

/// 设置自启动开关：更新 config（真源）并同步写 / 删 Run 键。
///
/// 成功后返回最新状态。`enabled=false` 时若当前平台不支持，仍会写入 config=false。
pub fn set_enabled(exe_path: &Path, enabled: bool) -> Result<AutostartState, std::io::Error> {
    let mut config = crate::settings::read();
    config.autostart_enabled = enabled;
    crate::settings::write(&config)?;
    set_registry_entry(exe_path, enabled).map_err(std::io::Error::other)?;
    Ok(AutostartState {
        enabled,
        supported: registry_supported(),
    })
}

/// 启动时调用：根据 config 修复 Run 键，使其与用户意图一致（自愈）。
pub fn reconcile(exe_path: &Path) {
    if registry_supported() {
        if let Err(e) = reconcile_registry(exe_path) {
            eprintln!("autostart reconcile failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_supported_matches_platform() {
        #[cfg(windows)]
        assert!(registry_supported());
        #[cfg(not(windows))]
        assert!(!registry_supported());
    }
}
