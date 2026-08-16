//! winit 键码 → Android keycode 映射（常用键子集，对照 scrcpy 的 SDL 映射表）。

use winit::keyboard::KeyCode;

/// 将 winit 键码映射为 Android keycode；不支持的键返回 None（忽略）。
pub fn to_android_keycode(code: KeyCode) -> Option<u32> {
    use winit::keyboard::KeyCode::*;
    Some(match code {
        // 导航/系统键
        Escape => crate::control::keycode::BACK, // scrcpy 默认：ESC = 返回
        Home => crate::control::keycode::HOME,
        Backspace => crate::control::keycode::DEL,
        Enter | NumpadEnter => crate::control::keycode::ENTER,
        Tab => crate::control::keycode::TAB,
        Space => crate::control::keycode::SPACE,
        Delete => 112,  // KEYCODE_FORWARD_DEL
        PageUp => 92,   // KEYCODE_PAGE_UP
        PageDown => 93, // KEYCODE_PAGE_DOWN
        ArrowUp => crate::control::keycode::DPAD_UP,
        ArrowDown => crate::control::keycode::DPAD_DOWN,
        ArrowLeft => crate::control::keycode::DPAD_LEFT,
        ArrowRight => crate::control::keycode::DPAD_RIGHT,
        // 标点（对照 android KeyEvent）
        Semicolon => crate::control::keycode::KEY_SEMICOLON,
        Quote => crate::control::keycode::KEY_APOSTROPHE,
        Comma => crate::control::keycode::KEY_COMMA,
        Period => crate::control::keycode::KEY_PERIOD,
        Slash => crate::control::keycode::KEY_SLASH,
        Backslash => crate::control::keycode::KEY_BACKSLASH,
        BracketLeft => crate::control::keycode::KEY_LEFT_BRACKET,
        BracketRight => crate::control::keycode::KEY_RIGHT_BRACKET,
        Minus => crate::control::keycode::KEY_MINUS,
        Equal => crate::control::keycode::KEY_EQUALS,
        Backquote => crate::control::keycode::KEY_GRAVE,
        // 音量
        AudioVolumeUp => crate::control::keycode::VOLUME_UP,
        AudioVolumeDown => crate::control::keycode::VOLUME_DOWN,
        // 字母/数字/功能键：显式映射（winit KeyCode 判别值按字母序，不能依赖数值）
        other => return letter_digit_or_f_key(other),
    })
}

/// 字母（KEYCODE_A=29 起）、数字（KEYCODE_0=7 起）、功能键（KEYCODE_F1=131 起）映射。
fn letter_digit_or_f_key(code: KeyCode) -> Option<u32> {
    use winit::keyboard::KeyCode::*;
    Some(match code {
        KeyA => 29,
        KeyB => 30,
        KeyC => 31,
        KeyD => 32,
        KeyE => 33,
        KeyF => 34,
        KeyG => 35,
        KeyH => 36,
        KeyI => 37,
        KeyJ => 38,
        KeyK => 39,
        KeyL => 40,
        KeyM => 41,
        KeyN => 42,
        KeyO => 43,
        KeyP => 44,
        KeyQ => 45,
        KeyR => 46,
        KeyS => 47,
        KeyT => 48,
        KeyU => 49,
        KeyV => 50,
        KeyW => 51,
        KeyX => 52,
        KeyY => 53,
        KeyZ => 54,
        Digit0 => 7,
        Digit1 => 8,
        Digit2 => 9,
        Digit3 => 10,
        Digit4 => 11,
        Digit5 => 12,
        Digit6 => 13,
        Digit7 => 14,
        Digit8 => 15,
        Digit9 => 16,
        F1 => 131,
        F2 => 132,
        F3 => 133,
        F4 => 134,
        F5 => 135,
        F6 => 136,
        F7 => 137,
        F8 => 138,
        F9 => 139,
        F10 => 140,
        F11 => 141,
        F12 => 142,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_are_sequential() {
        // KEYCODE_A=29, KEYCODE_Z=54
        assert_eq!(to_android_keycode(KeyCode::KeyA), Some(29));
        assert_eq!(to_android_keycode(KeyCode::KeyZ), Some(54));
        assert_eq!(to_android_keycode(KeyCode::KeyM), Some(41));
    }

    #[test]
    fn digits_are_sequential() {
        // KEYCODE_0=7, KEYCODE_9=16
        assert_eq!(to_android_keycode(KeyCode::Digit0), Some(7));
        assert_eq!(to_android_keycode(KeyCode::Digit9), Some(16));
    }

    #[test]
    fn special_keys() {
        assert_eq!(to_android_keycode(KeyCode::Escape), Some(4)); // BACK
        assert_eq!(to_android_keycode(KeyCode::Backspace), Some(67));
        assert_eq!(to_android_keycode(KeyCode::Enter), Some(66));
        assert_eq!(to_android_keycode(KeyCode::Tab), Some(61));
        assert_eq!(to_android_keycode(KeyCode::Space), Some(62));
        assert_eq!(to_android_keycode(KeyCode::ArrowUp), Some(19));
        assert_eq!(to_android_keycode(KeyCode::ArrowDown), Some(20));
        assert_eq!(to_android_keycode(KeyCode::ArrowLeft), Some(21));
        assert_eq!(to_android_keycode(KeyCode::ArrowRight), Some(22));
        assert_eq!(to_android_keycode(KeyCode::Home), Some(3));
        assert_eq!(to_android_keycode(KeyCode::AudioVolumeUp), Some(24));
        assert_eq!(to_android_keycode(KeyCode::AudioVolumeDown), Some(25));
        assert_eq!(to_android_keycode(KeyCode::F1), Some(131));
        assert_eq!(to_android_keycode(KeyCode::F12), Some(142));
    }

    #[test]
    fn unsupported_keys_are_none() {
        assert_eq!(to_android_keycode(KeyCode::CapsLock), None);
        assert_eq!(to_android_keycode(KeyCode::ShiftLeft), None);
        assert_eq!(to_android_keycode(KeyCode::AltLeft), None);
        assert_eq!(to_android_keycode(KeyCode::SuperLeft), None);
    }
}
