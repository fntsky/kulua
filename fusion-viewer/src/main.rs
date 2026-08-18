//! fusion-viewer 入口：自研 kulua-server 客户端（UDP 直连模式）。
//!
//! 用法：`fusion-viewer.exe --connect <ip:port> --package <pkg> [--label <名称>] [--display <WxH/DPI>]`
//! 流程：连接 daemon session 已部署的 kulua-server（CreateDisplay 创建虚拟显示器）→
//! 启动应用（START_APP）→ winit 窗口渲染 + 输入注入。
//! 不部署 server、不 kill 远程进程：窗口关闭只 close 会话。

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

    // 先建事件循环（proxy 供解码线程唤醒）
    let event_loop = match winit::event_loop::EventLoop::new() {
        Ok(loop_) => loop_,
        Err(e) => {
            eprintln!("fusion-viewer: 创建事件循环失败: {}", e);
            std::process::exit(1);
        }
    };
    let proxy = event_loop.create_proxy();

    // 1. 连接已有 kulua-server + 创建虚拟显示器（HELLO → CreateDisplay → DisplayReady）
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

    // 立即发送 START_APP：虚拟显示器在 CreateDisplay 已创建（displayId 即返回），
    // 不需要等首帧——空显示器上无内容，MediaCodec 不会产生任何帧，等首帧会死锁
    if let Err(e) = session.send_ctrl(&control::start_app(session.display_id, &args.package)) {
        eprintln!("[viewer] START_APP 发送失败: {}", e);
    } else {
        println!("[viewer] 已请求启动 {}", args.package);
    }

    // 2. 视频解码线程（输入：UDP 会话转发的媒体/config；输出：解码帧）
    //    用有界 sync_channel + try_send：std mpsc::channel 是 rendezvous 同步通道，
    //    send 会阻塞到对方 recv —— 本架构 main 喂解码线程、解码线程又喂 main，
    //    两个阻塞 send 会互相等待死锁（打开即卡死）。有界 + try_send 满了丢新帧
    //    （视频容忍丢帧，渲染用最新帧即可），任何线程都不阻塞在 send 上。
    let (video_input_tx, video_input_rx) = mpsc::sync_channel::<video::VideoInput>(8);
    let (video_event_tx, video_event_rx) = mpsc::sync_channel::<video::VideoEvent>(8);
    let codec_id = session.codec_id;
    video::spawn_video_thread(video_input_rx, codec_id, video_event_tx, proxy);

    // 3. 把 UDP 会话交给事件循环（主循环负责消费事件流并转发给解码线程）
    let display_id = session.display_id;
    let udp_session = session.into_inner();
    let mut viewer = app::ViewerApp::new(
        args,
        udp_session,
        display_id,
        video_input_tx,
        video_event_rx,
    );
    if let Err(e) = event_loop.run_app(&mut viewer) {
        eprintln!("fusion-viewer: 事件循环错误: {}", e);
        std::process::exit(1);
    }

    // 4. 窗口关闭：close 会话（发 BYE）→ server 检测到空闲自动拆除显示器
    viewer.shutdown();
    println!("[viewer] 退出");
}
