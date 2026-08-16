//! fusion-viewer 入口：自研 scrcpy 客户端。
//!
//! 用法：`fusion-viewer.exe --serial <serial> --package <pkg> [--label <名称>] --jar <jar路径>`
//! 流程：部署 scrcpy-server（虚拟显示器）→ 连接 video/control socket →
//! 启动应用（START_APP）→ winit 窗口渲染 + 输入注入。

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
    let adb = resolve_adb();

    // 1. 部署 server + 建立连接
    let mut session = match server::ServerSession::deploy(&args, &adb) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fusion-viewer: 部署失败: {}", e);
            std::process::exit(1);
        }
    };
    println!(
        "[viewer] server 就绪 (scid={}, port={})",
        session.scid_hex, session.port
    );

    // 2. 启动应用（START_APP 控制消息）
    match control::start_app(&args.package) {
        Ok(msg) => {
            if let Err(e) = session.send(&msg) {
                eprintln!("fusion-viewer: {}", e);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("fusion-viewer: {}", e);
            std::process::exit(2);
        }
    }
    println!("[viewer] 已请求启动 {}", args.package);

    // 3. 视频读取线程（解码后唤醒事件循环重绘）
    let video = match session.take_video() {
        Some(v) => v,
        None => {
            eprintln!("fusion-viewer: 视频 socket 已被取走");
            std::process::exit(1);
        }
    };
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
    video::spawn_video_thread(video, video_tx, proxy);

    let mut viewer = app::ViewerApp::new(args, session, video_rx);
    if let Err(e) = event_loop.run_app(&mut viewer) {
        eprintln!("fusion-viewer: 事件循环错误: {}", e);
        std::process::exit(1);
    }

    // 5. 窗口关闭：socket drop → server 检测断连自动清理
    println!("[viewer] 退出");
}

/// 解析 adb 路径：当前目录优先，PATH 兜底（与 sync-core 一致）。
fn resolve_adb() -> String {
    #[cfg(windows)]
    let local = "./adb.exe";
    #[cfg(not(windows))]
    let local = "./adb";

    if std::path::Path::new(local).exists() {
        local.to_string()
    } else {
        "adb".to_string()
    }
}
