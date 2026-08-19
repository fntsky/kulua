//! fusion-viewer 入口：自研 kulua-server 客户端（UDP 直连 + eframe/egui）。
//!
//! 用法：`fusion-viewer.exe --connect <ip:port> --package <pkg> [--label <名称>] [--display <WxH/DPI>] [--debug]`
//! 流程：连接 daemon session 已部署的 kulua-server（CreateDisplay 创建虚拟显示器）→
//! 启动应用（START_APP）→ egui 窗口渲染 + 输入注入 + HUD（码率/帧率/丢包率）。

mod app;
mod args;
mod control;
mod decoder;
mod keys;
mod server;
mod video;
mod yuv;

use std::sync::mpsc;

fn main() {
    let args = match args::parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("fusion-viewer: {}", e);
            std::process::exit(2);
        }
    };
    let window_title = args.window_title();

    // 连接已有 kulua-server + 创建虚拟显示器（HELLO → CreateDisplay → DisplayReady）
    let mut session = match server::ViewerSession::connect(args.addr, &args.display) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fusion-viewer: 连接失败: {}", e);
            std::process::exit(1);
        }
    };
    println!(
        "[viewer] server 已连接 (addr={}, display_id={})",
        args.addr, session.display_id
    );

    // 立即发送 START_APP（虚拟显示器在 CreateDisplay 已创建，不等首帧以免死锁）
    if let Err(e) = session.send_ctrl(&control::start_app(session.display_id, &args.package)) {
        eprintln!("[viewer] START_APP 发送失败: {}", e);
    } else {
        println!("[viewer] 已请求启动 {}", args.package);
    }

    // 视频解码线程：输入来自 UI 线程转发的会话事件；输出 RGBA 帧
    let (video_input_tx, video_input_rx) = mpsc::sync_channel::<video::VideoInput>(64);
    let (video_event_tx, video_event_rx) = mpsc::sync_channel::<video::VideoEvent>(8);
    let codec_id = session.codec_id;
    video::spawn_video_thread(video_input_rx, codec_id, video_event_tx);

    let display_id = session.display_id;
    let udp_session = session.into_inner();
    let app = app::ViewerApp::new(
        args,
        udp_session,
        display_id,
        video_input_tx,
        video_event_rx,
    );

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(window_title)
            .with_inner_size([1280.0, 960.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "fusion-viewer",
        options,
        Box::new(move |_cc| Ok(Box::new(app) as Box<dyn eframe::App>)),
    );
    if let Err(e) = result {
        eprintln!("fusion-viewer: eframe 错误: {e}");
        std::process::exit(1);
    }
    println!("[viewer] 退出");
}
