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
fn setup_tray_icon(token: tokio_util::sync::CancellationToken) {
    use tray_icon::menu::{Menu, MenuItem, MenuEvent};
    use tray_icon::{TrayIconBuilder, Icon};

    std::thread::spawn(move || {
        let open_item = MenuItem::new("打开", true, None);
        let quit_item = MenuItem::new("退出", true, None);

        let menu = Menu::new();
        menu.append(&open_item).expect("append menu item");
        menu.append(&quit_item).expect("append menu item");

        let icon = Icon::from_rgba(
            std::iter::repeat([0x44u8, 0xbb, 0xff, 0xff])
                .take(16 * 16)
                .flatten()
                .collect(),
            16,
            16,
        )
        .expect("create icon");

        let _tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_tooltip("Sync Workspace")
            .build()
            .expect("build tray icon");

        // Windows: Win32 message loop required by tray-icon
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                GetMessageW, TranslateMessage, DispatchMessageW,
            };
            use std::ptr::null_mut;

            unsafe {
                let mut msg = std::mem::zeroed();
                loop {
                    let ret = GetMessageW(&mut msg, null_mut(), 0, 0);
                    if ret == 0 || ret == -1 {
                        break;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);

                    while let Ok(event) = MenuEvent::receiver().try_recv() {
                        if event.id() == quit_item.id() {
                            token.cancel();
                        } else if event.id() == open_item.id() {
                            // Launch sync-ui window
                            let ui_path = std::env::current_exe()
                                .ok()
                                .and_then(|p| p.parent().map(|d| d.join("sync-ui.exe")))
                                .filter(|p| p.exists())
                                .unwrap_or_else(|| {
                                    std::path::PathBuf::from("../target/debug/sync-ui.exe")
                                });
                            let _ = std::process::Command::new(ui_path).spawn();
                        }
                    }
                }
            }
        }

        // Non-Windows: block on channel
        #[cfg(not(target_os = "windows"))]
        {
            let receiver = MenuEvent::receiver();
            while let Ok(event) = receiver.recv() {
                if event.id() == quit_item.id() {
                    token.cancel();
                } else if event.id() == open_item.id() {
                    // Launch sync-ui window
                    let ui_name = if cfg!(target_os = "windows") { "sync-ui.exe" } else { "sync-ui" };
                    let ui_path = std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.join(ui_name)))
                        .filter(|p| p.exists())
                        .unwrap_or_else(|| {
                            std::path::PathBuf::from(if cfg!(target_os = "windows") {
                                "../target/debug/sync-ui.exe"
                            } else {
                                "../target/debug/sync-ui"
                            })
                        });
                    let _ = std::process::Command::new(ui_path).spawn();
                }
            }
        }
    });
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
    setup_tray_icon(token);

    core.run().await;
}
