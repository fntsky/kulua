use crate::{
    adb_cmd::AdbOps,
    types::{AdbError, Device},
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
    time::Duration,
};
pub struct ScrcpyServer {
    device: Device,
    process: std::process::Child,
    pub port: u16,
}

impl ScrcpyServer {
    pub fn deploy_clipboard_only(
        adb: &dyn AdbOps,
        device: &Device,
        local_jar: &str,
        port: u16,
    ) -> Result<Self, crate::types::AdbError> {
        let remote_jar = "/data/local/tmp/scrcpy-server.jar";
        let needs_push = adb
            .run(&["-s", &device.serial, "shell", "test", "-f", remote_jar])
            .is_err();
        if needs_push {
            adb.push(device, local_jar, remote_jar)?;
        } else {
            println!("scrcpy-server.jar already exists on device, skipping push");
        }
        adb.forward(device, port, "scrcpy")?;
        let args = [
            &format!("CLASSPATH={}", remote_jar),
            "app_process",
            "/",
            "com.genymobile.scrcpy.Server",
            "4.0",
            "log_level=debug",
            "tunnel_forward=true",
            "video=false",
            "audio=false",
            "control=true",
            "cleanup=true",
            "send_device_meta=false",
            "send_dummy_byte=true",
            "send_frame_meta=false",
            "send_stream_meta=false",
        ];
        let mut process = adb.spawn_shell(device, &args)?;
        if let Some(stderr) = process.stderr.take() {
            let serial = device.serial.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) => eprintln!("scrcpy-server[{}] stderr: {}", serial, l),
                        Err(_) => break,
                    }
                }
            });
        }
        Ok(ScrcpyServer {
            device: device.clone(),
            process,
            port,
        })
    }
    pub fn stop(&mut self, adb: &dyn AdbOps) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        let _ = adb.run(&[
            "-s",
            &self.device.serial,
            "forward",
            "--remove",
            &format!("tcp:{}", self.port),
        ]);
    }
}

/// 从 reader 中解析一个 scrcpy DeviceMessage 剪贴板事件。
///
/// scrcpy 手机→PC 剪贴板消息格式（全部大端序）：
/// - 0:  1 byte:  消息类型（0x00 = TYPE_CLIPBOARD）
/// - 1:  4 bytes: 文本长度（大端 u32）
/// - 5:  N bytes: UTF-8 文本内容
///
/// 另见 reference 的 DeviceMessageWriter.java 和 device_msg.c
///
/// 返回 `Ok(Some(text))` 表示成功读取剪贴板事件，
/// 返回 `Ok(None)` 表示消息类型不是剪贴板事件，
/// 返回 `Err` 表示读取失败（连接断开或协议错误）。
#[allow(dead_code)]
pub fn parse_clipboard_event<R: Read>(reader: &mut R) -> Result<Option<String>, AdbError> {
    let mut type_buf = [0u8; 1];
    reader.read_exact(&mut type_buf).map_err(AdbError::Io)?;

    if type_buf[0] != 0x00 {
        // 非 TYPE_CLIPBOARD 消息，读尽可能多的字节做调试输出
        let mut extra = [0u8; 64];
        let n = reader.read(&mut extra).unwrap_or(0);
        println!(
            "scrcpy msg: type=0x{:02X}, payload ({} bytes): {:02X?}",
            type_buf[0],
            n,
            &extra[..n]
        );
        return Ok(None);
    }

    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(AdbError::Io)?;
    let text_len = u32::from_be_bytes(len_buf) as usize;

    let mut text = vec![0u8; text_len];
    reader.read_exact(&mut text).map_err(AdbError::Io)?;

    // 格式化输出完整消息
    let mut raw_buf = Vec::with_capacity(1 + 4 + text_len);
    raw_buf.extend_from_slice(&type_buf);
    raw_buf.extend_from_slice(&len_buf);
    raw_buf.extend_from_slice(&text);
    println!("scrcpy clipboard msg ({text_len} bytes): {:02X?}", raw_buf);

    let clip_text = String::from_utf8(text).map_err(|e| AdbError::Other(format!("{}", e)))?;
    Ok(Some(clip_text))
}

/// 启动 clipboard 监听线程（Phone→PC）。
///
/// 在一个 TcpStream 上同时处理双向通信：
/// - 读取设备发来的 clipbaord 事件（phone_clipboard_tx 转发给 Core）
/// - 接收 Core 发来的剪贴板写入指令（ctrl_rx）并写入设备
///
/// 返回 (线程句柄, 连接成功信号)。
#[allow(dead_code)]
pub fn spawn_clipboard_listener(
    port: u16,
    ctrl_rx: Receiver<String>,
    phone_clipboard_tx: Sender<String>,
) -> (JoinHandle<()>, Receiver<()>) {
    let (alive_tx, alive_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut stream = loop {
            match TcpStream::connect(format!("127.0.0.1:{}", port)) {
                Ok(mut s) => {
                    // 读取 dummy byte（0x00）验证 adb tunnel 确实连上了服务端
                    let mut dummy = [0u8; 1];
                    match s.read_exact(&mut dummy) {
                        Ok(()) => break s,
                        Err(_) => {
                            drop(s);
                            thread::sleep(Duration::from_millis(500));
                            continue;
                        }
                    }
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(500));
                    continue;
                }
            }
        };
        // 连接成功，通知调用方
        let _ = alive_tx.send(());

        // 设置 read timeout，以便在循环中能交替检查 ctrl_rx
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));

        loop {
            // 优先处理 Core 发来的写入指令（PC→Phone）
            while let Ok(text) = ctrl_rx.try_recv() {
                if let Err(e) = send_clipboard_to_device(&mut stream, &text) {
                    eprintln!("Failed to send clipboard to device: {}", e);
                } else {
                    println!("Clipboard sent to device: {}", text);
                }
            }

            // 读取设备事件（Phone→PC）
            match read_device_msg(&mut stream) {
                Ok(Some(text)) => {
                    println!("Phone clipboard: {}", text);
                    let _ = phone_clipboard_tx.send(text);
                }
                Ok(None) => {} // timeout，继续循环
                Err(_) => break, // 连接断开
            }
        }
    });
    (handle, alive_rx)
}

/// 从 scrcpy 控制连接中读取一条设备消息，超时时返回 Ok(None)。
///
/// 先检查 type byte（设了 read_timeout），有数据时取消超时读完整消息。
#[allow(dead_code)]
fn read_device_msg(stream: &mut TcpStream) -> Result<Option<String>, AdbError> {
    let mut type_buf = [0u8; 1];
    // 先尝试读 type byte，可能超时
    match stream.read_exact(&mut type_buf) {
        Err(e)
            if e.kind() == std::io::ErrorKind::TimedOut
                || e.kind() == std::io::ErrorKind::WouldBlock =>
        {
            return Ok(None);
        }
        Err(e) => return Err(AdbError::Io(e)),
        Ok(()) => {}
    }

    // 有消息来了，取消超时读完剩余部分
    let _ = stream.set_read_timeout(None);

    if type_buf[0] != 0x00 {
        // 非 TYPE_CLIPBOARD 消息，读尽可能多的字节做调试输出
        let mut extra = [0u8; 64];
        let n = stream.read(&mut extra).unwrap_or(0);
        println!(
            "scrcpy msg: type=0x{:02X}, payload ({} bytes): {:02X?}",
            type_buf[0],
            n,
            &extra[..n]
        );
        return Ok(None);
    }

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(AdbError::Io)?;
    let text_len = u32::from_be_bytes(len_buf) as usize;

    let mut text = vec![0u8; text_len];
    stream.read_exact(&mut text).map_err(AdbError::Io)?;

    let clip_text = String::from_utf8(text).map_err(|e| AdbError::Other(format!("{}", e)))?;
    Ok(Some(clip_text))
}

/// 通过 scrcpy 控制协议向设备写入剪贴板文本（PC→Phone）。
///
/// scrcpy 控制协议剪贴板消息格式（全部大端序）：
/// - 0:  1 byte:  消息类型（0x09 = TYPE_SET_CLIPBOARD）
/// - 1:  8 bytes: 序列号（uint64，大端序）
/// - 9:  1 byte:  paste 标记（0 = 不自动粘贴）
/// - 10: 4 bytes: 文本长度（uint32，大端序）
/// - 14: N bytes: UTF-8 文本内容
#[allow(dead_code)]
pub fn send_clipboard_to_device(stream: &mut TcpStream, text: &str) -> Result<(), AdbError> {
    let text_bytes = text.as_bytes();
    let len = text_bytes.len() as u32;

    let mut msg = Vec::with_capacity(1 + 8 + 1 + 4 + text_bytes.len());
    msg.push(0x09); // CONTROL_MSG_TYPE_SET_CLIPBOARD
    msg.extend_from_slice(&0u64.to_be_bytes()); // sequence = 0
    msg.push(0u8); // paste = false
    msg.extend_from_slice(&len.to_be_bytes()); // 文本长度（大端 u32）
    msg.extend_from_slice(text_bytes); // UTF-8 文本

    stream.write_all(&msg).map_err(AdbError::Io)?;
    Ok(())
}

/// 异步版 `send_clipboard_to_device`，配合 tokio::net::TcpStream 使用
pub async fn send_clipboard_async(
    stream: &mut tokio::net::TcpStream,
    text: &str,
) -> Result<(), AdbError> {
    use tokio::io::AsyncWriteExt;
    let text_bytes = text.as_bytes();
    let len = text_bytes.len() as u32;

    let mut msg = Vec::with_capacity(1 + 8 + 1 + 4 + text_bytes.len());
    msg.push(0x09);
    msg.extend_from_slice(&0u64.to_be_bytes());
    msg.push(0u8);
    msg.extend_from_slice(&len.to_be_bytes());
    msg.extend_from_slice(text_bytes);

    stream.write_all(&msg).await.map_err(AdbError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_clipboard_event;
    use crate::types::AdbError;
    use std::io::Cursor;

    fn build_clipboard_msg(text: &str) -> Vec<u8> {
        let bytes = text.as_bytes();
        [
            &[0x00u8][..], // type = TYPE_CLIPBOARD
            &(bytes.len() as u32).to_be_bytes()[..],
            bytes,
        ]
        .concat()
    }

    #[test]
    fn test_parse_clipboard_event_normal() {
        let mut cursor = Cursor::new(build_clipboard_msg("Hello"));
        let result = parse_clipboard_event(&mut cursor).unwrap();
        assert_eq!(result, Some("Hello".to_string()));
    }

    #[test]
    fn test_parse_clipboard_event_chinese() {
        let text = "你好世界";
        let mut cursor = Cursor::new(build_clipboard_msg(text));
        let result = parse_clipboard_event(&mut cursor).unwrap();
        assert_eq!(result, Some(text.to_string()));
    }

    #[test]
    fn test_parse_clipboard_event_empty() {
        let mut cursor = Cursor::new(build_clipboard_msg(""));
        let result = parse_clipboard_event(&mut cursor).unwrap();
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn test_parse_clipboard_event_large_text() {
        let text = "a".repeat(10_000);
        let mut cursor = Cursor::new(build_clipboard_msg(&text));
        let result = parse_clipboard_event(&mut cursor).unwrap();
        assert_eq!(result, Some(text));
    }

    #[test]
    fn test_parse_clipboard_event_skip_other_types() {
        // TYPE_ACK_CLIPBOARD = 1
        let data = [0x01u8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut cursor = Cursor::new(data);
        let result = parse_clipboard_event(&mut cursor).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_clipboard_event_skip_unknown_types() {
        for &msg_type in &[0x02u8, 0x09u8, 0x0Au8] {
            let data = [msg_type];
            let mut cursor = Cursor::new(data);
            let result = parse_clipboard_event(&mut cursor)
                .unwrap_or_else(|_| panic!("msg type 0x{:02X} should not error", msg_type));
            assert_eq!(
                result, None,
                "msg type 0x{:02X} should be skipped",
                msg_type
            );
        }
    }

    #[test]
    fn test_parse_clipboard_event_truncated_stream() {
        let data = [0x00u8, 0x00, 0x00, 0x00];
        let mut cursor = Cursor::new(data);
        let result = parse_clipboard_event(&mut cursor);
        assert!(matches!(result, Err(AdbError::Io(_))));
    }

    #[test]
    fn test_parse_clipboard_event_empty_reader() {
        let data = [];
        let mut cursor = Cursor::new(data);
        let result = parse_clipboard_event(&mut cursor);
        assert!(matches!(result, Err(AdbError::Io(_))));
    }
}
