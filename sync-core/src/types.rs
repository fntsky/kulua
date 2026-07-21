/// 设备信息
#[derive(Debug, Clone)]
pub struct Device {
    /// 设备序列号 / IP 地址
    pub serial: String,
    /// 连接状态
    pub state: DeviceState,
}

/// 设备状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceState {
    /// 正常连接
    Device,
    /// 离线
    Offline,
    /// 未授权（需要确认调试弹窗）
    Unauthorized,
    /// 其他未知状态
    Unknown(String),
}

/// ADB 错误类型
#[derive(Debug)]
pub enum AdbError {
    /// 找不到 adb 可执行文件
    AdbNotFound { tried: String },
    /// 命令执行失败
    CommandFailed(String),
    /// IO 错误
    Io(std::io::Error),
    /// UTF-8 解析错误
    Utf8(std::str::Utf8Error),
    /// 其他错误（保留供未来使用）
    #[allow(dead_code)]
    Other(String),
}

impl std::fmt::Display for AdbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdbError::AdbNotFound { tried } => {
                write!(f, "adb not found (tried: {})", tried)
            }
            AdbError::CommandFailed(msg) => write!(f, "adb command failed: {}", msg),
            AdbError::Io(e) => write!(f, "I/O error: {}", e),
            AdbError::Utf8(e) => write!(f, "UTF-8 error: {}", e),
            AdbError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for AdbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AdbError::Io(e) => Some(e),
            AdbError::Utf8(e) => Some(e),
            _ => None,
        }
    }
}

impl PartialEq for AdbError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AdbError::AdbNotFound { tried: a }, AdbError::AdbNotFound { tried: b }) => a == b,
            (AdbError::CommandFailed(a), AdbError::CommandFailed(b)) => a == b,
            (AdbError::Io(_), AdbError::Io(_)) => {
                // std::io::Error 没有 PartialEq，当变体匹配就算相等
                true
            }
            (AdbError::Utf8(_), AdbError::Utf8(_)) => true,
            (AdbError::Other(a), AdbError::Other(b)) => a == b,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AdbError;
    use std::io;

    #[test]
    fn test_display_adb_not_found() {
        let err = AdbError::AdbNotFound { tried: "/usr/bin/adb".into() };
        let output = err.to_string();
        assert!(output.contains("adb not found"), "expected 'adb not found' in display, got: {}", output);
    }

    #[test]
    fn test_display_command_failed() {
        let err = AdbError::CommandFailed("something went wrong".into());
        let output = err.to_string();
        assert!(output.contains("adb command failed"), "expected 'adb command failed' in display, got: {}", output);
        assert!(output.contains("something went wrong"), "expected message in display, got: {}", output);
    }

    #[test]
    fn test_display_io() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let err = AdbError::Io(io_err);
        let output = err.to_string();
        assert!(output.contains("I/O error"), "expected 'I/O error' in display, got: {}", output);
    }

    #[test]
    fn test_partial_eq_adb_not_found() {
        let a = AdbError::AdbNotFound { tried: "/path/a".into() };
        let b = AdbError::AdbNotFound { tried: "/path/a".into() };
        assert_eq!(a, b, "same tried path should be equal");

        let c = AdbError::AdbNotFound { tried: "/path/b".into() };
        assert_ne!(a, c, "different tried paths should not be equal");
    }

    #[test]
    fn test_partial_eq_command_failed() {
        let a = AdbError::CommandFailed("msg".into());
        let b = AdbError::CommandFailed("msg".into());
        assert_eq!(a, b, "same message should be equal");

        let c = AdbError::CommandFailed("other".into());
        assert_ne!(a, c, "different messages should not be equal");
    }

    #[test]
    fn test_partial_eq_different_variants() {
        let not_found = AdbError::AdbNotFound { tried: "adb".into() };
        let cmd_failed = AdbError::CommandFailed("adb".into());
        assert_ne!(not_found, cmd_failed, "different variants should not be equal");
    }

    #[test]
    fn test_error_source_io() {
        let inner = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");
        let err = AdbError::Io(inner);
        let source = std::error::Error::source(&err);
        assert!(source.is_some(), "Io variant should have a source");
        assert!(source.unwrap().to_string().contains("permission denied"));
    }

    #[test]
    fn test_error_source_utf8() {
        let invalid = &[0xFF, 0xFE][..];
        let utf8_err = std::str::from_utf8(invalid).unwrap_err();
        let err = AdbError::Utf8(utf8_err);
        let source = std::error::Error::source(&err);
        assert!(source.is_some(), "Utf8 variant should have a source");
    }

    #[test]
    fn test_error_source_other() {
        let err = AdbError::Other("generic".into());
        let source = std::error::Error::source(&err);
        assert!(source.is_none(), "Other variant should not have a source");
    }
}
