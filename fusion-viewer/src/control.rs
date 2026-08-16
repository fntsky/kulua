//! kulua-server 控制协议消息构建（client → server）。
//!
//! 字节布局对照 kulua-server ControlChannel.java 核实（scrcpy v4.0 布局 +
//! 自研多显示器扩展：touch/scroll/start_app/resize 均带 displayId u32be 前缀）：
//! - `kulua-server/src/.../ControlChannel.java`（服务端解析）
//! 所有多字节字段均为大端序。

/// 控制消息类型（client → server）。
pub mod msg_type {
    pub const INJECT_KEYCODE: u8 = 0;
    pub const INJECT_TEXT: u8 = 1;
    pub const INJECT_TOUCH_EVENT: u8 = 2;
    pub const INJECT_SCROLL_EVENT: u8 = 3;
    pub const BACK_OR_SCREEN_ON: u8 = 4;
    pub const START_APP: u8 = 16;
    pub const RESIZE_DISPLAY: u8 = 21;
}

/// 按键事件动作（android.view.KeyEvent）。
pub mod key_action {
    pub const DOWN: u8 = 0;
    pub const UP: u8 = 1;
}

/// 触摸事件动作（android.view.MotionEvent，低 8 位为主动作）。
pub mod touch_action {
    pub const DOWN: u8 = 0;
    pub const UP: u8 = 1;
    pub const MOVE: u8 = 2;
}

/// 鼠标指针 id（scrcpy 约定：-1 = 鼠标，-2 = 通用手指，-3 = 虚拟手指）。
pub const POINTER_ID_MOUSE: u64 = u64::MAX;

/// 常见 Android keycode（android.view.KeyEvent.KEYCODE_*）。
pub mod keycode {
    pub const BACK: u32 = 4;
    pub const HOME: u32 = 3;
    pub const VOLUME_UP: u32 = 24;
    pub const VOLUME_DOWN: u32 = 25;
    pub const ENTER: u32 = 66;
    pub const DEL: u32 = 67;
    pub const TAB: u32 = 61;
    pub const SPACE: u32 = 62;
    pub const DPAD_UP: u32 = 19;
    pub const DPAD_DOWN: u32 = 20;
    pub const DPAD_LEFT: u32 = 21;
    pub const DPAD_RIGHT: u32 = 22;
    pub const KEY_SEMICOLON: u32 = 74;
    pub const KEY_APOSTROPHE: u32 = 75;
    pub const KEY_COMMA: u32 = 55;
    pub const KEY_PERIOD: u32 = 56;
    pub const KEY_SLASH: u32 = 76;
    pub const KEY_LEFT_BRACKET: u32 = 71;
    pub const KEY_RIGHT_BRACKET: u32 = 72;
    pub const KEY_MINUS: u32 = 69;
    pub const KEY_EQUALS: u32 = 70;
    pub const KEY_GRAVE: u32 = 68;
    pub const KEY_BACKSLASH: u32 = 73;
}

/// 构建 INJECT_KEYCODE 消息（18 字节，含 displayId u32be 前缀）。
pub fn inject_keycode(display_id: u32, action: u8, keycode: u32, repeat: u32, metastate: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 18];
    buf[0] = msg_type::INJECT_KEYCODE;
    buf[1..5].copy_from_slice(&display_id.to_be_bytes());
    buf[5] = action;
    buf[6..10].copy_from_slice(&keycode.to_be_bytes());
    buf[10..14].copy_from_slice(&repeat.to_be_bytes());
    buf[14..18].copy_from_slice(&metastate.to_be_bytes());
    buf
}

/// 构建 INJECT_TEXT 消息（5 + 4 + len，UTF-8 文本，含 displayId u32be 前缀）。
pub fn inject_text(display_id: u32, text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut buf = Vec::with_capacity(9 + bytes.len());
    buf.push(msg_type::INJECT_TEXT);
    buf.extend_from_slice(&display_id.to_be_bytes());
    buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(bytes);
    buf
}

/// 构建 INJECT_TOUCH_EVENT 消息（36 字节，含 displayId u32be 前缀）。
///
/// `display_id` 为 server 分配的虚拟显示器 id（video 握手时返回），
/// `x`/`y` 为设备物理像素坐标（int32），`screen_w`/`screen_h` 为当前视频分辨率，
/// `pressure` 0.0~1.0 转为 16 位定点。
pub fn inject_touch(
    display_id: u32,
    action: u8,
    pointer_id: u64,
    x: i32,
    y: i32,
    screen_w: u16,
    screen_h: u16,
    pressure: f32,
    action_button: u32,
    buttons: u32,
) -> Vec<u8> {
    let mut buf = vec![0u8; 36];
    buf[0] = msg_type::INJECT_TOUCH_EVENT;
    buf[1..5].copy_from_slice(&display_id.to_be_bytes());
    buf[5] = action;
    buf[6..14].copy_from_slice(&pointer_id.to_be_bytes());
    buf[14..18].copy_from_slice(&x.to_be_bytes());
    buf[18..22].copy_from_slice(&y.to_be_bytes());
    buf[22..24].copy_from_slice(&screen_w.to_be_bytes());
    buf[24..26].copy_from_slice(&screen_h.to_be_bytes());
    buf[26..28].copy_from_slice(&f32_to_u16fp(pressure).to_be_bytes());
    buf[28..32].copy_from_slice(&action_button.to_be_bytes());
    buf[32..36].copy_from_slice(&buttons.to_be_bytes());
    buf
}

/// 构建 INJECT_SCROLL_EVENT 消息（25 字节，含 displayId u32be 前缀）。
///
/// `hscroll`/`vscroll` 接受 [-16, 16] 范围（像素滚动量），归一化后转 16 位定点。
pub fn inject_scroll(
    display_id: u32,
    x: i32,
    y: i32,
    screen_w: u16,
    screen_h: u16,
    hscroll: f32,
    vscroll: f32,
    buttons: u32,
) -> Vec<u8> {
    let mut buf = vec![0u8; 25];
    buf[0] = msg_type::INJECT_SCROLL_EVENT;
    buf[1..5].copy_from_slice(&display_id.to_be_bytes());
    buf[5..9].copy_from_slice(&x.to_be_bytes());
    buf[9..13].copy_from_slice(&y.to_be_bytes());
    buf[13..15].copy_from_slice(&screen_w.to_be_bytes());
    buf[15..17].copy_from_slice(&screen_h.to_be_bytes());
    buf[17..19].copy_from_slice(&f32_to_i16fp(hscroll / 16.0).to_be_bytes());
    buf[19..21].copy_from_slice(&f32_to_i16fp(vscroll / 16.0).to_be_bytes());
    buf[21..25].copy_from_slice(&buttons.to_be_bytes());
    buf
}

/// 构建 BACK_OR_SCREEN_ON 消息（6 字节，含 displayId u32be 前缀）。
pub fn back_or_screen_on(display_id: u32, action: u8) -> Vec<u8> {
    let mut buf = vec![0u8; 6];
    buf[0] = msg_type::BACK_OR_SCREEN_ON;
    buf[1..5].copy_from_slice(&display_id.to_be_bytes());
    buf[5] = action;
    buf
}

/// 构建 START_APP 消息（5 + len，包名最长 255 字节，含 displayId u32be 前缀）。
pub fn start_app(display_id: u32, package: &str) -> Result<Vec<u8>, String> {
    let bytes = package.as_bytes();
    if bytes.len() > 255 {
        return Err(format!("包名过长: {} 字节", bytes.len()));
    }
    let mut buf = Vec::with_capacity(6 + bytes.len());
    buf.push(msg_type::START_APP);
    buf.extend_from_slice(&display_id.to_be_bytes());
    buf.push(bytes.len() as u8);
    buf.extend_from_slice(bytes);
    Ok(buf)
}

/// 构建 RESIZE_DISPLAY 消息（9 字节，含 displayId u32be 前缀）。
pub fn resize_display(display_id: u32, width: u16, height: u16) -> Vec<u8> {
    let mut buf = vec![0u8; 9];
    buf[0] = msg_type::RESIZE_DISPLAY;
    buf[1..5].copy_from_slice(&display_id.to_be_bytes());
    buf[5..7].copy_from_slice(&width.to_be_bytes());
    buf[7..9].copy_from_slice(&height.to_be_bytes());
    buf
}

/// float [0,1] → u16 定点（0..0xFFFF）。
fn f32_to_u16fp(value: f32) -> u16 {
    let clamped = value.clamp(0.0, 1.0);
    (clamped * 65536.0) as u16
}

/// float [-1,1] → i16 定点（-0x8000..0x7FFF，对应 sc_float_to_i16fp）。
fn f32_to_i16fp(value: f32) -> i16 {
    let clamped = value.clamp(-1.0, 1.0);
    (clamped * 32768.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycode_message_layout() {
        let msg = inject_keycode(7, key_action::DOWN, keycode::HOME, 0, 0);
        assert_eq!(msg.len(), 18);
        assert_eq!(msg[0], msg_type::INJECT_KEYCODE);
        assert_eq!(&msg[1..5], &7u32.to_be_bytes(), "displayId 前缀");
        assert_eq!(msg[5], 0); // down
        assert_eq!(&msg[6..10], &3u32.to_be_bytes()); // HOME
        assert_eq!(&msg[10..14], &0u32.to_be_bytes());
        assert_eq!(&msg[14..18], &0u32.to_be_bytes());
    }

    #[test]
    fn text_message_layout() {
        let msg = inject_text(2, "hello");
        assert_eq!(msg[0], msg_type::INJECT_TEXT);
        assert_eq!(&msg[1..5], &2u32.to_be_bytes(), "displayId 前缀");
        assert_eq!(&msg[5..9], &5u32.to_be_bytes());
        assert_eq!(&msg[9..], b"hello");
    }

    #[test]
    fn touch_message_layout() {
        // 对照 kulua-server ControlChannel.java：type, displayId(u32be), action,
        // pointerId(u64be), x/y(i32be), w/h(u16be), pressure(u16fp), actionButton, buttons
        let msg = inject_touch(
            7,
            touch_action::DOWN,
            POINTER_ID_MOUSE,
            100,
            200,
            1280,
            960,
            1.0,
            0,
            0,
        );
        assert_eq!(msg.len(), 36);
        assert_eq!(msg[0], msg_type::INJECT_TOUCH_EVENT);
        assert_eq!(&msg[1..5], &7u32.to_be_bytes(), "displayId 前缀");
        assert_eq!(msg[5], 0);
        assert_eq!(&msg[6..14], &POINTER_ID_MOUSE.to_be_bytes());
        assert_eq!(&msg[14..18], &100i32.to_be_bytes());
        assert_eq!(&msg[18..22], &200i32.to_be_bytes());
        assert_eq!(&msg[22..24], &1280u16.to_be_bytes());
        assert_eq!(&msg[24..26], &960u16.to_be_bytes());
        assert_eq!(
            &msg[26..28],
            &0xFFFFu16.to_be_bytes(),
            "pressure=1.0 → 0xFFFF"
        );
        assert_eq!(&msg[28..32], &0u32.to_be_bytes());
        assert_eq!(&msg[32..36], &0u32.to_be_bytes());
    }

    #[test]
    fn touch_pressure_zero() {
        let msg = inject_touch(
            0,
            touch_action::MOVE,
            POINTER_ID_MOUSE,
            0,
            0,
            100,
            100,
            0.0,
            0,
            0,
        );
        assert_eq!(&msg[26..28], &0u16.to_be_bytes());
    }

    #[test]
    fn scroll_message_layout() {
        // 对照 kulua-server：type, displayId(u32be), position(12B), hscroll/vscroll i16fp, buttons
        let msg = inject_scroll(3, 50, 60, 1280, 960, 0.0, -16.0, 0);
        assert_eq!(msg.len(), 25);
        assert_eq!(msg[0], msg_type::INJECT_SCROLL_EVENT);
        assert_eq!(&msg[1..5], &3u32.to_be_bytes(), "displayId 前缀");
        assert_eq!(&msg[5..9], &50i32.to_be_bytes());
        assert_eq!(&msg[9..13], &60i32.to_be_bytes());
        assert_eq!(&msg[13..15], &1280u16.to_be_bytes());
        assert_eq!(&msg[15..17], &960u16.to_be_bytes());
        // vscroll=-16 → 归一化 -1.0 → i16fp -0x8000
        assert_eq!(&msg[19..21], &(-0x8000i16).to_be_bytes());
        assert_eq!(&msg[17..19], &0i16.to_be_bytes(), "hscroll=0");
        assert_eq!(&msg[21..25], &0u32.to_be_bytes());
    }

    #[test]
    fn scroll_positive_half() {
        // vscroll=8 → 归一化 0.5 → 0x4000
        let msg = inject_scroll(0, 0, 0, 100, 100, 0.0, 8.0, 0);
        assert_eq!(&msg[19..21], &0x4000i16.to_be_bytes());
    }

    #[test]
    fn back_and_start_app_layout() {
        let back = back_or_screen_on(3, key_action::DOWN);
        assert_eq!(back.len(), 6);
        assert_eq!(back[0], msg_type::BACK_OR_SCREEN_ON);
        assert_eq!(&back[1..5], &3u32.to_be_bytes(), "displayId 前缀");
        assert_eq!(back[5], 0);

        let start = start_app(5, "com.android.settings").unwrap();
        assert_eq!(start[0], msg_type::START_APP);
        assert_eq!(&start[1..5], &5u32.to_be_bytes(), "displayId 前缀");
        assert_eq!(start[5], 20);
        assert_eq!(&start[6..], b"com.android.settings");

        assert!(start_app(0, &"a".repeat(300)).is_err());
    }

    #[test]
    fn resize_display_layout() {
        let msg = resize_display(9, 1920, 1080);
        assert_eq!(msg.len(), 9);
        assert_eq!(msg[0], msg_type::RESIZE_DISPLAY);
        assert_eq!(&msg[1..5], &9u32.to_be_bytes(), "displayId 前缀");
        assert_eq!(&msg[5..7], &1920u16.to_be_bytes());
        assert_eq!(&msg[7..9], &1080u16.to_be_bytes());
    }
}
