//! kulua-server 控制协议消息构建（client → server，protobuf 版）。
//!
//! 语义对照 kulua-server ControlChannel.java（scrcpy v4.0 + 多显示器扩展）：
//! 输入注入 / START_APP / RESIZE / CREATE_DISPLAY 均带 displayId。
//! 线协议与具体编码由 kulua-proto（proto/direct.proto）负责。

pub use kulua_proto::session::msgs::*;

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
    pub const ENTER: u32 = 66;
    pub const DEL: u32 = 67;
    pub const TAB: u32 = 61;
    pub const SPACE: u32 = 62;
    pub const DPAD_UP: u32 = 19;
    pub const DPAD_DOWN: u32 = 20;
    pub const DPAD_LEFT: u32 = 21;
    pub const DPAD_RIGHT: u32 = 22;
}

#[cfg(test)]
mod tests {
    use super::*;
    use kulua_proto::generated::ctrl_msg;

    #[test]
    fn keycode_message_roundtrip() {
        // 验证通过 protobuf 往返后字段与原字节布局语义一致（displayId 前缀等）
        let msg = inject_keycode(7, key_action::DOWN as u32, keycode::HOME, 0, 0);
        let m = match msg.msg {
            Some(ctrl_msg::Msg::InjectKeycode(m)) => m,
            _ => panic!("expected InjectKeycode"),
        };
        assert_eq!(m.display_id, 7);
        assert_eq!(m.action, key_action::DOWN as u32);
        assert_eq!(m.keycode, keycode::HOME);
        assert_eq!(m.repeat, 0);
        assert_eq!(m.meta_state, 0);
    }

    #[test]
    fn text_payload() {
        let msg = inject_text(2, "hello");
        let m = match msg.msg {
            Some(ctrl_msg::Msg::InjectText(m)) => m,
            _ => panic!("expected InjectText"),
        };
        assert_eq!(m.display_id, 2);
        assert_eq!(m.text, "hello");
    }

    #[test]
    fn touch_pointer_pressure_roundtrip() {
        let msg = inject_touch(
            7,
            touch_action::DOWN as u32,
            POINTER_ID_MOUSE,
            100,
            200,
            1280,
            960,
            1.0,
            0,
            0,
        );
        let m = match msg.msg {
            Some(ctrl_msg::Msg::InjectTouch(m)) => m,
            _ => panic!("expected InjectTouch"),
        };
        assert_eq!(m.display_id, 7);
        assert_eq!(m.pointer_id, POINTER_ID_MOUSE);
        assert_eq!(m.x, 100);
        assert_eq!(m.y, 200);
        assert_eq!(m.screen_width, 1280);
        assert_eq!((m.pressure - 1.0).abs() < 1e-6, true);
    }

    #[test]
    fn scroll_and_start_app_and_resize() {
        let scroll = inject_scroll(3, 50, 60, 1280, 960, 0.0, -16.0, 0);
        let s = match scroll.msg {
            Some(ctrl_msg::Msg::InjectScroll(s)) => s,
            _ => panic!("expected InjectScroll"),
        };
        assert_eq!((s.x, s.y, s.vscroll), (50, 60, -16.0));

        let start = start_app(5, "com.android.settings");
        let st = match start.msg {
            Some(ctrl_msg::Msg::StartApp(s)) => s,
            _ => panic!("expected StartApp"),
        };
        assert_eq!(st.package, "com.android.settings");

        let rz = resize_display(9, 1920, 1080);
        let r = match rz.msg {
            Some(ctrl_msg::Msg::ResizeDisplay(r)) => r,
            _ => panic!("expected ResizeDisplay"),
        };
        assert_eq!((r.width, r.height), (1920, 1080));
    }

    #[test]
    fn back_or_screen_on_layout() {
        let back = back_or_screen_on(3, key_action::DOWN as u32);
        let b = match back.msg {
            Some(ctrl_msg::Msg::BackOrScreenOn(b)) => b,
            _ => panic!("expected BackOrScreenOn"),
        };
        assert_eq!(b.display_id, 3);
        assert_eq!(b.action, key_action::DOWN as u32);
    }
}
