//! Kulua direct UDP 协议公共库。
//!
//! - [`generated`]：`proto/direct.proto` 生成的 protobuf 类型
//! - [`codec`]：Frame 编解码 / 媒体分片 / 接收端重装
//! - [`reliable`]：control 流滑动窗口 + 累积 ACK + 超时重传
//! - [`session`]：UDP 会话（握手 / 心跳 / 收发）——daemon 与 fusion-viewer 共用

pub mod codec;
pub mod generated;
pub mod reliable;
pub mod session;
