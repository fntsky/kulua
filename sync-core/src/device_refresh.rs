use crate::types::{Device, DeviceState};

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use tokio::sync::watch;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncBufReadExt;
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
        let mut retry_delay = Duration::from_millis(100);

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

                // 消耗 payload 之后的换行符
                // ADB on Windows 使用 \r\n 行尾，长度计数包含 \r 但不包含末尾的 \n，
                // 如果不消耗掉 \n，下一帧的 4 字节长度头读到的第一个字节就是 \n → 非法 hex 字符
                loop {
                    let buf = reader.fill_buf().await.unwrap_or(&[][..]);
                    if buf.is_empty() || (buf[0] != b'\r' && buf[0] != b'\n') {
                        break;
                    }
                    // 只有 \r/\n 会进入这里，而 hex 字符 (0-9a-f) 不可能等于 \r/\n，
                    // 所以遇到下一帧长度头时一定 break，不会误吞数据。
                    debug_assert!(buf[0] == b'\r' || buf[0] == b'\n');
                    if buf[0] == b'\r' && buf.len() > 1 && buf[1] == b'\n' {
                        reader.consume(2);
                    } else {
                        reader.consume(1);
                    }
                }

                // 解析并推送（解析失败记录日志，避免静默停止更新）
                match crate::protocol::devices::parse_devices(&payload) {
                    Ok(devices) => {
                        // 只推送 Device 状态的设备，offline/unauthorized 等暂不暴露给上层。
                        // 上层如果需要在 UI 提示"未授权"等状态，可在此放开过滤。
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
