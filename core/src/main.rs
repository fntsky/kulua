use std::path::Path;

mod adb_cmd;
mod cli;
mod core;
mod notification;
mod scrcpy;
mod session;
mod types;
mod wireless_pair;
mod tray;

fn find_jar() -> Option<String> {
    let candidates = [
        Path::new("scrcpy-server"),
        Path::new("./scrcpy-server"),
        Path::new("../scrcpy-server"),
    ];
    for p in &candidates {
        if p.exists() {
            return Some(p.to_string_lossy().to_string());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("../../scrcpy-server");
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
    }
    None
}

fn main() {
    let jar_path = find_jar().unwrap_or_else(|| {
        eprintln!("scrcpy-server not found. Please place it in the project root directory.");
        std::process::exit(1);
    });

    let wireless_pair = wireless_pair::WirelessPairing::new();
    println!("Wireless Pairing Info: {}", wireless_pair.get_info());

    let (_handle, rx) = wireless_pair::start_discovery(&wireless_pair).unwrap();
    cli::print_qr_to_terminal(&wireless_pair.get_info());

    let mut core = core::Core::new();
    let stop_flag = core.get_stop_flag();

    // Ctrl+C 时触发停止信号，Drop 会自动清理所有 session
    // 用 was_shutdown 防止 Windows 上 handler 被多次调用
    let was_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    ctrlc::set_handler({
        let was_shutdown = was_shutdown.clone();
        let stop_flag = stop_flag.clone();
        move || {
            if was_shutdown.swap(true, std::sync::atomic::Ordering::SeqCst) {
                // 第二次收到 Ctrl+C 直接强退
                std::process::exit(0);
            }
            eprintln!("\nShutting down...");
            stop_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    })
    .expect("Error setting Ctrl+C handler");

    // 启动系统托盘
    let _tray_handle = tray::create_tray();

    core.run(rx, &wireless_pair, &jar_path);
    // core 在这里 drop，stop_all() 自动执行
    println!("Exited. Terminal should be restored.");
}
