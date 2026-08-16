//! fusion-viewer 入口：自研 kulua-server 客户端（连接模式）。
//!
//! 用法：`fusion-viewer.exe --connect <port> --package <pkg> [--label <名称>] [--display <WxH/DPI>]`
//! 流程：连接 daemon session 已部署的 kulua-server（创建虚拟显示器）→
//! 启动应用（START_APP）→ winit 窗口渲染 + 输入注入。
//! 不部署 server、不 kill 远程进程：窗口关闭只断 socket。

mod app;
mod args;
mod control;
mod decoder;
mod keys;
mod server;
mod video;
mod yuv;

use std::io::Read;
use std::sync::mpsc;

fn main() {
    let args = match args::parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("fusion-viewer: {}", e);
            std::process::exit(2);
        }
    };

    // 1. 连接已有 kulua-server + 创建虚拟显示器
    let mut session = match server::ViewerSession::connect(args.port, &args.display) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fusion-viewer: 连接失败: {}", e);
            std::process::exit(1);
        }
    };
    println!(
        "[viewer] server 已连接 (port={}, display_id={})",
        args.port, session.display_id
    );

    // 立即发送 START_APP：自研 server 的虚拟显示器在 video 握手时已创建
    // （displayId 即返回），不需要等首帧——空显示器上无内容，MediaCodec
    // 不会产生任何帧，等首帧会死锁（应用启动后才有画面）
    match control::start_app(session.display_id, &args.package) {
        Ok(msg) => {
            if let Err(e) = session.send(&msg) {
                eprintln!("[viewer] START_APP 发送失败: {}", e);
            } else {
                println!("[viewer] 已请求启动 {}", args.package);
            }
        }
        Err(e) => eprintln!("[viewer] {}", e),
    }

    // 2. control socket 读方向 drain：server 会向所有 control 连接推送剪贴板
    //    事件（0x00），viewer 不处理剪贴板（session 负责），但必须把数据读走，
    //    否则 TCP 缓冲区满后 server 的推送线程会阻塞在写
    {
        let mut drain = session.control.try_clone().expect("control socket clone");
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match drain.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        });
    }

    // 3. 视频读取线程（解码后唤醒事件循环重绘）
    let (video_tx, video_rx) = mpsc::channel::<video::VideoEvent>();

    // 4. winit 事件循环
    let event_loop = match winit::event_loop::EventLoop::new() {
        Ok(loop_) => loop_,
        Err(e) => {
            eprintln!("fusion-viewer: 创建事件循环失败: {}", e);
            std::process::exit(1);
        }
    };
    let proxy = event_loop.create_proxy();
    let codec_id = session.codec_id;
    let video = match session.take_video() {
        Some(v) => v,
        None => {
            eprintln!("fusion-viewer: 视频 socket 已被取走");
            std::process::exit(1);
        }
    };
    video::spawn_video_thread(video, codec_id, video_tx, proxy);

    let mut viewer = app::ViewerApp::new(args, session, video_rx);
    if let Err(e) = event_loop.run_app(&mut viewer) {
        eprintln!("fusion-viewer: 事件循环错误: {}", e);
        std::process::exit(1);
    }

    // 5. 窗口关闭：socket drop → server 检测断连自动销毁显示器
    println!("[viewer] 退出");
}
