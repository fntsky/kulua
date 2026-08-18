//! winit 窗口应用：渲染视频帧 + 输入注入（自研客户端核心）。
//!
//! 交互设计对照 scrcpy 客户端：
//! - 左键 = 触摸（down/move/up），右键 = 返回，滚轮 = 滚动
//! - 键盘：字母/数字/常用键 → keycode 注入（带 Ctrl/Shift/Alt meta），
//!   可输入文本键 → INJECT_TEXT（支持中文等 UTF-8）
//! - ESC = 返回，窗口尺寸变化 → RESIZE_DISPLAY（弹性显示器）

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use kulua_proto::generated::ctrl_msg;
use kulua_proto::session::{Event, UdpSession};

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::args::Args;
use crate::control::{self, POINTER_ID_MOUSE, key_action, touch_action};
use crate::decoder::DecodedFrame;
use crate::video::{VideoEvent, VideoInput};
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

/// 窗口尺寸轮询间隔：拖动时内尺寸连续变化，500ms 粒度足够捕捉
/// 最终尺寸，又不至于拖慢事件循环。
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 尺寸需连续稳定多少轮才发送 RESIZE_DISPLAY（500ms × 2 = 1s）。
const RESIZE_STABLE_TICKS: u32 = 2;

/// 虚拟显示器分辨率上限（单边）。
///
/// 目标分辨率始终取自**实时窗口物理尺寸**，但高 DPI 大屏 / `--scale>1` 会让
/// 结果远超 MediaCodec 的实际编码能力（多数设备 H.264 编码上限在 1080p~4K）：
/// 超大分辨率会导致编码器失败或输出异常（表现为“画面/缩放没反应”）。
const MAX_DISPLAY_DIMENSION: u32 = 3840;
/// 虚拟显示器总面积上限（4K：3840×2160）。
const MAX_DISPLAY_PIXELS: u64 = 3840u64 * 2160u64;

/// 把目标分辨率截到上限内：**统一缩放因子**，同时满足
/// 单边 ≤ `MAX_DISPLAY_DIMENSION` 与总面积 ≤ `MAX_DISPLAY_PIXELS`，且保持
/// 原始宽高比（避免高 DPI 大窗口 / scale>1 时分辨率爆到编码器能力之外）。
fn cap_display(w: u32, h: u32) -> (u32, u32) {
    let w = w.max(1);
    let h = h.max(1);
    // 各边上限的缩放因子
    let mut scale = 1.0f64;
    scale = scale.min(MAX_DISPLAY_DIMENSION as f64 / w as f64);
    scale = scale.min(MAX_DISPLAY_DIMENSION as f64 / h as f64);
    // 面积上限（4K）的缩放因子
    let area_scale = ((MAX_DISPLAY_PIXELS as f64) / (w as u64 * h as u64) as f64).sqrt();
    scale = scale.min(area_scale);
    if scale >= 1.0 {
        return (w, h); // 未超上限，原样返回
    }
    let nw = (w as f64 * scale).floor().max(1.0) as u32;
    let nh = (h as f64 * scale).floor().max(1.0) as u32;
    (nw, nh)
}

pub struct ViewerApp {
    args: Args,
    /// UDP 会话（事件流消费者 + 输入注入发送器）
    session: UdpSession,
    /// server 分配的虚拟显示器 id
    display_id: u32,
    /// 把 UDP 会话的媒体/config 事件转发给解码线程
    video_input_tx: Sender<VideoInput>,
    video_rx: Receiver<VideoEvent>,

    // 窗口与渲染（Arc<Window> 让 softbuffer Context 无生命周期问题）
    window: Option<Arc<Window>>,
    context: Option<Context<Arc<Window>>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    /// 窗口内容区尺寸（物理像素）
    window_size: (u32, u32),
    /// 上次轮询窗口尺寸的时间（每 500ms 轮询一次）
    last_resize_check: Instant,
    /// 最近一轮轮询到的缩放后尺寸（防抖候选）
    observed_size: (u32, u32),
    /// 候选尺寸已连续稳定观察到的轮数（达到 RESIZE_STABLE_TICKS 才发送）
    stable_ticks: u32,
    /// 上次发送给 server 的显示器尺寸（物理像素）。
    /// 轮询到窗口尺寸与它不同才发 RESIZE_DISPLAY，避免重复发送
    last_sent_size: (u32, u32),
    /// 最近一帧（渲染用）
    latest_frame: Option<DecodedFrame>,
    /// viewer 启动时刻（用于打印首帧耗时，诊断"应用启动很久"）
    started_at: Instant,

    // 输入状态
    mouse_down: bool,
    mouse_pos: PhysicalPosition<f64>,
    /// 当前修饰键状态（ModifiersChanged 事件维护）
    modifiers: ModifiersState,
    last_error: Option<String>,
    /// 会话致命错误/关闭时置位，触发窗口退出（避免"看起来活着其实是死的"）
    should_exit: bool,
}

impl ViewerApp {
    pub fn new(
        args: Args,
        session: UdpSession,
        display_id: u32,
        video_input_tx: Sender<VideoInput>,
        video_rx: Receiver<VideoEvent>,
    ) -> Self {
        Self {
            args,
            session,
            display_id,
            video_input_tx,
            video_rx,
            window: None,
            context: None,
            surface: None,
            window_size: (0, 0),
            last_resize_check: Instant::now(),
            observed_size: (0, 0),
            stable_ticks: 0,
            last_sent_size: (0, 0), // (0,0) 保证首次轮询必然发送
            latest_frame: None,
            started_at: Instant::now(),
            mouse_down: false,
            mouse_pos: PhysicalPosition::new(0.0, 0.0),
            modifiers: ModifiersState::default(),
            last_error: None,
            should_exit: false,
        }
    }

    /// 关闭 UDP 会话（发 BYE；main 在事件循环退出后调用）。
    pub fn shutdown(&mut self) {
        self.session.close();
    }

    /// 消费 UDP 会话事件：视频/config 转发给解码线程，其余忽略/错误处理。
    fn drain_session(&mut self) {
        while let Some(event) = self.session.try_recv() {
            match event {
                Event::Video(frame) => {
                    let _ = self.video_input_tx.send(VideoInput::Frame(frame));
                }
                Event::Control(msg) => {
                    // 视频 codec config（H.264 SPS/PPS）走可靠 MediaConfig
                    if let Some(ctrl_msg::Msg::MediaConfig(cfg)) = msg.msg {
                        if cfg.stream == 2 {
                            let _ = self.video_input_tx.send(VideoInput::Config(cfg.data));
                        }
                    }
                    // 剪贴板等事件由 session 负责，viewer 忽略（保持 drain）
                }
                Event::Audio(_) => {}
                Event::Error(e) => {
                    eprintln!("[viewer] 会话错误: {}", e);
                    self.last_error = Some(e);
                }
                Event::Closed => {
                    eprintln!("[viewer] 会话已关闭（设备断开或 server 退出）");
                    self.last_error = Some("设备断开或 server 退出".into());
                }
                _ => {}
            }
        }
    }

    /// 处理来自视频线程的事件（每次唤醒时调用）。
    fn drain_video_events(&mut self) {
        while let Ok(event) = self.video_rx.try_recv() {
            match event {
                VideoEvent::Frame(frame) => {
                    // 首帧计时：诊断"应用启动很久"——区分 viewer/连接耗时、
                    // am 子进程耗时与应用自身冷启动耗时（对照 server 日志）
                    if self.latest_frame.is_none() {
                        println!(
                            "[viewer] 首帧到达（自 viewer 启动 {:.1}s）",
                            self.started_at.elapsed().as_secs_f32()
                        );
                    }
                    // START_APP 已在连接后由 main 发送（displayId 握手时即返回，
                    // 空显示器无帧，等首帧会死锁）
                    self.latest_frame = Some(frame);
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
    fn video_size(&self) -> (u32, u32) {
        match &self.latest_frame {
            Some(f) => (
                f.width.min(u32::MAX as usize) as u32,
                f.height.min(u32::MAX as usize) as u32,
            ),
            None => (0, 0),
        }
    }

    fn send_touch(&mut self, action: u8, x: i32, y: i32) {
        let (sw, sh) = self.video_size();
        if sw == 0 || sh == 0 {
            return;
        }
        let msg = control::inject_touch(
            self.display_id,
            action as u32,
            POINTER_ID_MOUSE,
            x,
            y,
            sw,
            sh,
            1.0,
            0,
            0,
        );
        let _ = self.session.send_ctrl(&msg);
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
        let msg = control::inject_keycode(self.display_id, action as u32, keycode, 0, meta);
        let _ = self.session.send_ctrl(&msg);
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
                    let msg = control::inject_text(self.display_id, trimmed);
                    let _ = self.session.send_ctrl(&msg);
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
        let msg = control::inject_scroll(self.display_id, x, y, sw, sh, hscroll, vscroll, 0);
        let _ = self.session.send_ctrl(&msg);
    }

    /// 轮询窗口当前尺寸，稳定后发 RESIZE_DISPLAY。
    ///
    /// 为什么轮询而非依赖 Resized 事件：窗口拖动/系统缩放时 Resized 事件
    /// 可能不触发或触发不稳定（实测缩放后分辨率不跟随），轮询窗口实际
    /// 物理尺寸能稳定收敛。首帧前虚拟显示器未就绪，跳过。
    ///
    /// 为什么防抖（稳定 N 轮才发）：拖动窗口边缘时 inner_size 会连续变化，
    /// 若每轮都立即发送，server 每次都要重建 MediaCodec（停线程→release→
    /// 重建→重启，几百 ms 断流）且客户端要重建 FFmpeg 解码器——拖动期间
    /// 连续重建导致画面闪烁/花屏（实测"分辨率不能稳定变换"）。要求尺寸
    /// 连续稳定 RESIZE_STABLE_TICKS 轮（500ms × 2 = 1s）才发送：
    /// 拖动中一次也不发，松手后只发最终尺寸一次，画面平稳切换。
    fn poll_window_size(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if self.latest_frame.is_none() {
            return; // 首帧前显示器未就绪
        }
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        // 虚拟显示器分辨率 = 窗口物理尺寸 × 缩放系数（--scale）。
        // 为什么乘以系数：窗口尺寸就是用户看到的画面大小，但把虚拟显示器
        // 分辨率设成与窗口完全一致可能超出 MediaCodec 编码能力上限或带宽
        // 预算，系数 <1 时以更低分辨率编码、由 FFmpeg 放大到窗口，画质
        // 略降但流畅度与带宽更可控；>1 则超采样更清晰。
        // 每轮都以 window.inner_size()（物理像素）实时计算，再截到编码能力上限。
        let w = ((size.width as f32 * self.args.scale).round() as u32).max(1);
        let h = ((size.height as f32 * self.args.scale).round() as u32).max(1);
        let (w, h) = cap_display(w, h);
        let current = (w, h);
        if current == self.last_sent_size {
            self.observed_size = current; // 与已发送一致，无操作
            self.stable_ticks = 0;
            return;
        }
        if current != self.observed_size {
            // 尺寸又变了：拖动中，重新计时，不发送
            self.observed_size = current;
            self.stable_ticks = 0;
            return;
        }
        self.stable_ticks += 1;
        if self.stable_ticks < RESIZE_STABLE_TICKS {
            return; // 稳定时间不足，继续等
        }
        // 尺寸已稳定：发送。成功才记录 last_sent（失败下轮重试）
        self.stable_ticks = 0;
        if self
            .session
            .send_ctrl(&control::resize_display(
                self.display_id,
                w as u32,
                h as u32,
            ))
            .is_ok()
        {
            self.last_sent_size = current;
        }
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
        // 启用 IME：否则 Windows 输入法（微软拼音等）无法在窗口唤起
        // （winit 0.30 无 with_ime_allowed 属性，只能创建后设置）
        window.set_ime_allowed(true);
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
                // 只更新渲染尺寸；RESIZE_DISPLAY 由每秒轮询统一发送——
                // Resized 事件在拖动/DPI 变化时可能丢事件或不稳定触发，
                // 轮询窗口实际尺寸更可靠（见 poll_window_size）
                self.window_size = (size.width, size.height);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.drain_session();
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
                        let _ = self
                            .session
                            .send_ctrl(&control::back_or_screen_on(self.display_id, action as u32));
                    }
                    _ => {}
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.handle_mouse_wheel(delta);
            }
            WindowEvent::Ime(ime) => {
                // 中文输入：IME 提交的文本 → INJECT_TEXT。
                // 非 ASCII 由 server 内部走剪贴板+PASTE 粘贴注入
                // （KeyCharacterMap 无法映射中文）。
                // 注意：IME 启用后字母键也会以 Commit 形式提交，
                // 与 KeyboardInput 的 text 双通道重复注入——这里只处理
                // 含非 ASCII 的提交（纯 ASCII 由 KeyboardInput 注入）
                if let Ime::Commit(text) = ime {
                    if !text.is_empty() && !text.is_ascii() {
                        let msg = control::inject_text(self.display_id, &text);
                        let _ = self.session.send_ctrl(&msg);
                    }
                }
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

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // 被视频线程 user event 唤醒或需要重绘时刷新
        self.drain_session();
        if self.should_exit {
            // 会话已死：结束事件循环，避免窗口僵死
            event_loop.exit();
            return;
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        // 每 500ms 轮询窗口尺寸 → 弹性显示器 resize（稳定 1s 才发，见 poll_window_size）
        if self.last_resize_check.elapsed() >= RESIZE_POLL_INTERVAL {
            self.last_resize_check = Instant::now();
            self.poll_window_size();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, (): ()) {
        // 视频线程每解码一帧发送 user event 唤醒事件循环
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::cap_display;

    #[test]
    fn cap_keeps_normal_sizes_unchanged() {
        assert_eq!(cap_display(1280, 960), (1280, 960));
        assert_eq!(cap_display(1920, 1080), (1920, 1080));
        assert_eq!(cap_display(1, 1), (1, 1));
    }

    #[test]
    fn cap_clamps_single_dimension() {
        // 8192 超宽 → 统一缩小，单边 ≤3840 且保持宽高比
        let (w, h) = cap_display(8192, 1080);
        assert_eq!(w, 3840, "宽截到上限");
        assert!(h < 1080, "超高景宽比 → 高按比例缩小, got {w}x{h}");
        // 比例 ≈ 8192/1080
        assert!(
            ((w as f64 / h as f64) - 8192.0 / 1080.0).abs() < 0.02,
            "应等比, got {w}x{h}"
        );

        let (w2, h2) = cap_display(1080, 8192);
        assert_eq!(h2, 3840, "高截到上限");
        assert!(
            ((w2 as f64 / h2 as f64) - 1080.0 / 8192.0).abs() < 0.02,
            "应等比, got {w2}x{h2}"
        );
    }

    #[test]
    fn cap_scales_down_when_area_exceeds_4k() {
        // 3840x3840 > 4K → 等比缩到面积 ≤ 4K
        let (w, h) = cap_display(3840, 3840);
        assert!(w as u64 * h as u64 <= 3840u64 * 2160u64, "面积应 ≤ 4K");
        // 宽高比基本保持（±1 取整误差）
        let ratio = w as f64 / h as f64;
        assert!((ratio - 1.0).abs() < 0.01, "应等比缩放, got {w}x{h}");

        let (w2, h2) = cap_display(7680, 4320); // 8K 16:9 → 等比缩到 ≤4K
        assert_eq!((w2, h2), (3840, 2160), "8K 应等比缩到 4K");
    }

    #[test]
    fn cap_never_zero() {
        let (w, h) = cap_display(0, 0);
        assert!(w >= 1 && h >= 1);
    }
}
