mod common;

/// 验证 adb 可执行文件可用（不需要设备）
#[test]
#[ignore = "requires adb in PATH"]
fn test_adb_version() {
    assert!(common::has_adb(), "adb should be available");
    let output = std::process::Command::new("adb")
        .arg("version")
        .output()
        .expect("failed to run adb version");
    assert!(output.status.success(), "adb version should succeed");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("Android Debug Bridge"),
        "output should contain 'Android Debug Bridge'"
    );
}

/// 验证 devices 命令返回格式正确
#[test]
#[ignore = "requires adb in PATH"]
fn test_adb_devices_format() {
    assert!(common::has_adb());
    let output = std::process::Command::new("adb")
        .args(["devices", "-l"])
        .output()
        .expect("failed to run adb devices");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.starts_with("List of devices attached"),
        "should start with header"
    );
}
