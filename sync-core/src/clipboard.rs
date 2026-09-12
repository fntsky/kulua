//! 系统剪贴板读写（跨平台薄封装）。
//!
//! - Windows：`clipboard-win` 直连 Win32 剪贴板 API（保持 Windows 原有行为）；
//! - 其余平台（Linux/macOS）：`arboard` — 纯 Rust 实现，Linux 走 Wayland/X11
//!   协议，无需安装 wl-copy/xclip 等外部工具。
//!
//! 所有失败统一转换为 [`crate::types::AdbError`]，调用方按需忽略或降级，
//! 本模块不 panic。

use crate::types::AdbError;

/// 读取系统剪贴板文本。
///
/// 剪贴板为空返回 `Ok("")`；在无显示服务器（SSH / 后台轮询）或剪贴板被其他
/// 程序占用时可能返回 `Err`，调用方应降级处理而不是中断流程。
///
/// WHY 返回 `Result` 而不是直接给空串：轮询剪贴板用于「PC→手机」同步，
/// 区分「剪贴板为空」与「读取失败」才能避免误清空手机端内容。
pub fn get_text() -> Result<String, AdbError> {
    #[cfg(windows)]
    {
        clipboard_win::get_clipboard_string().map_err(|e| AdbError::Other(e.to_string()))
    }
    #[cfg(not(windows))]
    {
        let mut cb = arboard::Clipboard::new().map_err(|e| AdbError::Other(e.to_string()))?;
        cb.get_text().map_err(|e| AdbError::Other(e.to_string()))
    }
}

/// 写入系统剪贴板文本（覆盖当前内容）。
pub fn set_text(text: &str) -> Result<(), AdbError> {
    #[cfg(windows)]
    {
        clipboard_win::set_clipboard_string(text).map_err(|e| AdbError::Other(e.to_string()))
    }
    #[cfg(not(windows))]
    {
        let mut cb = arboard::Clipboard::new().map_err(|e| AdbError::Other(e.to_string()))?;
        cb.set_text(text.to_owned())
            .map_err(|e| AdbError::Other(e.to_string()))
    }
}
