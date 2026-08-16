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

/// scid 高 16 位固定标记：`0x4B4C` 即 ASCII "KL"（Kulua 前缀），低 16 位为 ADB 转发端口。
///
/// 每个 Kulua session 生成确定性 scid，server 监听 `localabstract:scrcpy_<scid>`
/// （官方 scrcpy 不带 scid 时用默认 `scrcpy` socket）。这样 session 重启/清理时
/// 可以精准定位自己的 server 进程，绝不误杀融合窗口等其它 scrcpy 实例。
pub const SCID_PREFIX: u32 = 0x4B4C_0000;

/// 由 ADB 转发端口推导确定性 scid（31 位非负，server 侧按 16 进制解析）。
pub fn scid_for_port(port: u16) -> u32 {
    SCID_PREFIX | u32::from(port)
}

/// scid 的 8 位小写 16 进制字符串（server socket 名用 `%08x` 格式化，必须对齐）。
pub fn scid_hex(port: u16) -> String {
    format!("{:08x}", scid_for_port(port))
}

/// 生成按 scid 精准 kill 的 shell 脚本。
///
/// 用 `grep -l` 一次扫描全部 cmdline（匹配文件路径），而不是逐进程循环
/// `tr | grep`：无线 adb 下 toybox 每个 fork 都慢，984 个 /proc 进程的循环
/// 实测 ~26s，而单次 grep 扫描 <0.2s（快 160 倍）。匹配结果形如
/// `/proc/<pid>/cmdline`，从中提取 pid 后 kill。
/// - 跳过 `$$`（脚本自身 shell）与 `/proc/self`、`/proc/thread-self`
/// - 以 `true` 结尾保证 `adb shell` 退出码为 0
pub fn build_scid_kill_script(scid_hex: &str) -> String {
    format!(
        "for f in $(grep -l 'scid={}' /proc/[0-9]*/cmdline 2>/dev/null); do \
         p=${{f#/proc/}}; p=${{p%/cmdline}}; \
         [ \"$p\" = \"$$\" ] && continue; \
         [ \"$p\" = \"self\" ] && continue; \
         kill -9 \"$p\" 2>/dev/null; done; true",
        scid_hex
    )
}

/// 按 scid 精准杀死设备上属于本会话的 scrcpy-server 进程。
///
/// 替换旧的 broad kill（`grep com.genymobile.scrcpy` 全量击杀）：只匹配参数含
/// `scid=<本会话 hex>` 的进程，官方 scrcpy 融合窗口（各自随机 scid）不受影响。
pub fn kill_by_scid(adb: &dyn AdbOps, serial: &str, port: u16) {
    let script = build_scid_kill_script(&scid_hex(port));
    let _ = adb.run(&["-s", serial, "shell", &script]);
}

pub struct ScrcpyServer {
    device: Device,
    process: std::process::Child,
    pub port: u16,
}

/// 远程 jar 路径（自研 kulua-server，替代官方 scrcpy-server）。
pub const REMOTE_JAR: &str = "/data/local/tmp/kulua-server.jar";

impl ScrcpyServer {
    /// 部署自研 kulua-server（单进程多显示器，连接驱动）。
    ///
    /// 与官方 scrcpy-server 的关键区别（docs/fusion-mode-plan.md 阶段 4）：
    /// - 一个 server 进程管理多个虚拟显示器：video 连接携带创建参数（WxH/DPI）
    /// - 每个连接首字节为类型握手（0x01=control / 0x02=audio / 0x03=video）
    /// - server 参数：scid（会话隔离）+ audio_codec / audio_bit_rate（音频编码）
    pub fn deploy_scrcpy(
        adb: &dyn AdbOps,
        device: &Device,
        local_jar: &str,
        port: u16,
        _audio_enabled: bool,
        params: crate::settings::ScrcpyParams,
    ) -> Result<Self, crate::types::AdbError> {
        // 部署分步计时（KULUA_TIMING=1 时打印），定位启动慢的瓶颈
        let timing = std::env::var("KULUA_TIMING").map(|v| v == "1").unwrap_or(false);
        let t0 = std::time::Instant::now();
        let mark = |label: &str| {
            if timing {
                eprintln!("[timing]   deploy: {} — {}ms", label, t0.elapsed().as_millis());
            }
        };

        // 仅清理同 scid 的陈旧进程（上次崩溃残留），不碰其它 scrcpy 实例（如融合窗口）
        kill_by_scid(adb, &device.serial, port);
        mark("kill_by_scid");

        let remote_jar = REMOTE_JAR;
        let needs_push = adb
            .run(&["-s", &device.serial, "shell", "test", "-f", remote_jar])
            .is_err();
        mark("jar 检查");
        if needs_push {
            adb.push(device, local_jar, remote_jar)?;
        } else {
            println!("kulua-server.jar already exists on device, skipping push");
        }
        mark("jar push（如需）");
        let classpath = format!("CLASSPATH={}", remote_jar);
        let scid_arg = format!("scid={}", scid_hex(port));
        let mut args = vec![
            classpath.clone(),
            "app_process".to_string(),
            "/".to_string(),
            "com.kulua.server.Server".to_string(),
            scid_arg.clone(),
        ];
        // 音频编码参数：仅当显式配置时追加，否则 server 用默认（raw）
        if !params.audio_codec.is_empty() {
            args.push(format!("audio_codec={}", params.audio_codec));
        }
        if params.audio_bit_rate > 0 {
            args.push(format!("audio_bit_rate={}", params.audio_bit_rate));
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        // 隔离 socket：客户端默认连 `scrcpy_<hex>`（scid 会话隔离）
        let forward_target = format!("scrcpy_{}", scid_hex(port));
        adb.forward(device, port, &forward_target)?;
        mark("adb forward");
        let mut process = adb.spawn_shell(device, &arg_refs)?;
        mark("spawn app_process");
        if let Some(stderr) = process.stderr.take() {
            let serial = device.serial.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) => eprintln!("kulua-server[{}] stderr: {}", serial, l),
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
    /// 非阻塞检查 server 进程是否已退出。
    /// 返回 `None` 表示仍在运行，`Some(exit_status)` 表示已退出。
    pub fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.process.try_wait().ok().flatten()
    }

    /// 强制停止 server（清理远程进程和本地 adb shell）。
    pub fn stop(&mut self, adb: &dyn AdbOps) {
        // 先按 scid 精准杀远程（设备端 scrcpy-server），确保无论本地如何终止都不会残留
        kill_by_scid(adb, &self.device.serial, self.port);

        // 再杀本地 adb shell 进程
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scid_prefix_is_kl_marker() {
        // 高 16 位 0x4B4C 即 ASCII "KL"，用于在设备上区分 Kulua 会话与官方 scrcpy
        assert_eq!(SCID_PREFIX, 0x4B4C_0000);
    }

    #[test]
    fn scid_is_deterministic_from_port() {
        // 27183 = 0x6A2F
        assert_eq!(scid_for_port(27183), 0x4B4C_6A2F);
        assert_eq!(scid_for_port(0), SCID_PREFIX);
        assert_eq!(scid_for_port(0xFFFF), SCID_PREFIX | 0xFFFF);
        // 31 位非负，满足 server 侧 scid 约束（-1 或 0..2^31）
        assert!(scid_for_port(u16::MAX) < 1 << 31);
    }

    #[test]
    fn scid_hex_is_8_lowercase_hex_digits() {
        // server 侧 socket 名用 String.format("_%08x", scid)，必须严格 8 位对齐
        assert_eq!(scid_hex(27183), "4b4c6a2f");
        assert_eq!(scid_hex(0), "4b4c0000");
        assert_eq!(scid_hex(0xFFFF), "4b4cffff");
    }

    #[test]
    fn kill_script_targets_only_own_scid() {
        let script = build_scid_kill_script("4b4c6a17");
        assert!(script.contains("scid=4b4c6a17"), "应匹配本会话 scid");
        assert!(!script.contains("scid=4b4c6a18"), "不得匹配其它 scid");
        assert!(script.contains("$$"), "应跳过脚本自身 shell 进程");
        assert!(
            script.contains("/proc/[0-9]*/cmdline"),
            "应 grep 扫描 /proc cmdline（高效，非逐进程循环）"
        );
        assert!(!script.contains("tr '\\0'"), "不得用逐进程 tr|grep 循环（无线 adb 下极慢）");
        assert!(script.contains("self"), "应跳过 /proc/self 与 thread-self");
        assert!(script.ends_with("true"), "应以 true 结尾保证 exit 0");
    }

    #[test]
    fn kill_script_quotes_scid() {
        // scid 必须带引号，防止设备 shell 展开/分词
        let script = build_scid_kill_script("4b4c6a17");
        assert!(script.contains("'scid=4b4c6a17'"), "scid 应被单引号包裹");
    }

    #[test]
    fn kill_script_extracts_pid_from_grep_path() {
        // 验证从 /proc/<pid>/cmdline 提取 pid 的参数展开逻辑
        let script = build_scid_kill_script("4b4c6a17");
        assert!(
            script.contains("p=${f#/proc/}") && script.contains("p=${p%/cmdline}"),
            "应从 grep 输出路径提取 pid"
        );
        assert!(script.contains("kill -9 \"$p\""), "应 kill 提取出的 pid");
    }
}
