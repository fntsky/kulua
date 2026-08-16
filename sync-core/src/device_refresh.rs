use crate::types::Device;

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const MAX_PAYLOAD: usize = 64 * 1024;

/// 通过 `adb track-devices` 长连接监听设备状态变化，通过 watch channel 推送。
pub fn spawn(device_tx: watch::Sender<HashMap<String, Device>>, token: CancellationToken) {
    let adb_path = crate::adb_cmd::resolve_adb();

    tokio::spawn(async move {
        use tokio::process::Command;
        let mut last: HashMap<String, Device> = HashMap::new();
        let mut retry_delay = Duration::from_millis(100);

        while !token.is_cancelled() {
            let mut cmd = Command::new(&adb_path);
            cmd.arg("track-devices")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            #[cfg(windows)]
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
            let mut child = match cmd.spawn() {
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

                // 解析并推送（解析失败记录日志，避免静默停止更新）

                match crate::protocol::devices::parse_devices(&payload) {
                    Ok(devices) => {
                        // 推全量原始列表（含 Offline/Unauthorized/Unknown），
                        // 供 IPC 的 device.adb_list / adb.updated（UI "ADB 连接" 页面）。
                        // 上层（Core）自行过滤 Device 状态后再做设备合并。
                        let mut map = HashMap::new();
                        for d in devices {
                            map.insert(d.serial.clone(), d);
                        }

                        // 避免重复发送相同设备列表
                        if map != last {
                            last = map.clone();
                            let _ = device_tx.send(map);
                        }
                    }
                    Err(e) => {
                        eprintln!("parse_devices failed ({} bytes): {}", payload.len(), e);
                    }
                }
            }

            child.kill().await.ok();
            tokio::select! {
                _ = token.cancelled() => {},
                _ = child.wait() => {},
            }
            // 即使被取消也再试一次，避免 kill 未生效就 drop → 孤儿进程
            let _ = child.wait().await;
            // 'frame 循环因出错退出或断线后，退避再重连
            // 避免 ADB 反复崩溃时 CPU 忙循环
            tokio::select! {
                _ = token.cancelled() => break,
                _ = tokio::time::sleep(retry_delay) => {
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(5));
                }
            }
        }
    });
}
