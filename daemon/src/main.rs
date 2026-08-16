// Release 构建不显示控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use sync_core::app;
use sync_core::cli;
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
/// 查找并启动 sync-ui 桌面窗口（与 daemon.exe 同目录优先，调试构建回退到 target/debug）。
fn launch_ui() {
    let ui_name = if cfg!(target_os = "windows") {
        "sync-ui.exe"
    } else {
        "sync-ui"
    };
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
    match std::process::Command::new(&ui_path).spawn() {
        Ok(_) => println!("sync-ui 已启动: {}", ui_path.display()),
        Err(e) => eprintln!("启动 sync-ui 失败 ({}): {}", ui_path.display(), e),
    }
}
fn setup_tray_icon(token: tokio_util::sync::CancellationToken) {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    std::thread::spawn(move || {
        let open_item = MenuItem::new("打开", true, None);
        let quit_item = MenuItem::new("退出", true, None);

        let menu = Menu::new();
        menu.append(&open_item).expect("append menu item");
        menu.append(&quit_item).expect("append menu item");

        // 从项目根 logo.png 解码为托盘图标
        let logo_bytes = include_bytes!("../../logo.png");
        let img = image::load_from_memory(logo_bytes).expect("decode logo.png");
        let rgba = img.to_rgba8();
        let (icon_w, icon_h) = rgba.dimensions();
        let icon = Icon::from_rgba(rgba.into_raw(), icon_w, icon_h).expect("create tray icon");

        let _tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_tooltip("Sync Workspace")
            .build()
            .expect("build tray icon");

        // Windows: Win32 message loop required by tray-icon
        #[cfg(target_os = "windows")]
        {
            use std::ptr::null_mut;
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, GetMessageW, TranslateMessage,
            };

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
                            launch_ui();
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
                    launch_ui();
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

    // 隐藏控制台窗口（Release 构建双击无黑框，但所有 println! 仍正常工作）
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::GetConsoleWindow;
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
        unsafe {
            ShowWindow(GetConsoleWindow(), SW_HIDE);
        }
    }

    let mut core = app::Core::new(jar_path, wireless_pair_info);
    let token = core.get_token();

    // 启动时自愈：让 HKCU Run 键与 config 里的自启动意图保持一致
    if let Ok(exe) = std::env::current_exe() {
        sync_core::autostart::reconcile(&exe);
    }

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

    // 启动时自动打开 UI（UI 会轮询 %TEMP%/sync-daemon.port 等待 daemon 就绪）
    launch_ui();

    core.run().await;
}
