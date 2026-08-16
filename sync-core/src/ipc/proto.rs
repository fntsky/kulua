//! IPC Protobuf 消息定义。
//!
//! 帧格式仍为 `[length:u32 LE][type:u8][payload]`，Request / Response / Event
//! 的 payload 以及各方法的参数、结果、事件数据都已使用 Protobuf 强类型编码。

use prost::Message;

use crate::ipc::types::SessionSummary as CoreSessionSummary;
use crate::types::{Device as CoreDevice, DeviceIdentity as CoreDeviceIdentity};

// ── 外层信封 ──

/// GUI → daemon 的请求。
///
/// `method` 仍保留为字符串，便于日志与向后兼容的方法名；`params` 是按方法
/// 选择的强类型 oneof。无参数方法不设置 `params`。
#[derive(Clone, PartialEq, Message)]
pub struct Request {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(string, tag = "2")]
    pub method: String,
    #[prost(oneof = "request::Payload", tags = "10, 11, 12, 13, 14, 15")]
    pub params: Option<request::Payload>,
}

/// daemon → GUI 的响应。
///
/// 无返回值的方法不设置 `result`；失败时设置 `error`。
#[derive(Clone, PartialEq, Message)]
pub struct Response {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(oneof = "response::Payload", tags = "10, 11, 12, 13, 14")]
    pub result: Option<response::Payload>,
    #[prost(message, optional, tag = "3")]
    pub error: Option<RpcError>,
}

/// RPC 风格错误对象。
#[derive(Clone, PartialEq, Message)]
pub struct RpcError {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}

/// daemon → GUI 的主动事件。
///
/// 事件名由 oneof 的变体表达，不再需要字符串字段。
#[derive(Clone, PartialEq, Message)]
pub struct Event {
    #[prost(oneof = "event::Payload", tags = "10, 11, 12, 13, 14")]
    pub data: Option<event::Payload>,
}

// ── 通用数据消息 ──

/// 设备地址身份。
#[derive(Clone, PartialEq, Message)]
pub struct DeviceIdentity {
    #[prost(string, optional, tag = "1")]
    pub usb: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub mdns: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub ip: Option<String>,
}

/// IPC 中的设备信息。
#[derive(Clone, PartialEq, Message)]
pub struct Device {
    /// UUID 字符串形式。
    #[prost(string, tag = "1")]
    pub uuid: String,
    #[prost(string, tag = "2")]
    pub id: String,
    #[prost(string, tag = "3")]
    pub serial: String,
    /// ADB 状态文本，例如 `Device` / `Offline` / `Unauthorized`。
    #[prost(string, tag = "4")]
    pub state: String,
    #[prost(string, tag = "5")]
    pub name: String,
    #[prost(message, optional, tag = "6")]
    pub identity: Option<DeviceIdentity>,
}

impl From<&CoreDevice> for Device {
    fn from(device: &CoreDevice) -> Self {
        Self {
            uuid: device.uuid.to_string(),
            id: device.id.clone(),
            serial: device.serial.clone(),
            state: format!("{:?}", device.state),
            name: device.name.clone(),
            identity: Some(DeviceIdentity::from(&device.identity)),
        }
    }
}

impl From<&CoreDeviceIdentity> for DeviceIdentity {
    fn from(identity: &CoreDeviceIdentity) -> Self {
        Self {
            usb: identity.usb.clone(),
            mdns: identity.mdns.clone(),
            ip: identity.ip.clone(),
        }
    }
}

/// 设备列表载荷。
#[derive(Clone, PartialEq, Message)]
pub struct DeviceListData {
    #[prost(message, repeated, tag = "1")]
    pub devices: Vec<Device>,
}

/// `session.updated` 事件 / 会话列表载荷。
#[derive(Clone, PartialEq, Message)]
pub struct SessionListData {
    #[prost(message, repeated, tag = "1")]
    pub sessions: Vec<SessionSummary>,
}

/// 会话摘要。
#[derive(Clone, PartialEq, Message)]
pub struct SessionSummary {
    /// UUID 字符串形式。
    #[prost(string, tag = "1")]
    pub uuid: String,
    #[prost(string, tag = "2")]
    pub id: String,
    #[prost(string, tag = "3")]
    pub serial: String,
    #[prost(string, tag = "4")]
    pub name: String,
    #[prost(string, tag = "5")]
    pub state: String,
    #[prost(string, tag = "6")]
    pub session_state: String,
    #[prost(bool, tag = "7")]
    pub clipboard_sync: bool,
    #[prost(bool, tag = "8")]
    pub notification_sync: bool,
    #[prost(bool, tag = "9")]
    pub audio_enabled: bool,
    #[prost(uint32, tag = "10")]
    pub volume: u32,
    #[prost(uint64, tag = "11")]
    pub audio_buffer_ms: u64,
}

impl From<&CoreSessionSummary> for SessionSummary {
    fn from(session: &CoreSessionSummary) -> Self {
        Self {
            uuid: session.uuid.to_string(),
            id: session.id.clone(),
            serial: session.serial.clone(),
            name: session.name.clone(),
            state: session.state.clone(),
            session_state: session.session_state.clone(),
            clipboard_sync: session.clipboard_sync,
            notification_sync: session.notification_sync,
            audio_enabled: session.audio_enabled,
            volume: u32::from(session.volume),
            audio_buffer_ms: session.audio_buffer_ms,
        }
    }
}

/// `clipboard.changed` 事件载荷。
#[derive(Clone, PartialEq, Message)]
pub struct ClipboardData {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(string, tag = "2")]
    pub serial: String,
}

/// `notification` 事件载荷。
#[derive(Clone, PartialEq, Message)]
pub struct NotificationData {
    #[prost(string, tag = "1")]
    pub serial: String,
    #[prost(string, tag = "2")]
    pub title: String,
    #[prost(string, tag = "3")]
    pub text: String,
    #[prost(string, tag = "4")]
    pub app: String,
}

// ── 请求参数 ──

pub mod request {
    use super::*;

    /// 仅携带设备 serial 的参数。
    #[derive(Clone, PartialEq, Message)]
    pub struct DeviceSerial {
        #[prost(string, tag = "1")]
        pub serial: String,
    }

    /// 仅携带 UUID 的参数。
    #[derive(Clone, PartialEq, Message)]
    pub struct UuidParam {
        #[prost(string, tag = "1")]
        pub uuid: String,
    }

    /// `session.update` 参数。
    ///
    /// bool / volume 使用 optional 以区分“未提供”和“false / 0”。
    #[derive(Clone, PartialEq, Message)]
    pub struct SessionUpdate {
        #[prost(string, tag = "1")]
        pub uuid: String,
        #[prost(bool, optional, tag = "2")]
        pub clipboard_sync: Option<bool>,
        #[prost(bool, optional, tag = "3")]
        pub notification_sync: Option<bool>,
        #[prost(bool, optional, tag = "4")]
        pub audio_sync: Option<bool>,
        #[prost(uint32, optional, tag = "5")]
        pub volume: Option<u32>,
    }

    /// `settings.set_autostart` 参数：目标开关状态。
    #[derive(Clone, PartialEq, Message)]
    pub struct SetAutostart {
        #[prost(bool, tag = "1")]
        pub enabled: bool,
    }

    /// 按方法区分的请求参数 oneof。
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Payload {
        /// `device.connect`
        #[prost(message, tag = "10")]
        DeviceConnect(DeviceSerial),
        /// `device.disconnect`
        #[prost(message, tag = "11")]
        DeviceDisconnect(DeviceSerial),
        /// `session.update`
        #[prost(message, tag = "12")]
        SessionUpdate(SessionUpdate),
        /// `session.retry`
        #[prost(message, tag = "13")]
        SessionRetry(UuidParam),
        /// `session.start`
        #[prost(message, tag = "14")]
        SessionStart(DeviceSerial),
        /// `settings.set_autostart`
        #[prost(message, tag = "15")]
        SetAutostart(SetAutostart),
    }
}

// ── 响应结果 ──

pub mod response {
    use super::*;

    /// `device.list` / `device.adb_list` 的结果。
    #[derive(Clone, PartialEq, Message)]
    pub struct DeviceList {
        #[prost(message, repeated, tag = "1")]
        pub devices: Vec<super::Device>,
    }

    /// `clipboard.get` 的结果。
    #[derive(Clone, PartialEq, Message)]
    pub struct ClipboardGet {
        #[prost(string, tag = "1")]
        pub text: String,
    }

    /// `pairing.info` 的结果。
    #[derive(Clone, PartialEq, Message)]
    pub struct PairingInfo {
        #[prost(string, tag = "1")]
        pub dns_id: String,
        #[prost(string, tag = "2")]
        pub psk: String,
        #[prost(string, tag = "3")]
        pub wifi_string: String,
    }

    /// `settings.get` / `settings.set_autostart` 的结果。
    #[derive(Clone, PartialEq, Message)]
    pub struct Settings {
        /// 是否开启自启动（config 真源值）
        #[prost(bool, tag = "1")]
        pub autostart_enabled: bool,
        /// 当前平台是否支持自启动
        #[prost(bool, tag = "2")]
        pub autostart_supported: bool,
    }

    /// 按方法区分的结果 oneof。
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Payload {
        /// `device.list`
        #[prost(message, tag = "10")]
        DeviceList(DeviceList),
        /// `device.adb_list`
        #[prost(message, tag = "11")]
        AdbList(DeviceList),
        /// `clipboard.get`
        #[prost(message, tag = "12")]
        ClipboardGet(ClipboardGet),
        /// `pairing.info`
        #[prost(message, tag = "13")]
        PairingInfo(PairingInfo),
        /// `settings.get` / `settings.set_autostart`
        #[prost(message, tag = "14")]
        Settings(Settings),
    }
}

// ── 事件数据 ──

pub mod event {
    /// daemon 主动推送的事件数据 oneof。
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Payload {
        /// `device.updated`
        #[prost(message, tag = "10")]
        DeviceUpdated(super::DeviceListData),
        /// `adb.updated`
        #[prost(message, tag = "11")]
        AdbUpdated(super::DeviceListData),
        /// `session.updated`
        #[prost(message, tag = "12")]
        SessionUpdated(super::SessionListData),
        /// `clipboard.changed`
        #[prost(message, tag = "13")]
        ClipboardChanged(super::ClipboardData),
        /// `notification`
        #[prost(message, tag = "14")]
        Notification(super::NotificationData),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_conversion_keeps_debug_state_text() {
        let core = CoreDevice {
            uuid: uuid::Uuid::new_v4(),
            id: "serialno".into(),
            serial: "192.168.1.2:5555".into(),
            state: crate::types::DeviceState::Device,
            name: "Pixel".into(),
            identity: CoreDeviceIdentity::default(),
        };
        let device = Device::from(&core);
        assert_eq!(device.uuid, core.uuid.to_string());
        assert_eq!(device.state, "Device");
        assert_eq!(device.name, "Pixel");
    }

    #[test]
    fn request_roundtrip_with_typed_params() {
        let req = Request {
            id: 42,
            method: "session.start".into(),
            params: Some(request::Payload::SessionStart(request::DeviceSerial {
                serial: "R58N1234567".into(),
            })),
        };
        let bytes = req.encode_to_vec();
        let decoded = Request::decode(&bytes[..]).unwrap();
        assert_eq!(decoded, req);
    }

    #[test]
    fn event_roundtrip_with_typed_data() {
        let event = Event {
            data: Some(event::Payload::SessionUpdated(SessionListData {
                sessions: Vec::new(),
            })),
        };
        let bytes = event.encode_to_vec();
        let decoded = Event::decode(&bytes[..]).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn response_settings_roundtrip() {
        // 回归：Response.result oneof 必须声明 tag=14，settings.get/set_autostart 才能编码/解码
        let resp = Response {
            id: 7,
            result: Some(response::Payload::Settings(response::Settings {
                autostart_enabled: true,
                autostart_supported: true,
            })),
            error: None,
        };
        let bytes = resp.encode_to_vec();
        let decoded = Response::decode(&bytes[..]).unwrap();
        assert_eq!(decoded, resp);
    }

    #[test]
    fn request_set_autostart_roundtrip() {
        let req = Request {
            id: 8,
            method: "settings.set_autostart".into(),
            params: Some(request::Payload::SetAutostart(request::SetAutostart {
                enabled: true,
            })),
        };
        let bytes = req.encode_to_vec();
        let decoded = Request::decode(&bytes[..]).unwrap();
        assert_eq!(decoded, req);
    }
}
