use serde::{Deserialize, Serialize};

use uuid::Uuid;

/// 来自 `adb get-serialno` 的规范设备 ID（主键）
pub type DeviceId = String;

/// 设备地址形式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAddrKind {
    Usb,
    Mdns,
    Ip,
}

impl DeviceAddrKind {
    /// 根据地址字符串猜测其形式
    pub fn classify(addr: &str) -> Self {
        // USB serial: 纯字母数字，无冒号（如 "10AE6X05XP001TD"）
        // IP:port: 包含冒号和点（如 "192.168.1.100:5555"）
        // mDNS:port: 包含 ".local:" 或 "._tcp"
        if addr.contains(".local:") || addr.contains("._tcp") {
            DeviceAddrKind::Mdns
        } else if addr.contains(':') {
            DeviceAddrKind::Ip
        } else {
            DeviceAddrKind::Usb
        }
    }

    /// 优先级权重，越大越优先
    pub fn priority(self) -> u8 {
        match self {
            DeviceAddrKind::Usb => 3,
            DeviceAddrKind::Mdns => 2,
            DeviceAddrKind::Ip => 1,
        }
    }
}

/// 设备三种身份形式，用于去重和选择最佳连接地址
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub usb: Option<String>,
    pub mdns: Option<String>,
    pub ip: Option<String>,
}

impl DeviceIdentity {
    /// 获取优先级最高的已有地址
    pub fn best_serial(&self) -> Option<&str> {
        self.usb
            .as_deref()
            .or_else(|| self.mdns.as_deref())
            .or_else(|| self.ip.as_deref())
    }

    /// 合并另一个 identity（较高优先级的地址覆盖较低优先级的）
    pub fn merge(&mut self, other: &DeviceIdentity) {
        if other.usb.is_some() {
            self.usb = other.usb.clone();
        }
        if other.mdns.is_some() {
            self.mdns = other.mdns.clone();
        }
        if other.ip.is_some() {
            self.ip = other.ip.clone();
        }
    }

    /// 根据 address 和已知的 form 填入对应字段
    pub fn set_by_kind(&mut self, addr: String, kind: DeviceAddrKind) {
        match kind {
            DeviceAddrKind::Usb => self.usb = Some(addr),
            DeviceAddrKind::Mdns => self.mdns = Some(addr),
            DeviceAddrKind::Ip => self.ip = Some(addr),
        }
    }

    /// 检查是否包含某地址（任意形式）
    pub fn contains_addr(&self, addr: &str) -> bool {
        self.usb.as_deref() == Some(addr)
            || self.mdns.as_deref() == Some(addr)
            || self.ip.as_deref() == Some(addr)
    }
}

/// 设备信息
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// 全局唯一标识（生成时分配，不可变）
    pub uuid: Uuid,
    /// 规范设备 ID（来自 `adb get-serialno`，主键）
    pub id: DeviceId,
    /// 当前用于 adb 命令的最佳地址（最优先的已确认形式）
    pub serial: String,
    /// 连接状态
    pub state: DeviceState,
    /// 设备名称（从 scrcpy 协议获取，空表示未知）
    #[serde(default)]
    pub name: String,
    /// 所有已知身份的地址形式
    #[serde(default)]
    pub identity: DeviceIdentity,
}

/// 设备状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        let err = AdbError::AdbNotFound {
            tried: "/usr/bin/adb".into(),
        };
        let output = err.to_string();
        assert!(
            output.contains("adb not found"),
            "expected 'adb not found' in display, got: {}",
            output
        );
    }

    #[test]
    fn test_display_command_failed() {
        let err = AdbError::CommandFailed("something went wrong".into());
        let output = err.to_string();
        assert!(
            output.contains("adb command failed"),
            "expected 'adb command failed' in display, got: {}",
            output
        );
        assert!(
            output.contains("something went wrong"),
            "expected message in display, got: {}",
            output
        );
    }

    #[test]
    fn test_display_io() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let err = AdbError::Io(io_err);
        let output = err.to_string();
        assert!(
            output.contains("I/O error"),
            "expected 'I/O error' in display, got: {}",
            output
        );
    }

    #[test]
    fn test_partial_eq_adb_not_found() {
        let a = AdbError::AdbNotFound {
            tried: "/path/a".into(),
        };
        let b = AdbError::AdbNotFound {
            tried: "/path/a".into(),
        };
        assert_eq!(a, b, "same tried path should be equal");

        let c = AdbError::AdbNotFound {
            tried: "/path/b".into(),
        };
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
        let not_found = AdbError::AdbNotFound {
            tried: "adb".into(),
        };
        let cmd_failed = AdbError::CommandFailed("adb".into());
        assert_ne!(
            not_found, cmd_failed,
            "different variants should not be equal"
        );
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
