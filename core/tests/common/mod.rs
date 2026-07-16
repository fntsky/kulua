/// 集成测试共享辅助函数
use std::path::Path;

/// 检查当前环境中是否有 adb 可用
pub fn has_adb() -> bool {
    std::process::Command::new("adb")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 检查是否有已连接的设备
#[allow(dead_code)]
pub fn has_connected_device() -> bool {
    if !has_adb() {
        return false;
    }
    let output = std::process::Command::new("adb")
        .args(["devices", "-l"])
        .output()
        .ok();
    match output {
        Some(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            text.lines()
                .skip(1)
                .any(|l| l.trim().contains("device") && !l.contains("devices"))
        }
        None => false,
    }
}

/// 检查 scrcpy-server.jar 是否存在
#[allow(dead_code)]
pub fn has_scrcpy_jar() -> bool {
    Path::new("scrcpy-server").exists() || Path::new("./scrcpy-server").exists()
}
