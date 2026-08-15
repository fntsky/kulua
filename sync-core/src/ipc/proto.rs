//! IPC Protobuf 消息定义。
//!
//! 当前阶段使用 Protobuf 作为外层信封，内部 `params` / `result` / `data`
//! 暂时仍承载 JSON 字节，便于逐步迁移；后续再替换为强类型消息。

use prost::Message;

/// GUI → daemon 的请求。
#[derive(Clone, PartialEq, Message)]
pub struct Request {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(string, tag = "2")]
    pub method: String,
    /// 方法参数。当前为 JSON 字节；后续替换为对应方法的强类型消息。
    #[prost(bytes = "vec", tag = "3")]
    pub params: Vec<u8>,
}

/// daemon → GUI 的响应。
#[derive(Clone, PartialEq, Message)]
pub struct Response {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    /// 成功结果。当前为 JSON 字节；后续替换为强类型消息。空字节表示无结果。
    #[prost(bytes = "vec", tag = "2")]
    pub result: Vec<u8>,
    #[prost(message, optional, tag = "3")]
    pub error: Option<RpcError>,
}

/// JSON-RPC 风格错误对象。
#[derive(Clone, PartialEq, Message)]
pub struct RpcError {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}

/// daemon → GUI 的主动事件。
#[derive(Clone, PartialEq, Message)]
pub struct Event {
    /// 事件名，如 `device.updated`。
    #[prost(string, tag = "1")]
    pub event: String,
    /// 事件数据。当前为 JSON 字节；后续替换为强类型消息。
    #[prost(bytes = "vec", tag = "2")]
    pub data: Vec<u8>,
}
