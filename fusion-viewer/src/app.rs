//! winit 窗口应用：渲染视频帧 + 输入注入（自研客户端核心）。
//!
//! 交互设计对照 scrcpy 客户端：
//! - 左键 = 触摸（down/move/up），右键 = 返回，滚轮 = 滚动
//! - 键盘：字母/数字/常用键 → keycode 注入（带 Ctrl/Shift/Alt meta），
//!   可输入文本键 → INJECT_TEXT（支持中文等 UTF-8）
//! - ESC = 返回，窗口尺寸变化 → RESIZE_DISPLAY（弹性显示器）

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::args::Args;
use crate::control::{self, POINTER_ID_MOUSE, key_action, touch_action};
use crate::decoder::DecodedFrame;
use crate::server::ServerSession;
use crate::video::VideoEvent;
use crate::yuv::render_yuv420_to_rgbx;

/// Android 键盘 meta 标志（KeyEvent.META_*）。
mod meta {
    pub const SHIFT_ON: u32 = 0x1;
    pub const ALT_ON: u32 = 0x2;
    pub const CTRL_ON: u32 = 0x1000;
    pub const META_ON: u32 = 0x10000;
}

/// 滚轮每行对应的滚动像素（对照 scrcpy 默认手感）。
const SCROLL_LINES_TO_PIXELS: f32 = 8.0;

pub struct ViewerApp {
    args: Args,
    session: ServerSession,
    video_rx: Receiver<VideoEvent>,

    // 窗口与渲染（Arc<Window> 让 softbuffer Context 无生命周期问题）
    window: Option<Arc<Window>>,
    context: Option<Context<Arc<Window>>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    /// 窗口内容区尺寸（物理像素）
    window_size: (u32, u32),
    /// 最近一帧（渲染用）
    latest_frame: Option<DecodedFrame>,

    // 输入状态
    mouse_down: bool,
    mouse_pos: PhysicalPosition<f64>,
    /// 当前修饰键状态（ModifiersChanged 事件维护）
    modifiers: ModifiersState,
    /// 是否已发送 START_APP（收到首帧后一次性发送）
    started_app: bool,
    last_error: Option<String>,
}

impl ViewerApp {
    pub fn new(args: Args, session: ServerSession, video_rx: Receiver<VideoEvent>) -> Self {
        Self {
            args,
            session,
            video_rx,
            window: None,
            context: None,
            surface: None,
            window_size: (0, 0),
            latest_frame: None,
            mouse_down: false,
            mouse_pos: PhysicalPosition::new(0.0, 0.0),
            modifiers: ModifiersState::default(),
            started_app: false,
            last_error: None,
        }
    }

    /// 处理来自视频线程的事件（每次唤醒时调用）。
    fn drain_video_events(&mut self) {
        while let Ok(event) = self.video_rx.try_recv() {
            match event {
                VideoEvent::Frame(frame) => {
                    // 收到首帧 → 虚拟显示器已创建，此时才发 START_APP
                    // （server 侧 getStartAppDisplayId 只等 1s，过早发送会
                    //   "No known display id"）
                    if !self.started_app {
                        self.started_app = true;
                        match crate::control::start_app(&self.args.package) {
                            Ok(msg) => {
                                if let Err(e) = self.session.send(&msg) {
                                    eprintln!("[viewer] START_APP 发送失败: {}", e);
                                } else {
                                    println!("[viewer] 已请求启动 {}", self.args.package);
                                }
                            }
                            Err(e) => eprintln!("[viewer] {}", e),
                        }
                    }
                    self.latest_frame = Some(frame);
                }
                VideoEvent::Session {
                    width,
                    height,
                    client_resized,
                } => {
                    // 分辨率变化日志；渲染以解码帧自带的尺寸为准
                    println!(
                        "[viewer] 分辨率 {}x{} (client_resized={})",
                        width, height, client_resized
                    );
                }
                VideoEvent::Error(e) => {
                    eprintln!("[viewer] {}", e);
                    self.last_error = Some(e);
                }
                VideoEvent::Closed => {
                    eprintln!("[viewer] 视频流已结束");
                    self.last_error = Some("视频流已结束（设备断开或应用退出）".into());
                }
            }
        }
    }

    /// 渲染最新帧到窗口（letterbox 等比显示）。
    fn render(&mut self) {
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else {
            return;
        };
        let Some(frame) = self.latest_frame.as_ref() else {
            return;
        };
        let (win_w, win_h) = self.window_size;
        if win_w == 0 || win_h == 0 {
            return;
        }

        let vw = frame.width as f64;
        let vh = frame.height as f64;
        if vw == 0.0 || vh == 0.0 {
            return;
        }
        // letterbox：等比缩放到窗口内
        let scale = ((win_w as f64 / vw).min(win_h as f64 / vh)).max(0.001);
        let disp_w = (vw * scale).round() as usize;
        let disp_h = (vh * scale).round() as usize;
        let offset_x = (win_w as i64 - disp_w as i64).max(0) / 2;
        let offset_y = (win_h as i64 - disp_h as i64).max(0) / 2;

        let Some(nw) = u32::try_from(win_w)
            .ok()
            .and_then(|v| std::num::NonZeroU32::new(v))
        else {
            return;
        };
        let Some(nh) = u32::try_from(win_h)
            .ok()
            .and_then(|v| std::num::NonZeroU32::new(v))
        else {
            return;
        };
        if surface.resize(nw, nh).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        let pixels = buffer.as_mut();
        // 黑底
        pixels.fill(0);
        // 视频区域（先渲染到区域大小的临时缓冲，再整体 blit）
        let mut video_pixels = vec![0u32; disp_w * disp_h];
        render_yuv420_to_rgbx(
            &mut video_pixels,
            disp_w,
            disp_h,
            &frame.y,
            &frame.u,
            &frame.v,
            vw as usize,
            vh as usize,
        );
        let row_bytes = disp_w.min(win_w as usize);
        for row in 0..disp_h {
            let dst_row = (offset_y + row as i64) as usize;
            if dst_row >= win_h as usize {
                break;
            }
            let src_start = row * disp_w;
            let dst_start = dst_row * win_w as usize + offset_x as usize;
            pixels[dst_start..dst_start + row_bytes]
                .copy_from_slice(&video_pixels[src_start..src_start + row_bytes]);
        }
        let _ = window.request_redraw();
        let _ = buffer.present();
    }

    /// 窗口物理坐标 → 视频坐标（letterbox 逆变换）；在黑边外返回 None。
    fn to_video_coords(&self, x: f64, y: f64) -> Option<(i32, i32)> {
        let frame = self.latest_frame.as_ref()?;
        let (win_w, win_h) = self.window_size;
        if win_w == 0 || win_h == 0 {
            return None;
        }
        let vw = frame.width as f64;
        let vh = frame.height as f64;
        let scale = ((win_w as f64 / vw).min(win_h as f64 / vh)).max(0.001);
        let disp_w = vw * scale;
        let disp_h = vh * scale;
        let offset_x = (win_w as f64 - disp_w) / 2.0;
        let offset_y = (win_h as f64 - disp_h) / 2.0;
        let vx = (x - offset_x) / scale;
        let vy = (y - offset_y) / scale;
        if vx < 0.0 || vy < 0.0 || vx >= vw || vy >= vh {
            return None;
        }
        Some((vx as i32, vy as i32))
    }

    /// 当前视频分辨率（触摸消息的 screen_w/screen_h 字段）。
    fn video_size(&self) -> (u16, u16) {
        match &self.latest_frame {
            Some(f) => (
                f.width.min(u16::MAX as usize) as u16,
                f.height.min(u16::MAX as usize) as u16,
            ),
            None => (0, 0),
        }
    }

    fn send_touch(&mut self, action: u8, x: i32, y: i32) {
        let (sw, sh) = self.video_size();
        if sw == 0 || sh == 0 {
            return;
        }
        let msg = control::inject_touch(action, POINTER_ID_MOUSE, x, y, sw, sh, 1.0, 0, 0);
        let _ = self.session.send(&msg);
    }

    fn send_keycode(&mut self, action: u8, keycode: u32, modifiers: &ModifiersState) {
        let mut meta = 0u32;
        if modifiers.shift_key() {
            meta |= meta::SHIFT_ON;
        }
        if modifiers.control_key() {
            meta |= meta::CTRL_ON;
        }
        if modifiers.alt_key() {
            meta |= meta::ALT_ON;
        }
        if modifiers.super_key() {
            meta |= meta::META_ON;
        }
        let msg = control::inject_keycode(action, keycode, 0, meta);
        let _ = self.session.send(&msg);
    }

    fn handle_keyboard(
        &mut self,
        code: KeyCode,
        state: ElementState,
        text: Option<&str>,
        modifiers: &ModifiersState,
    ) {
        let action = match state {
            ElementState::Pressed => key_action::DOWN,
            ElementState::Released => key_action::UP,
        };
        // 优先文本输入（可输入的字符键，如字母/数字/中文 IME 输出）
        if state == ElementState::Pressed {
            if let Some(text) = text {
                let trimmed = text.trim();
                if !trimmed.is_empty() && !modifiers.control_key() && !modifiers.alt_key() {
                    let msg = control::inject_text(trimmed);
                    let _ = self.session.send(&msg);
                    return;
                }
            }
        }
        if let Some(keycode) = crate::keys::to_android_keycode(code) {
            self.send_keycode(action, keycode, modifiers);
        }
    }

    fn handle_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        let (sw, sh) = self.video_size();
        let Some((x, y)) = self.to_video_coords(self.mouse_pos.x, self.mouse_pos.y) else {
            return;
        };
        if sw == 0 || sh == 0 {
            return;
        }
        let (hscroll, vscroll) = match delta {
            MouseScrollDelta::LineDelta(h, v) => {
                (h * SCROLL_LINES_TO_PIXELS, v * SCROLL_LINES_TO_PIXELS)
            }
            MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
        };
        let msg = control::inject_scroll(x, y, sw, sh, hscroll, vscroll, 0);
        let _ = self.session.send(&msg);
    }
}

impl ApplicationHandler for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(self.args.window_title())
            .with_inner_size(LogicalSize::new(1280.0, 960.0));
        let window = match event_loop.create_window(attributes) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("创建窗口失败: {}", e);
                event_loop.exit();
                return;
            }
        };
        let context = Context::new(window.clone());
        let surface = context
            .as_ref()
            .ok()
            .and_then(|c| Surface::new(c, window.clone()).ok());
        if surface.is_none() {
            eprintln!("初始化渲染表面失败");
        }
        let size = window.inner_size();
        self.window_size = (size.width, size.height);
        self.context = context.ok();
        self.surface = surface;
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                self.window_size = (size.width, size.height);
                // 弹性显示器：窗口尺寸变化 → 调整虚拟显示器分辨率。
                // 首帧前虚拟显示器尚未创建，跳过（flex 模式 display 就绪后才可 resize）
                if self.latest_frame.is_some() && size.width > 0 && size.height > 0 {
                    let _ = self.session.resize_display(
                        size.width.min(u16::MAX as u32) as u16,
                        size.height.min(u16::MAX as u32) as u16,
                    );
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.drain_video_events();
                self.render();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = position;
                if self.mouse_down {
                    if let Some((x, y)) = self.to_video_coords(position.x, position.y) {
                        self.send_touch(touch_action::MOVE, x, y);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                match button {
                    MouseButton::Left => match state {
                        ElementState::Pressed => {
                            self.mouse_down = true;
                            if let Some((x, y)) =
                                self.to_video_coords(self.mouse_pos.x, self.mouse_pos.y)
                            {
                                self.send_touch(touch_action::DOWN, x, y);
                            }
                        }
                        ElementState::Released => {
                            self.mouse_down = false;
                            if let Some((x, y)) =
                                self.to_video_coords(self.mouse_pos.x, self.mouse_pos.y)
                            {
                                self.send_touch(touch_action::UP, x, y);
                            }
                        }
                    },
                    MouseButton::Right => {
                        // 右键 = 返回（scrcpy 默认）
                        let action = match state {
                            ElementState::Pressed => key_action::DOWN,
                            ElementState::Released => key_action::UP,
                        };
                        let _ = self.session.send(&control::back_or_screen_on(action));
                    }
                    _ => {}
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.handle_mouse_wheel(delta);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // physical_key 可能是未识别的键（PhysicalKey::Unidentified）
                let code = match event.physical_key {
                    PhysicalKey::Code(code) => code,
                    PhysicalKey::Unidentified(_) => return,
                };
                // 复制修饰键状态，避免同时可变/不可变借用 self
                let modifiers = self.modifiers;
                self.handle_keyboard(code, event.state, event.text.as_deref(), &modifiers);
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // 被视频线程 user event 唤醒或需要重绘时刷新
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, (): ()) {
        // 视频线程每解码一帧发送 user event 唤醒事件循环
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}
