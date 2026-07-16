use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tray_icon::menu::{Menu, MenuId, MenuItemBuilder};
use tray_icon::{Icon, TrayIconBuilder};

use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, WM_QUIT,
};

/// 托盘事件
#[derive(Debug, Clone, PartialEq)]
pub enum TrayEvent {
    /// 用户点击了"退出"
    Exit,
}

/// 托盘句柄。析构时通知后台线程退出并等待结束。
pub struct TrayHandle {
    thread_id: u32,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 创建系统托盘图标。
///
/// 在后台线程创建托盘并运行 Win32 消息泵。
/// 主线程通过 `tray_icon::menu::MenuEvent::receiver()` 接收菜单事件。
pub fn create_tray() -> TrayHandle {
    let tid = Arc::new(AtomicU32::new(0));
    let tid_clone = tid.clone();

    let handle = thread::Builder::new()
        .name("tray-worker".into())
        .spawn(move || {
            // 记录线程 ID，供主线程停止消息泵
            tid_clone.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);

            // 创建菜单
            let open_item = MenuItemBuilder::new()
                .text("打开")
                .id(MenuId::new("open"))
                .build();
            let exit_item = MenuItemBuilder::new()
                .text("退出")
                .id(MenuId::new("exit"))
                .build();

            let tray_menu = Menu::with_items(&[&open_item, &exit_item])
                .expect("failed to create tray menu");

            // 创建 32x32 蓝色图标
            let icon = Icon::from_rgba(create_default_icon_data(), 32, 32)
                .expect("failed to create tray icon");

            let _tray = TrayIconBuilder::new()
                .with_tooltip("ADB Wireless Tool")
                .with_menu(Box::new(tray_menu))
                .with_icon(icon)
                .build()
                .expect("failed to build tray icon");

            // Win32 消息泵 — 阻塞等待消息
            unsafe {
                let mut msg = std::mem::zeroed();
                while GetMessageW(&mut msg, 0, 0, 0) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            // _tray 在此析构 → NIM_DELETE + DestroyWindow
        })
        .expect("failed to spawn tray thread");

    // 等待后台线程写入 thread id（最多 5 秒）
    let start = Instant::now();
    while tid.load(Ordering::SeqCst) == 0 {
        if start.elapsed() > Duration::from_secs(5) {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    TrayHandle {
        thread_id: tid.load(Ordering::SeqCst),
        handle: Some(handle),
    }
}

/// 检查是否有托盘菜单事件。在主循环中调用。
pub fn check_event() -> Option<TrayEvent> {
    use tray_icon::menu::MenuEvent;
    loop {
        match MenuEvent::receiver().try_recv() {
            Ok(event) if event.id() == "exit" => return Some(TrayEvent::Exit),
            Err(_) => return None,
            _ => continue,
        }
    }
}

/// 生成 32x32 RGBA 蓝色圆形图标数据
fn create_default_icon_data() -> Vec<u8> {
    let size = 32;
    let cx = (size - 1) as f64 / 2.0;
    let cy = (size - 1) as f64 / 2.0;
    let radius = cx * 0.85;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f64 - cx) / radius;
            let dy = (y as f64 - cy) / radius;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= 1.0 {
                // 抗锯齿边缘
                let alpha = if dist > 0.9 {
                    ((1.0 - dist) / 0.1 * 255.0) as u8
                } else {
                    255
                };
                // Android 绿色风格 #3DDC84
                data.push(0x3D);
                data.push(0xDC);
                data.push(0x84);
                data.push(alpha);
            } else {
                data.push(0);
                data.push(0);
                data.push(0);
                data.push(0);
            }
        }
    }
    data
}
