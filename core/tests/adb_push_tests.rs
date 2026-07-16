mod common;

/// 验证可以推送文件到设备（需要一台已连接的设备）
#[test]
#[ignore = "requires device connected + scrcpy-server.jar"]
fn test_push_scrcpy_jar() {
    assert!(common::has_connected_device(), "requires connected device");
    assert!(common::has_scrcpy_jar(), "requires scrcpy-server.jar");
    let output = std::process::Command::new("adb")
        .args(["push", "scrcpy-server", "/data/local/tmp/scrcpy-server.jar"])
        .output()
        .expect("failed to push file");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() || text.contains("1 file pushed"),
        "push should succeed: {}",
        text
    );
}

/// 验证端口转发设置和移除
#[test]
#[ignore = "requires device connected"]
fn test_forward_roundtrip() {
    assert!(common::has_connected_device());
    // 先获取设备序列号
    let output = std::process::Command::new("adb")
        .args(["devices", "-l"])
        .output()
        .expect("failed to get devices");
    let text = String::from_utf8_lossy(&output.stdout);
    let serial = text
        .lines()
        .skip(1)
        .find(|l| l.trim().contains("device") && !l.contains("devices"))
        .and_then(|l| l.split_whitespace().next())
        .expect("should find a device serial");
    // 添加转发
    let add = std::process::Command::new("adb")
        .args(["-s", serial, "forward", "tcp:19999", "localabstract:test-scrcpy"])
        .output()
        .expect("failed to add forward");
    assert!(add.status.success(), "forward add should succeed");
    // 移除转发
    let remove = std::process::Command::new("adb")
        .args(["-s", serial, "forward", "--remove", "tcp:19999"])
        .output()
        .expect("failed to remove forward");
    assert!(remove.status.success(), "forward remove should succeed");
}
