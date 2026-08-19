//! egui 键 → Android keycode 映射（导航/编辑等特殊键）。
//!
//! 可打印字符（字母/数字/标点/IME）由 egui 的 `Event::Text` 走文本注入
//! （`inject_text`），不需要也不应映射成物理键。

use egui::Key;

/// 参与 keycode 注入的 egui 按键集合（可打印字符走文本注入，不在此列）。
pub const MAPPED_KEYS: &[Key] = &[
    Key::Escape,
    Key::Home,
    Key::Backspace,
    Key::Enter,
    Key::Tab,
    Key::Space,
    Key::Delete,
    Key::PageUp,
    Key::PageDown,
    Key::ArrowUp,
    Key::ArrowDown,
    Key::ArrowLeft,
    Key::ArrowRight,
    Key::F1,
    Key::F2,
    Key::F3,
    Key::F4,
    Key::F5,
    Key::F6,
    Key::F7,
    Key::F8,
    Key::F9,
    Key::F10,
    Key::F11,
    Key::F12,
];

/// 将 egui 特殊键映射为 Android keycode；不支持返回 None。
pub fn to_android_keycode(key: Key) -> Option<u32> {
    Some(match key {
        // 导航/系统键
        Key::Escape => crate::control::keycode::BACK, // scrcpy 默认：ESC = 返回
        Key::Home => crate::control::keycode::HOME,
        Key::Backspace => crate::control::keycode::DEL,
        Key::Enter => crate::control::keycode::ENTER,
        Key::Tab => crate::control::keycode::TAB,
        Key::Space => crate::control::keycode::SPACE,
        Key::Delete => 112,  // KEYCODE_FORWARD_DEL
        Key::PageUp => 92,   // KEYCODE_PAGE_UP
        Key::PageDown => 93, // KEYCODE_PAGE_DOWN
        Key::ArrowUp => crate::control::keycode::DPAD_UP,
        Key::ArrowDown => crate::control::keycode::DPAD_DOWN,
        Key::ArrowLeft => crate::control::keycode::DPAD_LEFT,
        Key::ArrowRight => crate::control::keycode::DPAD_RIGHT,
        // 功能键
        Key::F1 => 131,
        Key::F2 => 132,
        Key::F3 => 133,
        Key::F4 => 134,
        Key::F5 => 135,
        Key::F6 => 136,
        Key::F7 => 137,
        Key::F8 => 138,
        Key::F9 => 139,
        Key::F10 => 140,
        Key::F11 => 141,
        Key::F12 => 142,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_keys() {
        assert_eq!(to_android_keycode(Key::Escape), Some(4)); // BACK
        assert_eq!(to_android_keycode(Key::Backspace), Some(67));
        assert_eq!(to_android_keycode(Key::Enter), Some(66));
        assert_eq!(to_android_keycode(Key::Tab), Some(61));
        assert_eq!(to_android_keycode(Key::Space), Some(62));
        assert_eq!(to_android_keycode(Key::ArrowUp), Some(19));
        assert_eq!(to_android_keycode(Key::ArrowDown), Some(20));
        assert_eq!(to_android_keycode(Key::ArrowLeft), Some(21));
        assert_eq!(to_android_keycode(Key::ArrowRight), Some(22));
        assert_eq!(to_android_keycode(Key::Home), Some(3));
        assert_eq!(to_android_keycode(Key::F1), Some(131));
        assert_eq!(to_android_keycode(Key::F12), Some(142));
    }

    #[test]
    fn unsupported_keys_are_none() {
        assert_eq!(to_android_keycode(Key::A), None);
        assert_eq!(to_android_keycode(Key::Plus), None);
    }
}
