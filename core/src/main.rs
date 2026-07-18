mod adb_cmd;
mod cli;
mod core;
mod notification;
mod scrcpy;
mod session;
mod types;
mod wireless_pair;

use std::path::Path;

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

#[tokio::main]
async fn main() {
    let jar_path = find_jar().unwrap_or_else(|| {
        eprintln!("scrcpy-server not found. Please place it in the project root directory.");
        std::process::exit(1);
    });

    let wireless_pair_info = wireless_pair::WirelessPairing::new();
    cli::print_qr_to_terminal(&wireless_pair_info.get_info());

    let mut core = core::Core::new(jar_path, wireless_pair_info);
    let stop_flag = core.get_stop_flag();

    // Ctrl+C 触发停止信号
    let was_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    ctrlc::set_handler({
        let was_shutdown = was_shutdown.clone();
        let stop_flag = stop_flag.clone();
        move || {
            if was_shutdown.swap(true, std::sync::atomic::Ordering::SeqCst) {
                std::process::exit(0);
            }
            eprintln!("\nShutting down...");
            stop_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    })
    .expect("Error setting Ctrl+C handler");

    core.run().await;
    println!("Exited. Terminal should be restored.");
}
