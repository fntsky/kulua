//! scrcpy-server 部署与 socket 连接（自研客户端，不依赖官方 scrcpy.exe）。
//!
//! 流程：确保 jar 已 push → `adb forward tcp:<port> localabstract:scrcpy_<scid>` →
//! `adb shell` 启动 server（`new_display=<尺寸>` 创建虚拟显示器）→ 连接 video/control socket。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::args::Args;

/// 远程 jar 路径（与 Kulua session 共用同一份部署）。
pub const REMOTE_JAR: &str = "/data/local/tmp/scrcpy-server.jar";

/// scid 高 16 位标记：`0x4B4D` 即 ASCII "KM"（Kulua Viewer 前缀），与
/// Kulua session 的 `0x4B4C` 前缀及官方 scrcpy 随机 scid 天然隔离。
const SCID_PREFIX: u32 = 0x4B4D_0000;

/// 已部署的 server：持有 adb shell 子进程与两个 socket。
pub struct ServerSession {
    /// adb shell 子进程（scrcpy-server 宿主）
    _shell: Child,
    /// adb 可执行文件路径（Drop 时清理 forward 用）
    adb: String,
    /// 设备地址
    serial: String,
    /// 本地 adb 转发端口
    pub port: u16,
    /// scid 十六进制（调试用）
    pub scid_hex: String,
    /// 控制 socket（写控制消息）
    pub control: TcpStream,
    /// 视频 socket（交给视频读取线程）
    video: Option<TcpStream>,
}

impl ServerSession {
    /// 部署 server 并建立 video/control 连接。
    ///
    /// 阻塞直到两个 socket 都连上（或超时失败）。
    pub fn deploy(args: &Args, adb: &str) -> Result<Self, String> {
        let scid = random_scid();
        let scid_hex = format!("{:08x}", scid);
        let forward_target = format!("scrcpy_{}", scid_hex);

        // 0. 检查设备在线（否则后续 forward/push 都只会得到含糊的超时）
        let state = Command::new(adb)
            .args(["-s", &args.serial, "get-state"])
            .output()
            .map_err(|e| format!("adb 不可用: {}", e))?;
        let state_text = String::from_utf8_lossy(&state.stdout).trim().to_string();
        if state_text != "device" {
            return Err(format!("设备不在线（get-state: {}）", state_text));
        }

        // 1. 清理 viewer 残留的陈旧 server（scid 前缀 4b4d）。
        //    为什么：viewer 的 control-recv 线程崩溃后，server 检测不到 socket
        //    断连而永久残留（虚拟显示器泄漏，累积后新 server 无法创建显示器）。
        kill_stale_servers(adb, &args.serial);

        // 2. 总是 push 最新 jar：设备上可能残留旧版本（旧 jar 不识别 new_display
        //    等参数，会导致 server 启动失败），viewer 作为自包含客户端必须保证版本一致
        if !Path::new(&args.jar).exists() {
            return Err(format!("jar 不存在: {}", args.jar));
        }
        let status = Command::new(adb)
            .args(["-s", &args.serial, "push", &args.jar, REMOTE_JAR])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("adb push 失败: {}", e))?;
        if !status.success() {
            return Err("adb push 失败".into());
        }

        // 2. 挑选空闲本地端口并建立 forward
        let port = pick_free_port()?;
        let forward_status = Command::new(adb)
            .args([
                "-s",
                &args.serial,
                "forward",
                &format!("tcp:{}", port),
                &format!("localabstract:{}", forward_target),
            ])
            .status()
            .map_err(|e| format!("adb forward 失败: {}", e))?;
        if !forward_status.success() {
            return Err("adb forward 失败".into());
        }

        // 3. 启动 server（虚拟显示器 + 视频 + 控制，无音频/剪贴板同步）
        let classpath = format!("CLASSPATH={}", REMOTE_JAR);
        let video_rate = args.video_bit_rate.to_string();
        let shell_args = [
            &classpath,
            "app_process",
            "/",
            "com.genymobile.scrcpy.Server",
            "4.0",
            "log_level=info",
            "tunnel_forward=true",
            "video=true",
            "audio=false",
            "control=true",
            "cleanup=true",
            "send_device_meta=true",
            "send_dummy_byte=true",
            "clipboard_autosync=false",
            "flex_display=true",
            &format!("scid={}", scid_hex),
            &format!("new_display={}", args.display),
            "video_codec=h264",
            &format!("video_bit_rate={}", video_rate),
        ];
        let mut shell = Command::new(adb)
            .args(["-s", &args.serial, "shell"])
            .args(shell_args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("启动 scrcpy-server 失败: {}", e))?;
        // 捕获 server stderr，便于排查启动失败（如 jar 版本过旧不认识 new_display）
        if let Some(stderr) = shell.stderr.take() {
            std::thread::spawn(move || {
                use std::io::{BufRead, BufReader};
                for line in BufReader::new(stderr).lines() {
                    match line {
                        Ok(l) => eprintln!("[viewer:server] {}", l),
                        Err(_) => break,
                    }
                }
            });
        }

        // 4. 连接 video socket（读 dummy byte 验证 server 就绪），再连 control socket
        //    server 的 accept 顺序：video → control（无 audio）
        //
        //    注意：adb forward 监听器一建立就能 accept 本地 TCP，但 server 冷启动
        //    （app_process）需要 1~3 秒才监听 localabstract socket；此时连接会被
        //    adb 转发层直接断开 → dummy 读失败。因此连接成功后 dummy 读失败要
        //    重连整个流程（与 Kulua session 的 connect_socket 行为一致）。
        let start = std::time::Instant::now();
        let video = loop {
            match connect_video_with_dummy(port) {
                Ok(video) => break video,
                Err(e) => {
                    // server 可能已崩溃（stderr 会打印原因）；立即失败
                    if shell.try_wait().ok().flatten().is_some() {
                        let _ = shell.kill();
                        return Err(format!("scrcpy-server 提前退出: {}", e));
                    }
                    // 总超时 30s（server 冷启动 + jar push 后的部署窗口）
                    if start.elapsed() > std::time::Duration::from_secs(30) {
                        let _ = shell.kill();
                        return Err(format!("scrcpy-server 30s 内未就绪: {}", e));
                    }
                    eprintln!("[viewer] 连接视频通道失败（重试）: {}", e);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        };
        let control = connect_with_retry(port, 20).ok_or_else(|| {
            let _ = shell.kill();
            "连接控制通道超时".to_string()
        })?;
        let _ = video.set_nodelay(true);
        let _ = control.set_nodelay(true);

        Ok(ServerSession {
            _shell: shell,
            adb: adb.to_string(),
            serial: args.serial.clone(),
            port,
            scid_hex,
            control,
            video: Some(video),
        })
    }

    /// 取出视频 socket（只能取一次，交给视频读取线程）。
    pub fn take_video(&mut self) -> Option<TcpStream> {
        self.video.take()
    }

    /// 发送一条控制消息。
    pub fn send(&mut self, msg: &[u8]) -> Result<(), String> {
        self.control
            .write_all(msg)
            .map_err(|e| format!("控制通道写入失败: {}", e))
    }

    /// 发送 RESIZE_DISPLAY（弹性显示器尺寸变化）。
    pub fn resize_display(&mut self, width: u16, height: u16) -> Result<(), String> {
        self.send(&crate::control::resize_display(width, height))
    }
}

impl Drop for ServerSession {
    fn drop(&mut self) {
        // 断开 socket（正常路径下 server 检测 IO 错误退出）
        let _ = self.control.shutdown(std::net::Shutdown::Both);
        // 主动按 scid 精准 kill server：control 线程崩溃后 server 检测不到断连，
        // 不能只依赖优雅退出，否则虚拟显示器/进程永久残留
        let script = build_scid_kill_script(&self.scid_hex);
        let _ = Command::new(&self.adb)
            .args(["-s", &self.serial, "shell", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // 清理 ADB forward，避免残留（下次启动端口冲突 / 转发积累）
        let _ = Command::new(&self.adb)
            .args([
                "-s",
                &self.serial,
                "forward",
                "--remove",
                &format!("tcp:{}", self.port),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// 连接 video socket 并读取 dummy byte（server 就绪验证）。
///
/// 返回 Err 表示连接被断开（server 尚未监听 / 已崩溃），调用方应重试。
fn connect_video_with_dummy(port: u16) -> Result<TcpStream, String> {
    let mut stream =
        connect_with_retry(port, 4).ok_or_else(|| format!("连接 127.0.0.1:{} 失败", port))?;
    // dummy byte 应在 accept 后立即到达；3s 超时防止 server 半死不活时永久阻塞
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    let mut dummy = [0u8; 1];
    stream
        .read_exact(&mut dummy)
        .map_err(|e| format!("读取 dummy byte 失败: {}", e))?;
    let _ = stream.set_read_timeout(None);
    Ok(stream)
}

/// 生成按 scid 前缀精准 kill 的 shell 脚本（只杀 viewer 家族的残留 server）。
///
/// 匹配参数含 `scid=4b4d`（viewer scid 前缀）的进程；跳过 `$$` 防止 shell 自杀；
/// `[ -r ]` 预检避免 PID 竞争时 shell 打印 "can't open /proc/.../cmdline" 噪音；
/// 以 `true` 结尾保证 exit 0。不碰 Kulua session（`4b4c` 前缀）与官方 scrcpy（随机 scid）。
fn build_stale_kill_script() -> String {
    "for p in $(ls /proc | grep -E '^[0-9]+$'); do \
     [ \"$p\" = \"$$\" ] && continue; \
     [ -r /proc/$p/cmdline ] || continue; \
     if tr '\\0' ' ' < /proc/$p/cmdline 2>/dev/null | grep -q 'scid=4b4d' 2>/dev/null; then \
     kill -9 \"$p\" 2>/dev/null; fi; done; true"
        .to_string()
}

/// 生成按精确 scid 精准 kill 的 shell 脚本（Drop 时清理本会话 server）。
fn build_scid_kill_script(scid_hex: &str) -> String {
    format!(
        "for p in $(ls /proc | grep -E '^[0-9]+$'); do \
         [ \"$p\" = \"$$\" ] && continue; \
         [ -r /proc/$p/cmdline ] || continue; \
         if tr '\\0' ' ' < /proc/$p/cmdline 2>/dev/null | grep -q 'scid={}' 2>/dev/null; then \
         kill -9 \"$p\" 2>/dev/null; fi; done; true",
        scid_hex
    )
}

/// 清理设备上 viewer 残留的陈旧 server 进程（scid 前缀 4b4d）。
fn kill_stale_servers(adb: &str, serial: &str) {
    let script = build_stale_kill_script();
    let _ = Command::new(adb)
        .args(["-s", serial, "shell", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// 生成随机 scid（高 16 位固定 0x4B4D，低 16 位随机）。
fn random_scid() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    // 全局计数器 + 时间戳混合：即使同一纳秒内多次调用也产生不同值
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = u64::from(std::process::id());
    let low = ((nanos ^ (pid * 2654435761) ^ (counter as u64 * 40503)) & 0xFFFF) as u32;
    SCID_PREFIX | low
}

/// 挑一个空闲本地端口（bind :0 探测后释放；竞态可接受，forward 失败会报错）。
fn pick_free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    Ok(listener.local_addr().map_err(|e| e.to_string())?.port())
}

/// 带重试的本地端口连接。
fn connect_with_retry(port: u16, attempts: u32) -> Option<TcpStream> {
    for _ in 0..attempts {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            return Some(stream);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scid_prefix_is_km_marker() {
        assert_eq!(SCID_PREFIX, 0x4B4D_0000);
        // 与 Kulua session 前缀（0x4B4C）不同
        assert_ne!(SCID_PREFIX, 0x4B4C_0000);
    }

    #[test]
    fn scid_low_bits_are_randomized() {
        // 两次生成大概率不同（低 16 位随机）
        let a = random_scid();
        let b = random_scid();
        assert_ne!(a, b, "scid 应尽量随机，避免多窗口冲突");
        assert_eq!(a & 0xFFFF_0000, SCID_PREFIX);
    }

    #[test]
    fn pick_free_port_returns_valid_port() {
        let port = pick_free_port().unwrap();
        assert!(port > 0);
        // 端口应是可用的（立即再 bind 同端口会失败）
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    #[test]
    fn stale_kill_script_targets_only_viewer_family() {
        let script = build_stale_kill_script();
        assert!(script.contains("scid=4b4d"), "应匹配 viewer 前缀 4b4d");
        assert!(!script.contains("scid=4b4c"), "不得匹配 Kulua session 前缀");
        assert!(script.contains("$$"), "应跳过脚本自身 shell 进程");
        assert!(script.ends_with("true"), "应以 true 结尾保证 exit 0");
    }

    #[test]
    fn scid_kill_script_matches_exact_scid() {
        let script = build_scid_kill_script("4b4dabcd");
        assert!(script.contains("scid=4b4dabcd"), "应匹配本会话 scid");
        assert!(!script.contains("scid=4b4dabce"), "不得匹配其它 scid");
        assert!(script.contains("'scid=4b4dabcd'"), "scid 应被单引号包裹");
    }
}
