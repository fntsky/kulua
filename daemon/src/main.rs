use sync_core::cli;
use sync_core::app;
use sync_core::wireless_pair;

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

    let mut core = app::Core::new(jar_path, wireless_pair_info);
    let token = core.get_token();

    // Ctrl+C 触发停止信号
    let was_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    ctrlc::set_handler({
        let was_shutdown = was_shutdown.clone();
        let token = token.clone();
        move || {
            if was_shutdown.swap(true, std::sync::atomic::Ordering::SeqCst) {
                std::process::exit(0);
            }
            eprintln!("\nShutting down...");
            token.cancel();
        }
    })
    .expect("Error setting Ctrl+C handler");

    let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(32);
    core.run(cmd_rx).await;
}
