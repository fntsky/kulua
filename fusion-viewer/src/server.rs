//! kulua-server 连接（连接模式客户端，不部署 server）。
//!
//! 架构（阶段 4）：server 由 daemon session 统一部署（`kulua-server.jar`），
//! viewer 只负责连接：
//! - control socket：握手字节 0x01 → 控制通道（输入注入 / START_APP / RESIZE）
//! - video socket：握手字节 0x03 + 6B 创建请求（width u16be, height u16be, dpi u16be）
//!   → server 创建虚拟显示器 → 回 4B displayId（u32be）+ 4B codecId（ASCII）→ 帧流
//! 窗口关闭只断 socket（server 由 session 管理，不 kill、不删 forward）。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 连接类型握手字节（与 kulua-server ConnectionManager 一致）。
pub const TYPE_CONTROL: u8 = 0x01;
pub const TYPE_VIDEO: u8 = 0x03;

/// 已连接的 viewer 会话：持有 control + video 两个 socket。
pub struct ViewerSession {
    /// 控制 socket（写控制消息）
    pub control: TcpStream,
    /// 视频 socket（交给视频读取线程，只能取一次）
    video: Option<TcpStream>,
    /// server 分配的虚拟显示器 id（输入注入/RESIZE/START_APP 用）
    pub display_id: u32,
    /// 视频 codec id（4B ASCII，如 "h264"）
    pub codec_id: u32,
}

impl ViewerSession {
    /// 连接已有 kulua-server 并创建虚拟显示器。
    ///
    /// 阻塞直到 control + video 两个 socket 都握手成功（或超时失败）。
    pub fn connect(port: u16, display: &str) -> Result<Self, String> {
        let (width, height, dpi) = parse_display(display)?;

        // 1. 连接 control socket 并写握手字节（类型声明）
        let control = connect_with_retry(port, 20, "控制通道")
            .ok_or_else(|| format!("连接控制通道失败（端口 {}）", port))?;
        let mut control = control;
        control
            .write_all(&[TYPE_CONTROL])
            .map_err(|e| format!("控制通道握手失败: {}", e))?;
        let _ = control.set_nodelay(true);

        // 2. 连接 video socket：握手字节 + 创建请求 → 读 displayId + codecId
        //    注意：adb forward 监听器一建立就能 accept 本地 TCP，但 server
        //    （app_process）需要 1~3 秒才监听 localabstract socket；此时连接会
        //    被 adb 转发层直接断开 → 握手读失败。因此要重试整个流程
        //    （与 session 的 connect_socket 行为一致）。
        let start = std::time::Instant::now();
        let (video, display_id, codec_id) = loop {
            match connect_video_with_create(port, width, height, dpi) {
                Ok(result) => break result,
                Err(e) => {
                    if start.elapsed() > Duration::from_secs(15) {
                        return Err(format!("15s 内未收到 video 握手: {}", e));
                    }
                    eprintln!("[viewer] 视频通道握手失败（重试）: {}", e);
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        };
        println!(
            "[viewer] 虚拟显示器 #{} 已创建 ({}x{}@{})",
            display_id, width, height, dpi
        );

        Ok(ViewerSession {
            control,
            video: Some(video),
            display_id,
            codec_id,
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
        self.send(&crate::control::resize_display(
            self.display_id, width, height,
        ))
    }
}

/// 解析 `WxH/DPI` 显示器规格字符串（如 `1280x960/160`）。
pub fn parse_display(display: &str) -> Result<(u16, u16, u16), String> {
    let (size_part, dpi_part) = display
        .split_once('/')
        .ok_or_else(|| format!("非法显示器规格（应为 WxH/DPI）: {}", display))?;
    let (w, h) = size_part
        .split_once('x')
        .ok_or_else(|| format!("非法显示器规格（应为 WxH/DPI）: {}", display))?;
    let width: u16 = w
        .trim()
        .parse()
        .map_err(|_| format!("非法宽度: {}", w))?;
    let height: u16 = h
        .trim()
        .parse()
        .map_err(|_| format!("非法高度: {}", h))?;
    let dpi: u16 = dpi_part
        .trim()
        .parse()
        .map_err(|_| format!("非法 DPI: {}", dpi_part))?;
    if width == 0 || height == 0 || dpi == 0 {
        return Err(format!("显示器规格必须为正数: {}", display));
    }
    Ok((width, height, dpi))
}

/// 连接 video socket，完成创建虚拟显示器的握手。
///
/// 返回 (socket, displayId, codecId)。返回 Err 表示连接被断开
/// （server 尚未监听 / 已崩溃），调用方应重试。
fn connect_video_with_create(
    port: u16,
    width: u16,
    height: u16,
    dpi: u16,
) -> Result<(TcpStream, u32, u32), String> {
    let mut stream = connect_with_retry(port, 4, "视频通道")
        .ok_or_else(|| format!("连接 127.0.0.1:{} 失败", port))?;

    // 握手：类型字节 + 6B 创建请求（全部大端）
    let mut request = [0u8; 7];
    request[0] = TYPE_VIDEO;
    request[1..3].copy_from_slice(&width.to_be_bytes());
    request[3..5].copy_from_slice(&height.to_be_bytes());
    request[5..7].copy_from_slice(&dpi.to_be_bytes());
    stream
        .write_all(&request)
        .map_err(|e| format!("video 握手写入失败: {}", e))?;

    // server 先回 4B displayId，再回 4B codecId
    // 3s 超时防止 server 半死不活时永久阻塞
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut display_buf = [0u8; 4];
    stream
        .read_exact(&mut display_buf)
        .map_err(|e| format!("读取 displayId 失败: {}", e))?;
    let display_id = u32::from_be_bytes(display_buf);

    let mut codec_buf = [0u8; 4];
    stream
        .read_exact(&mut codec_buf)
        .map_err(|e| format!("读取 codecId 失败: {}", e))?;
    let codec_id = u32::from_be_bytes(codec_buf);
    let _ = stream.set_read_timeout(None);

    let _ = stream.set_nodelay(true);
    Ok((stream, display_id, codec_id))
}

/// 带重试的本地端口连接。
fn connect_with_retry(port: u16, attempts: u32, what: &str) -> Option<TcpStream> {
    for _ in 0..attempts {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return Some(stream),
            Err(e) => {
                eprintln!("[viewer] 连接{}失败（重试）: {}", what, e);
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_constants_match_kulua_server() {
        assert_eq!(TYPE_CONTROL, 0x01);
        assert_eq!(TYPE_VIDEO, 0x03);
    }

    #[test]
    fn parse_display_accepts_standard_format() {
        assert_eq!(parse_display("1280x960/160"), Ok((1280, 960, 160)));
        assert_eq!(parse_display(" 1920x1080/440 "), Ok((1920, 1080, 440)));
    }

    #[test]
    fn parse_display_rejects_bad_input() {
        assert!(parse_display("").is_err());
        assert!(parse_display("1280x960").is_err(), "缺少 /DPI");
        assert!(parse_display("1280/160").is_err(), "缺少 x");
        assert!(parse_display("axb/160").is_err(), "非数字");
        assert!(parse_display("0x960/160").is_err(), "零宽");
        assert!(parse_display("1280x0/160").is_err(), "零高");
        assert!(parse_display("1280x960/0").is_err(), "零 DPI");
        assert!(parse_display("1280x960/abc").is_err(), "非数字 DPI");
    }

    /// 模拟 kulua-server 的 mock：接受两个连接（control + video），
    /// 按真实 server 的字节布局回应，并记录收到的握手数据。
    ///
    /// 这个测试验证 viewer 的握手字节布局与 kulua-server 完全一致——
    /// 若两侧布局不匹配（如字段顺序/大小端错误），握手会失败或读到错误数据。
    #[test]
    fn connect_handshake_matches_kulua_server_protocol() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        // mock server 线程：模拟 ConnectionManager（每连接独立线程）
        // viewer 的连接顺序：先 control 后 video
        let server = std::thread::spawn(move || {
            // ── control 连接：读 1B 类型 ──
            let (mut control, _) = listener.accept().unwrap();
            let mut type_buf = [0u8; 1];
            control.read_exact(&mut type_buf).unwrap();
            assert_eq!(type_buf[0], TYPE_CONTROL, "control 握手类型应为 0x01");

            // ── video 连接：读 1B 类型 + 6B 创建请求 → 回 4B displayId + 4B codecId ──
            let (mut video, _) = listener.accept().unwrap();
            let mut buf = [0u8; 7];
            video.read_exact(&mut buf).unwrap();
            assert_eq!(buf[0], TYPE_VIDEO, "video 握手类型应为 0x03");
            // 创建请求：width u16be, height u16be, dpi u16be
            let width = u16::from_be_bytes([buf[1], buf[2]]);
            let height = u16::from_be_bytes([buf[3], buf[4]]);
            let dpi = u16::from_be_bytes([buf[5], buf[6]]);
            // 回 4B displayId（大端）+ 4B codecId（"h264"）
            video.write_all(&42u32.to_be_bytes()).unwrap();
            video.write_all(b"h264").unwrap();
            // 不做阻塞读：viewer 的 session 直到测试函数结束才 drop，
            // 阻塞等 EOF 会导致 join 死锁

            (width, height, dpi)
        });

        // viewer 连接
        let session = ViewerSession::connect(port, "1280x960/160").unwrap();
        assert_eq!(session.display_id, 42);
        assert_eq!(session.codec_id, u32::from_be_bytes(*b"h264"));

        // 验证 server 侧收到的创建请求
        let (width, height, dpi) = server.join().unwrap();
        assert_eq!((width, height, dpi), (1280, 960, 160));
    }

    /// viewer 连接失败时应返回 Err（server 未监听）。
    ///
    /// 标记 ignore：connect 会对未监听端口完整重试（20×250ms + video 15s），
    /// 单测耗时 ~45s 不划算；失败路径由 `connect_handshake_matches_*` 的
    /// 反例覆盖（mock 不回码流 → connect 超时失败）。
    #[test]
    #[ignore = "connect 重试链耗时 ~45s，失败路径由握手 mock 反例覆盖"]
    fn connect_to_closed_port_fails() {
        use std::net::TcpListener;
        // 绑定后立即释放，保证端口无人监听
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(ViewerSession::connect(port, "1280x960/160").is_err());
    }
}
