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

        // 1. 确保 jar 已部署（缺失才 push）
        let test_ok = Command::new(adb)
            .args(["-s", &args.serial, "shell", "test", "-f", REMOTE_JAR])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("adb 不可用: {}", e))?
            .success();
        if !test_ok {
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
            &format!("scid={}", scid_hex),
            &format!("new_display={}", args.display),
            "video_codec=h264",
            &format!("video_bit_rate={}", video_rate),
        ];
        let mut shell = Command::new(adb)
            .args(["-s", &args.serial, "shell"])
            .args(shell_args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("启动 scrcpy-server 失败: {}", e))?;

        // 4. 连接 video socket（读 dummy byte 验证 server 就绪），再连 control socket
        //    server 的 accept 顺序：video → control（无 audio）
        let mut video = connect_with_retry(port, 40).ok_or_else(|| {
            let _ = shell.kill();
            "连接视频通道超时（server 启动失败？）".to_string()
        })?;
        let mut dummy = [0u8; 1];
        if video.read_exact(&mut dummy).is_err() {
            let _ = shell.kill();
            return Err("读取 dummy byte 失败".into());
        }
        let control = connect_with_retry(port, 10).ok_or_else(|| {
            let _ = shell.kill();
            "连接控制通道超时".to_string()
        })?;
        let _ = video.set_nodelay(true);
        let _ = control.set_nodelay(true);

        Ok(ServerSession {
            _shell: shell,
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
        // 断开 socket → server 检测到 IO 错误 → cleanup 自动退出；
        // adb shell 子进程句柄释放后由 adb 侧回收
        let _ = self.control.shutdown(std::net::Shutdown::Both);
    }
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
}
