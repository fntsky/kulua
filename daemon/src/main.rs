// Release 构建不显示控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use sync_core::app;
use sync_core::cli;
use sync_core::wireless_pair;

use std::path::Path;
use std::process::Child;
use std::sync::{Arc, Mutex};

fn find_jar() -> Option<String> {
    // 自研 kulua-server.jar（阶段 4 起替代官方 scrcpy-server）
    let candidates = [
        Path::new("kulua-server.jar"),
        Path::new("./kulua-server.jar"),
        Path::new("../kulua-server.jar"),
    ];
    for p in &candidates {
        if p.exists() {
            return Some(p.to_string_lossy().to_string());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("../../kulua-server.jar");
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
    }
    None
}
/// 查找并启动 sync-ui 桌面窗口（与 daemon.exe 同目录优先，调试构建回退到 target/debug）。
///
/// 启动前先终止上一次残留的 UI 进程（避免重复窗口），并把新进程句柄存入 `ui_handle`，
/// 供托盘「退出」时结束前台 UI。
fn spawn_ui(ui_handle: &Mutex<Option<Child>>) {
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

    // 复用 handle；有旧 UI 先杀掉再拉起新实例
    {
        let mut guard = ui_handle.lock().unwrap();
        if let Some(mut old) = guard.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
    }

    match std::process::Command::new(&ui_path).spawn() {
        Ok(child) => {
            println!("sync-ui 已启动: {}", ui_path.display());
            *ui_handle.lock().unwrap() = Some(child);
        }
        Err(e) => eprintln!("启动 sync-ui 失败 ({}): {}", ui_path.display(), e),
    }
}

/// 结束前台 sync-ui 进程并清空句柄。
fn kill_ui(ui_handle: &Mutex<Option<Child>>) {
    let mut guard = ui_handle.lock().unwrap();
    if let Some(mut child) = guard.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn setup_tray_icon(
    ui_handle: Arc<Mutex<Option<Child>>>,
    token: tokio_util::sync::CancellationToken,
) {
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
            .with_tooltip("Kulua")
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
                            // 退出：先结束前台 UI，再让 daemon 优雅退出
                            kill_ui(&ui_handle);
                            token.cancel();
                        } else if event.id() == open_item.id() {
                            spawn_ui(&ui_handle);
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
                    kill_ui(&ui_handle);
                    token.cancel();
                } else if event.id() == open_item.id() {
                    spawn_ui(&ui_handle);
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

    // 共享的 UI 进程句柄：托盘「打开」/启动时写入，托盘「退出」时结束它
    let ui_handle: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    setup_tray_icon(ui_handle.clone(), token);

    // 启动时自动打开 UI（UI 会轮询 %TEMP%/sync-daemon.port 等待 daemon 就绪）
    spawn_ui(&ui_handle);

    core.run().await;
}
