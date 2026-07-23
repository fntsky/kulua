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
    pub fn deploy_scrcpy(
        adb: &dyn AdbOps,
        device: &Device,
        local_jar: &str,
        port: u16,
        audio_enabled: bool,
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
        let audio_flag = if audio_enabled { "true" } else { "false" };
        let classpath = format!("CLASSPATH={}", remote_jar);
        let audio_arg = format!("audio={}", audio_flag);
        let args = vec![
            &classpath,
            "app_process",
            "/",
            "com.genymobile.scrcpy.Server",
            "4.0",
            "log_level=debug",
            "tunnel_forward=true",
            "video=false",
            &audio_arg,
            "control=true",
            "cleanup=true",
            "send_device_meta=true",
            "send_dummy_byte=true",
            "send_frame_meta=false",
            "send_stream_meta=false",
        ];
        adb.forward(device, port, "scrcpy")?;
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
        // 先杀远程（设备端 scrcpy-server），确保无论本地如何终止都不会残留
        let kill_cmd = "kill -9 $(ps 2>/dev/null | grep com.genymobile.scrcpy | grep -v grep | awk '{print $2}') 2>/dev/null; true";
        let _ = adb.run(&["-s", &self.device.serial, "shell", kill_cmd]);

        // 再杀本地 adb shell 进程
        let _ = self.process.kill();
        let _ = self.process.wait();

        // 最后清理端口转发
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
pub use crate::protocol::clipboard::parse_clipboard_event;

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
                Ok(None) => {}   // timeout，继续循环
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
