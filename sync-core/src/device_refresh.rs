use crate::types::{Device, DeviceState};

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use tokio::sync::watch;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const MAX_PAYLOAD: usize = 64 * 1024;

/// 通过 `adb track-devices` 长连接监听设备状态变化，通过 watch channel 推送。
pub fn spawn(device_tx: watch::Sender<HashMap<String, Device>>, token: CancellationToken) {
    #[cfg(windows)]
    let adb_candidate = "./adb.exe";
    #[cfg(not(windows))]
    let adb_candidate = "./adb";

    let adb_path = if std::path::Path::new(adb_candidate).exists() {
        std::path::PathBuf::from(adb_candidate)
    } else {
        std::path::PathBuf::from("adb")
    };

    tokio::spawn(async move {
        use tokio::process::Command;
        let mut last: HashMap<String, Device> = HashMap::new();

        while !token.is_cancelled() {
            let mut child = match Command::new(&adb_path)
                .arg("track-devices")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to spawn adb track-devices: {}", e);
                    tokio::select! {
                        _ = token.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(2)) => continue,
                    }
                }
            };

            let stdout = child.stdout.take().expect("adb track-devices stdout");
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut len_buf = [0u8; 4];

            'frame: loop {
                // 读取 4 字节 ASCII hex 长度头（可取消）
                let result = tokio::select! {
                    _ = token.cancelled() => break 'frame,
                    r = reader.read_exact(&mut len_buf) => r,
                };

                if let Err(e) = result {
                    if e.kind() != io::ErrorKind::UnexpectedEof {
                        eprintln!("track-devices disconnected: {}", e);
                    }
                    break;
                }

                let len_str = match std::str::from_utf8(&len_buf) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("track-devices invalid length header: {}", e);
                        break;
                    }
                };

                let len = match usize::from_str_radix(len_str, 16) {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("track-devices bad length: {}", e);
                        break;
                    }
                };

                // ADB 协议允许空 payload，跳过
                if len == 0 {
                    continue;
                }

                if len > MAX_PAYLOAD {
                    eprintln!("track-devices payload too large: {} bytes", len);
                    break;
                }

                // 读取 payload（可取消）
                let mut payload = vec![0u8; len];
                let result = tokio::select! {
                    _ = token.cancelled() => break 'frame,
                    r = reader.read_exact(&mut payload) => r,
                };

                if let Err(e) = result {
                    eprintln!("track-devices read payload error: {}", e);
                    break;
                }

                // 解析并推送
                if let Ok(devices) = crate::protocol::devices::parse_devices(&payload) {
                    let mut map = HashMap::new();
                    for d in devices {
                        if d.state == DeviceState::Device {
                            map.insert(d.serial.clone(), d);
                        }
                    }

                    // 避免重复发送相同设备列表
                    if map != last {
                        last = map.clone();
                        let _ = device_tx.send(map);
                    }
                }
            }

            child.kill().await.ok();
            tokio::select! {
                _ = token.cancelled() => {},
                _ = child.wait() => {},
            }
        }
    });
}
