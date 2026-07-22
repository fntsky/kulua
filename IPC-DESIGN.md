# IPC Server Design

## Overview

为 daemon 新增 TCP-based IPC 服务，供 GUI 进程（Tauri v2）通过本地回环通信。单客户端独占模式。
未来音频传输走同一条 TCP 连接的多路复用通道。

## Architecture

```
┌─────────────┐         TCP localhost          ┌──────────────┐
│  GUI (Tauri) │ ◄──── len+type+payload ────► │   daemon     │
│  singleton   │        独占连接 (首个 accept)   │  后台服务     │
└─────────────┘                                └──────┬───────┘
                                                      │
                                              ┌───────┴───────┐
                                              │  adb / scrcpy │
                                              │  devices       │
                                              └───────────────┘
```

## Protocol Framing

```
┌────────────────┬──────┬─────────────────────┐
│  length (u32)  │ type │     payload (N)     │
│  little-endian │ (u8) │                     │
└────────────────┴──────┴─────────────────────┘
```

| Type | Name | Direction | Payload |
|------|------|-----------|---------|
| `0x00` | Request | GUI → daemon | JSON-RPC request object |
| `0x01` | Response | daemon → GUI | JSON-RPC response object |
| `0x02` | Event | daemon → GUI | JSON event object |
| `0x03` | Audio | daemon → GUI | Raw Opus frame (future) |

- **Request (0x00)** — 带 `id` 字段，需要 daemon 回复对应的 Response (0x01)。
- **Response (0x01)** — `id` 匹配对应的 Request；包含 `result` 或 `error`。
- **Event (0x02)** — 无 `id`，daemon 主动推送，GUI 不回复。
- **Audio (0x03)** — 预留类型，payload 为原始 Opus 编码音频帧。不做缓冲/排序/重传，直接 pipe；170ms 解码缓冲由 GUI 音频层负责。

## JSON-RPC Methods

### device.list
```
→ {"id": 1, "method": "device.list", "params": {}}
← {"id": 1, "result": [Device, ...]}
```

### device.connect
```
→ {"id": 2, "method": "device.connect", "params": {"serial": "..."}}
← {"id": 2, "result": {}}
```

### device.disconnect
```
→ {"id": 3, "method": "device.disconnect", "params": {"serial": "..."}}
← {"id": 3, "result": {}}
```

### clipboard.get
```
→ {"id": 4, "method": "clipboard.get", "params": {}}
← {"id": 4, "result": {"text": "phone clipboard content"}}
```

### clipboard.set *(未来)*
```
→ {"id": 5, "method": "clipboard.set", "params": {"text": "..."}}
← {"id": 5, "result": {}}
```

### Error format
```
{"id": N, "error": {"code": -1, "message": "device not found"}}
```

## Events (daemon → GUI)

### device.updated
```
{"event": "device.updated", "data": [Device, ...]}
```
设备列表发生变化（新设备上线 / 设备下线 / 状态变更）时推送全量列表。

### clipboard.changed
```
{"event": "clipboard.changed", "data": {"text": "...", "serial": "..."}}
```
手机剪贴板变化时推送。`serial` 标识来源设备。

### notification
```
{"event": "notification", "data": {"serial": "...", "title": "...", "text": "...", "app": "..."}}
```
手机新通知到达时推送。

## Connection Lifecycle

1. daemon 启动，`TcpListener::bind("127.0.0.1:0")` → OS 自动分配端口。
2. 端口号写入 `%TEMP%/sync-daemon.port`。
3. daemon 进入 `tokio::select!` 主循环：mDNS 事件 + device refresh + TCP accept + 已有连接 I/O。
4. 首个 TCP 连接被 accept，后续连接 `accept` 后立即 `shutdown`（单客户端独占）。
5. GUI 断开时，daemon 回到 accept 状态，接受下一个连接。
6. daemon 退出时（正常或 Ctrl+C），删除 `%TEMP%/sync-daemon.port`。

### GUI 端口发现

- 启动时读取 `%TEMP%/sync-daemon.port`。
- 文件不存在 → 提示用户启动 daemon。
- 连接失败 → 端口可能过时（daemon 崩溃残留），提示重启 daemon。

## Module Layout

```
sync-core/src/ipc/
├── mod.rs          # pub mod types; pub mod server; re-exports
├── types.rs        # FrameCodec, JsonRpcRequest, JsonRpcResponse, Event, IpcError
└── server.rs       # IpcServer { listener, handle_connection }
```

### types.rs

```rust
use serde::{Deserialize, Serialize};

// ── Frame level ──

pub const FRAME_TYPE_REQUEST: u8  = 0x00;
pub const FRAME_TYPE_RESPONSE: u8 = 0x01;
pub const FRAME_TYPE_EVENT: u8    = 0x02;
pub const FRAME_TYPE_AUDIO: u8    = 0x03; // reserved

pub struct FrameCodec;
// impl Decoder for length-delimited + type-byte framing
// impl Encoder for length-delimited + type-byte framing
// 基于 tokio_util::codec

// ── JSON-RPC ──

#[derive(Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    #[serde(rename = "device.updated")]
    DeviceUpdated { data: Vec<Device> },
    #[serde(rename = "clipboard.changed")]
    ClipboardChanged { data: ClipboardData },
    #[serde(rename = "notification")]
    Notification { data: NotifData },
}

#[derive(Serialize, Deserialize)]
pub struct ClipboardData {
    pub text: String,
    pub serial: String,
}

#[derive(Serialize, Deserialize)]
pub struct NotifData {
    pub serial: String,
    pub title: String,
    pub text: String,
    pub app: String,
}

// ── IPC Error ──

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("unknown frame type: {0}")]
    UnknownFrameType(u8),
    #[error("method not found: {0}")]
    MethodNotFound(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
}
```

### server.rs

```rust
pub struct IpcServer {
    listener: TcpListener,
    port_file: PathBuf,
}

impl IpcServer {
    /// Bind 127.0.0.1:0 and write port to %TEMP%/sync-daemon.port
    pub async fn bind() -> Result<Self, IpcError>;

    /// Accept first connection, reject subsequent ones
    pub async fn accept(&mut self) -> Result<TcpStream, IpcError>;

    /// Return port number
    pub fn port(&self) -> u16;
}

impl Drop for IpcServer {
    // delete port file
}
```

## Core Integration

`app.rs` `Core::run` 主循环改造为：

```rust
pub async fn run(&mut self, cmd_rx: mpsc::Receiver<Command>) {
    let server = IpcServer::bind().await?;
    // ... existing mDNS / device refresh setup ...

    loop {
        tokio::select! {
            // === 原有分支 ===
            Some(event) = self.mdns_rx.recv() => { ... }
            _ = device_refresh_tick.tick() => { ... }

            // === 新增: IPC 连接 ===
            Ok((stream, _addr)) = server.accept() => {
                self.handle_ipc_connection(stream).await;
            }

            // === 新增: IPC 命令 (来自 Core 内部 → 写入连接的 sink) ===
            Some(cmd) = cmd_rx.recv() => { ... }

            _ = self.token.cancelled() => break;
        }
    }
}
```

`handle_ipc_connection` 内部：
1. `Framed<TcpStream, FrameCodec>` 将 stream 拆为 frame
2. `tokio::select!` 两个分支：
   - 读 frame → 解析 Request → 分发到对应 handler → 写 Response
   - 内部事件 channel（broadcast 订阅 clip/devices/notif）→ 序列化 Event → 写 Event frame
3. 任一方向出错或关闭 → 断开，回到外层 accept 循环

## Dependencies to Add

```toml
# sync-core/Cargo.toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio-util = { version = "0.7", features = ["codec"] }
thiserror = "2"
```

## Implementation Order

1. **`sync-core/src/ipc/types.rs`** — FrameCodec + JSON-RPC types + Event enum + IpcError
2. **`sync-core/src/ipc/server.rs`** — IpcServer bind/accept/drop
3. **`sync-core/src/ipc/mod.rs`** — re-exports
4. **`sync-core/src/lib.rs`** — `pub mod ipc;`
5. **`app.rs`** — 集成 `IpcServer` 到主循环 + `handle_ipc_connection`
6. **`daemon/src/main.rs`** — 写端口文件路径、移除 Ctrl+C 的 `process::exit(0)`（改为 token.cancel）
7. **`cargo check && cargo fmt`**

## Non-Goals (this phase)

- ❌ 认证/Token（后续按需加）
- ❌ 多客户端
- ❌ 音频帧编解码（仅预留 type byte 和 enum variant）
- ❌ PC→手机剪贴板写入（仅预留 `clipboard.set` method）
- ❌ TLS 加密（本地回环无中间人风险）
