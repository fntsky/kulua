//! eframe/egui 窗口应用：渲染视频帧 + 输入注入 + HUD 调试面板。
//!
//! 视频帧由解码线程产出 RGBA，UI 线程上传 egui 纹理并按窗口 letterbox 显示；
//! HUD（egui 窗口）实时显示码率 / 帧率 / 丢包率 / 分辨率，便于诊断卡死/花屏。

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align2, Color32, Key, PointerButton, Rect, RichText, Sense, Vec2};

use kulua_proto::generated::ctrl_msg;
use kulua_proto::session::{Event, UdpSession};

use crate::args::Args;
use crate::control::{self, POINTER_ID_MOUSE, touch_action};
use crate::video::{VideoEvent, VideoInput};

/// 尝试加载系统中文字体并安装到 egui；成功返回 true。
///
/// WHY：egui 默认字体只有拉丁字形，中文会显示乱码/方块。Windows 基本必有
/// msyh.ttc（微软雅黑）/ simhei.ttf；.ttc 通过 FontData.index 取集合首张。
pub fn setup_fonts(ctx: &egui::Context) -> bool {
    let Some((data, name)) = load_cjk_font() else {
        return false;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert(name.clone(), std::sync::Arc::new(data));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push(name.clone());
    }
    ctx.set_fonts(fonts);
    true
}

/// 依平台常见路径探测中文字体（.ttf 优先，.ttc 需 epaint 支持集合索引）。
fn load_cjk_font() -> Option<(egui::FontData, String)> {
    let candidates: &[(&str, &str)] = &[
        // Windows
        ("C:\\Windows\\Fonts\\msyh.ttc", "msyh"),
        ("C:\\Windows\\Fonts\\simhei.ttf", "simhei"),
        ("C:\\Windows\\Fonts\\simsun.ttc", "simsun"),
        // macOS / Linux
        ("/System/Library/Fonts/PingFang.ttc", "pingfang"),
        ("/System/Library/Fonts/STHeiti Medium.ttc", "stheiti"),
        (
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "noto-cjk",
        ),
        (
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "wqymicrohei",
        ),
    ];
    for (path, name) in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        return Some((egui::FontData::from_owned(bytes), (*name).to_string()));
    }
    None
}

/// 每 500ms 轮询窗口尺寸。
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 尺寸需连续稳定多少轮才发送 RESIZE_DISPLAY。
const RESIZE_STABLE_TICKS: u32 = 2;
/// 渲染/上传纹理节流（≤60fps），防止 UI 线程被 present/上传占死。
const MIN_UPLOAD_INTERVAL: Duration = Duration::from_millis(16);
/// 虚拟显示器分辨率上限（单边，2K = 2560）。
const MAX_DISPLAY_DIMENSION: u32 = 2560;
/// 总面积上限（2K：2560×1440）。
const MAX_DISPLAY_PIXELS: u64 = 2560u64 * 1440u64;

/// 把目标分辨率截到上限内（统一缩放、保持宽高比）。
fn cap_display(w: u32, h: u32) -> (u32, u32) {
    let w = w.max(1);
    let h = h.max(1);
    let mut scale = 1.0f64;
    scale = scale.min(MAX_DISPLAY_DIMENSION as f64 / w as f64);
    scale = scale.min(MAX_DISPLAY_DIMENSION as f64 / h as f64);
    let area_scale = ((MAX_DISPLAY_PIXELS as f64) / (w as u64 * h as u64) as f64).sqrt();
    scale = scale.min(area_scale);
    if scale >= 1.0 {
        return (w, h);
    }
    let nw = (w as f64 * scale).floor().max(1.0) as u32;
    let nh = (h as f64 * scale).floor().max(1.0) as u32;
    (nw, nh)
}

/// 目标分辨率 = 窗口 ÷ scale，套 2K 上限，对齐偶数（H.264 宏块要求）。
fn target_display(window_w: u32, window_h: u32, scale: f32) -> (u32, u32) {
    let w = (window_w as f32 / scale).round() as u32;
    let h = (window_h as f32 / scale).round() as u32;
    let (w, h) = cap_display(w.max(1), h.max(1));
    (even_dim(w), even_dim(h))
}

/// 对齐到不小于 2 的偶数（H.264 宏块对齐）。
fn even_dim(v: u32) -> u32 {
    (v.max(2)) & !1
}

/// 由解码线程交付的 RGB 帧。
struct VideoFrame {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

/// HUD 统计（每秒推进一次窗口，锁内采样 session 丢包）。
struct Hud {
    window_start: Instant,
    rx_bytes: u64,
    seen: u64,
    disp: u64,
    prev_lost: u64,
    // 上窗口显示值
    fps: f64,
    bitrate_mbps: f64,
    loss_pct: f64,
    /// 累计丢失帧数（HUD 展示，直观感受丢包）。
    lost_total: u64,
}

/// 弹性显示器 resize 防抖状态。
struct ResizeState {
    last_check: Instant,
    observed: (u32, u32),
    stable: u32,
    last_sent: (u32, u32),
}

impl ResizeState {
    fn new() -> Self {
        Self {
            last_check: Instant::now(),
            observed: (0, 0),
            stable: 0,
            last_sent: (0, 0),
        }
    }
}

/// 窗口退出时向 phone 发 BYE（eframe 消费了 App，无法在 main 里收尾，用 Drop）。
impl Drop for ViewerApp {
    fn drop(&mut self) {
        self.session.close();
    }
}

pub struct ViewerApp {
    args: Args,
    session: UdpSession,
    display_id: u32,
    video_input_tx: SyncSender<VideoInput>,
    video_rx: Receiver<VideoEvent>,

    latest: Option<VideoFrame>,
    texture: Option<egui::TextureHandle>,
    last_upload: Instant,
    video_rect: Option<Rect>,

    hud: Hud,
    resize: ResizeState,
    started_at: Instant,

    // 输入
    mouse_down: bool,
    pressed_keys: HashSet<Key>,

    should_exit: bool,
    last_error: Option<String>,
    /// 是否加载到中文字体（false 时 HUD 用英文标签，避免乱码）。
    has_cjk: bool,
    /// 首帧超时诊断：已提示过 / 已自动重发 START_APP。
    no_frame_alerted: bool,
    start_app_resent: bool,
}

impl ViewerApp {
    pub fn new(
        args: Args,
        session: UdpSession,
        display_id: u32,
        video_input_tx: SyncSender<VideoInput>,
        video_rx: Receiver<VideoEvent>,
    ) -> Self {
        Self {
            args,
            session,
            display_id,
            video_input_tx,
            video_rx,
            latest: None,
            texture: None,
            last_upload: Instant::now(),
            video_rect: None,
            hud: Hud {
                window_start: Instant::now(),
                rx_bytes: 0,
                seen: 0,
                disp: 0,
                prev_lost: 0,
                fps: 0.0,
                bitrate_mbps: 0.0,
                loss_pct: 0.0,
                lost_total: 0,
            },
            resize: ResizeState::new(),
            started_at: Instant::now(),
            mouse_down: false,
            pressed_keys: HashSet::new(),
            should_exit: false,
            last_error: None,
            has_cjk: false,
            no_frame_alerted: false,
            start_app_resent: false,
        }
    }

    /// 设置是否已加载中文字体（HUD 据此切中文/英文标签）。
    pub fn set_has_cjk(&mut self, has: bool) {
        self.has_cjk = has;
    }

    /// 消费 UDP 会话事件 → 解码线程 + HUD 计数。
    fn drain_session(&mut self) {
        while let Some(event) = self.session.try_recv() {
            match event {
                Event::Video(frame) => {
                    self.hud.seen += 1;
                    self.hud.rx_bytes += frame.data.len() as u64;
                    let _ = self.video_input_tx.try_send(VideoInput::Frame(frame));
                }
                Event::Control(msg) => {
                    if let Some(ctrl_msg::Msg::MediaConfig(cfg)) = msg.msg {
                        if cfg.stream == 2 {
                            let _ = self.video_input_tx.try_send(VideoInput::Config(cfg.data));
                        }
                    }
                    // 剪贴板等由 session 负责，viewer 忽略（保持 drain）
                }
                Event::Error(e) => {
                    eprintln!("[viewer] 会话错误: {}", e);
                    self.last_error = Some(e);
                    self.should_exit = true;
                }
                Event::Closed => {
                    self.last_error = Some("会话已关闭（设备断开或 server 退出）".into());
                    self.should_exit = true;
                }
                _ => {}
            }
        }
    }

    /// 消费解码线程输出。
    fn drain_video(&mut self) {
        while let Ok(event) = self.video_rx.try_recv() {
            match event {
                VideoEvent::Frame {
                    width,
                    height,
                    rgba,
                } => {
                    if self.latest.is_none() {
                        println!(
                            "[viewer] 首帧到达（自 viewer 启动 {:.1}s）",
                            self.started_at.elapsed().as_secs_f32()
                        );
                    }
                    self.latest = Some(VideoFrame {
                        width,
                        height,
                        rgba,
                    });
                }
                VideoEvent::Error(e) => {
                    eprintln!("[viewer] {}", e);
                    self.last_error = Some(e);
                }
                VideoEvent::Closed => {
                    self.last_error = Some("视频流已结束（设备断开或应用退出）".into());
                }
            }
        }
    }

    /// 首帧看门狗：无视频帧时先重发 START_APP，再于 18s 给出终态告警。
    ///
    /// WHY：个别应用 `am start` 偶发失败/被忽略（尤其非系统应用），设备端会有
    /// `[kulua] startApp ... exit=...` 日志；这里兜底重发一次 + 可见告警，
    /// 而不是永久“卡在等待首帧”。
    fn watchdog_no_frame(&mut self) {
        if self.latest.is_some() || self.no_frame_alerted {
            return;
        }
        let elapsed = self.started_at.elapsed();
        if !self.start_app_resent && elapsed >= Duration::from_secs(8) {
            self.start_app_resent = true;
            let pkg = self.args.package.clone();
            println!("[viewer] 8s 无首帧，重发 START_APP {}", pkg);
            if let Err(e) = self
                .session
                .send_ctrl(&control::start_app(self.display_id, &pkg))
            {
                eprintln!("[viewer] 重发 START_APP 失败: {}", e);
            }
            self.last_error = Some(
                "应用 8s 未出首帧，已重发启动（仍无画面请看设备端 [kulua] startApp 日志）".into(),
            );
            return;
        }
        if self.start_app_resent && elapsed >= Duration::from_secs(18) {
            self.no_frame_alerted = true;
            let msg = "应用启动后 18s 仍无视频帧：am start 失败，或该应用不渲染到虚拟显示器（见设备端 [kulua] startApp 日志）";
            eprintln!("[viewer] {msg}");
            self.last_error = Some(msg.into());
        }
    }

    /// HUD 统计（250ms 窗口，实时刷新；丢包率 = 窗口内新增丢失 / (收到 + 新增丢失)）。
    fn tick_hud(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.hud.window_start) < Duration::from_millis(250) {
            return;
        }
        let secs = now
            .duration_since(self.hud.window_start)
            .as_secs_f64()
            .max(0.001);
        let lost_now = self.session.video_lost();
        let lost_delta = lost_now.saturating_sub(self.hud.prev_lost);
        self.hud.prev_lost = lost_now;
        self.hud.lost_total = lost_now;

        self.hud.fps = self.hud.disp as f64 / secs;
        self.hud.bitrate_mbps = self.hud.rx_bytes as f64 * 8.0 / 1e6 / secs;
        let recv = self.hud.seen;
        self.hud.loss_pct = if recv + lost_delta > 0 {
            lost_delta as f64 / (recv as f64 + lost_delta as f64) * 100.0
        } else {
            0.0
        };

        self.hud.window_start = now;
        self.hud.rx_bytes = 0;
        self.hud.seen = 0;
        self.hud.disp = 0;
    }

    /// 更新纹理（新帧 ≥16ms 才上传，节流 UI 线程）。
    fn upload_texture(&mut self, ctx: &egui::Context) {
        let Some(frame) = self.latest.as_ref() else {
            return;
        };
        if self.last_upload.elapsed() < MIN_UPLOAD_INTERVAL {
            return;
        }
        self.last_upload = Instant::now();
        let image =
            egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
        let tex = self.texture.get_or_insert_with(|| {
            ctx.load_texture("video", image.clone(), egui::TextureOptions::LINEAR)
        });
        tex.set(image, egui::TextureOptions::LINEAR);
    }

    /// 视频坐标换算（letterbox 逆变换）。
    fn to_video_coords(&self, pos: egui::Pos2) -> Option<(i32, i32)> {
        let frame = self.latest.as_ref()?;
        let rect = self.video_rect?;
        if !rect.contains(pos) {
            return None;
        }
        let rel = pos - rect.min;
        let rx = (rel.x / rect.width()).clamp(0.0, 1.0);
        let ry = (rel.y / rect.height()).clamp(0.0, 1.0);
        Some((
            (rx * frame.width as f32) as i32,
            (ry * frame.height as f32) as i32,
        ))
    }

    fn video_dims(&self) -> (u32, u32) {
        match &self.latest {
            Some(f) => (f.width as u32, f.height as u32),
            None => (0, 0),
        }
    }

    fn send_touch(&mut self, action: u8, x: i32, y: i32) {
        let (sw, sh) = self.video_dims();
        if sw == 0 || sh == 0 {
            return;
        }
        let _ = self.session.send_ctrl(&control::inject_touch(
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
        ));
    }

    fn handle_touch(&mut self, ctx: &egui::Context, resp: &egui::Response, rect: Rect) {
        let down = resp.is_pointer_button_down_on();
        let pos = ctx.input(|i| i.pointer.interact_pos());
        if down {
            if !self.mouse_down {
                self.mouse_down = true;
                if let Some(p) = pos {
                    if let Some((x, y)) = self.to_video_coords(p) {
                        self.send_touch(touch_action::DOWN, x, y);
                    }
                }
            } else if let Some(p) = pos {
                if let Some((x, y)) = self.to_video_coords(p) {
                    self.send_touch(touch_action::MOVE, x, y);
                }
            }
        } else if self.mouse_down {
            self.mouse_down = false;
            let p = pos.unwrap_or(rect.center());
            if let Some((x, y)) = self.to_video_coords(p) {
                self.send_touch(touch_action::UP, x, y);
            }
        }
    }

    /// 滚轮 → 滚动注入；右键 → 返回。
    fn handle_extra(&mut self, ctx: &egui::Context) {
        // 滚轮（仅当悬停在视频区）
        let hover = self
            .video_rect
            .as_ref()
            .is_some_and(|r| ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| r.contains(p))));
        if hover {
            let delta = ctx.input(|i| i.raw_scroll_delta);
            if delta.y.abs() > 0.0 {
                if let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
                    if let Some((x, y)) = self.to_video_coords(p) {
                        let (sw, sh) = self.video_dims();
                        if sw > 0 {
                            let _ = self.session.send_ctrl(&control::inject_scroll(
                                self.display_id,
                                x,
                                y,
                                sw,
                                sh,
                                0.0,
                                delta.y * 8.0,
                                0,
                            ));
                        }
                    }
                }
            }
        }
        // 右键 = 返回
        let down = ctx.input(|i| i.pointer.button_pressed(PointerButton::Secondary));
        let up = ctx.input(|i| i.pointer.button_released(PointerButton::Secondary));
        if down {
            let _ = self.session.send_ctrl(&control::back_or_screen_on(
                self.display_id,
                crate::control::key_action::DOWN as u32,
            ));
        }
        if up {
            let _ = self.session.send_ctrl(&control::back_or_screen_on(
                self.display_id,
                crate::control::key_action::UP as u32,
            ));
        }
    }

    /// 键盘：可打印字符走文本注入；特殊键 keycode（按下/抬起配对）。
    fn handle_keys(&mut self, ctx: &egui::Context) {
        let mods = ctx.input(|i| i.modifiers);
        // 文本（含 IME 中文提交）
        let events: Vec<egui::Event> = ctx.input(|i| i.events.clone());
        for ev in events {
            if let egui::Event::Text(s) = ev {
                let s = s.trim().to_string();
                if !s.is_empty() && !mods.ctrl && !mods.alt && !mods.command {
                    let _ = self
                        .session
                        .send_ctrl(&control::inject_text(self.display_id, &s));
                }
            }
        }
        // 特殊键
        for &key in crate::keys::MAPPED_KEYS {
            if ctx.input(|i| i.key_pressed(key)) {
                if let Some(kc) = crate::keys::to_android_keycode(key) {
                    let _ = self.session.send_ctrl(&control::inject_keycode(
                        self.display_id,
                        crate::control::key_action::DOWN as u32,
                        kc,
                        0,
                        0,
                    ));
                    self.pressed_keys.insert(key);
                }
            } else if self.pressed_keys.contains(&key) && ctx.input(|i| i.key_released(key)) {
                self.pressed_keys.remove(&key);
                if let Some(kc) = crate::keys::to_android_keycode(key) {
                    let _ = self.session.send_ctrl(&control::inject_keycode(
                        self.display_id,
                        crate::control::key_action::UP as u32,
                        kc,
                        0,
                        0,
                    ));
                }
            }
        }
    }

    /// 每 500ms 轮询窗口尺寸 → 稳定后发 RESIZE_DISPLAY（目标 = 窗口÷scale，偶对齐）。
    fn poll_window_size(&mut self, ctx: &egui::Context) {
        if self.resize.last_check.elapsed() < RESIZE_POLL_INTERVAL {
            return;
        }
        self.resize.last_check = Instant::now();
        if self.latest.is_none() {
            return;
        }
        let size = ctx.input(|i| i.screen_rect().size());
        if size.x <= 1.0 || size.y <= 1.0 {
            return;
        }
        let current = target_display(size.x as u32, size.y as u32, self.args.scale);
        if current == self.resize.last_sent {
            self.resize.observed = current;
            self.resize.stable = 0;
            return;
        }
        if current != self.resize.observed {
            self.resize.observed = current;
            self.resize.stable = 0;
            return;
        }
        self.resize.stable += 1;
        if self.resize.stable < RESIZE_STABLE_TICKS {
            return;
        }
        self.resize.stable = 0;
        if self
            .session
            .send_ctrl(&control::resize_display(
                self.display_id,
                current.0,
                current.1,
            ))
            .is_ok()
        {
            self.resize.last_sent = current;
        }
    }

    fn hud_ui(&mut self, ctx: &egui::Context) {
        let (vx, vy) = self.video_dims();
        egui::Window::new("调试 · Kulua")
            .anchor(Align2::LEFT_TOP, Vec2::new(10.0, 10.0))
            .collapsible(true)
            .resizable(false)
            .pivot(Align2::LEFT_TOP)
            .show(ctx, |ui| {
                egui::Grid::new("hud-grid")
                    .num_columns(2)
                    .spacing(Vec2::new(12.0, 4.0))
                    .show(ui, |ui| {
                        ui.label("显示帧率");
                        ui.label(RichText::new(format!("{:.0} fps", self.hud.fps)).monospace());
                        ui.end_row();
                        ui.label("视频码率");
                        ui.label(
                            RichText::new(format!("{:.2} Mbps", self.hud.bitrate_mbps)).monospace(),
                        );
                        ui.end_row();
                        ui.label("丢包率");
                        ui.label(RichText::new(format!("{:.1}%", self.hud.loss_pct)).monospace());
                        ui.end_row();
                        ui.label("分辨率");
                        ui.label(RichText::new(format!("{vx}×{vy}")).monospace());
                        ui.end_row();
                        ui.label("目标尺寸");
                        ui.label(RichText::new(format!("{:?}", self.resize.last_sent)).monospace());
                        ui.end_row();
                        let st = if self.should_exit {
                            "退出中"
                        } else {
                            "运行中"
                        };
                        ui.label("状态");
                        ui.label(st);
                        ui.end_row();
                    });
                if let Some(e) = &self.last_error {
                    ui.colored_label(Color32::RED, e);
                }
            });
    }
}

impl eframe::App for ViewerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_session();
        self.drain_video();
        self.tick_hud();
        if self.should_exit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.watchdog_no_frame();

        // 视频
        {
            let mut ui_rect = None;
            egui::CentralPanel::default().show(ctx, |ui| {
                let avail = ui.available_size();
                if let Some(frame) = &self.latest {
                    let aspect = frame.width as f32 / frame.height as f32;
                    let fit = letterbox(avail, aspect);
                    let (rect, _) =
                        ui.allocate_exact_size(fit.max(Vec2::splat(2.0)), Sense::hover());
                    if let Some(tex) = &self.texture {
                        let img = egui::Image::new((tex.id(), fit));
                        ui.put(rect, img);
                    } else {
                        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
                        ui.put(
                            rect,
                            egui::Label::new(if self.has_cjk {
                                "加载中…"
                            } else {
                                "Loading…"
                            })
                            .selectable(false),
                        );
                    }
                    self.video_rect = Some(rect);
                    ui_rect = Some(rect);
                    let resp =
                        ui.interact(rect, ui.id().with("video_touch"), Sense::click_and_drag());
                    self.handle_touch(ctx, &resp, rect);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                        ui.label(if self.has_cjk {
                            "等待视频帧…"
                        } else {
                            "Waiting for video…"
                        });
                    });
                }
            });
            let _ = ui_rect;
        }
        self.upload_texture(ctx);
        self.tick_upload_counter();
        self.hud_ui(ctx);
        self.handle_extra(ctx);
        self.handle_keys(ctx);
        self.poll_window_size(ctx);

        // 持续刷新（HUD 实时 + 事件轮询）
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

/// 计一次“本次呈现”用于帧率统计。
impl ViewerApp {
    fn tick_upload_counter(&mut self) {
        self.hud.disp += 1;
    }
}

/// letterbox 尺寸：在可用区域内按宽高比最大化。
fn letterbox(avail: Vec2, aspect: f32) -> Vec2 {
    if aspect <= 0.0 || avail.x <= 1.0 || avail.y <= 1.0 {
        return avail.max(Vec2::splat(1.0));
    }
    let mut w = avail.x;
    let mut h = w / aspect;
    if h > avail.y {
        h = avail.y;
        w = h * aspect;
    }
    Vec2::new(w, h)
}

#[cfg(test)]
mod tests {
    use super::{cap_display, even_dim, letterbox, target_display};
    use egui::Vec2;

    #[test]
    fn cap_keeps_normal_sizes_unchanged() {
        assert_eq!(cap_display(1280, 960), (1280, 960));
        assert_eq!(cap_display(1920, 1080), (1920, 1080));
    }

    #[test]
    fn cap_scales_down_when_area_exceeds_2k() {
        let (w, h) = cap_display(3840, 3840);
        assert!(w as u64 * h as u64 <= 2560u64 * 1440u64);
        let (w2, h2) = cap_display(7680, 4320);
        assert_eq!((w2, h2), (2560, 1440));
    }

    #[test]
    fn target_always_even_for_h264() {
        assert_eq!(target_display(2240, 1680, 1.5), (1492, 1120));
        assert_eq!(target_display(1920, 1080, 1.5), (1280, 720));
        assert_eq!(even_dim(1), 2);
        for (w, h) in [(3, 5), (1000, 1001), (2559, 1441), (7680, 4320)] {
            let (ew, eh) = target_display(w, h, 1.5);
            assert!(ew % 2 == 0 && eh % 2 == 0, "应偶对齐 got {ew}x{eh}");
        }
    }

    #[test]
    fn letterbox_fits_and_preserves_aspect() {
        let v = letterbox(Vec2::new(1920.0, 1080.0), 16.0 / 9.0);
        assert!((v.x / v.y - 16.0 / 9.0).abs() < 1e-3);
        assert!(v.x <= 1920.0 && v.y <= 1080.0);
    }
}
