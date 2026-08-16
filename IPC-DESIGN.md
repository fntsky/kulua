# IPC Server Design

## Overview

为 daemon 新增 TCP-based IPC 服务，供 GUI 进程（Tauri v2）通过本地回环通信。GUI 按单客户端使用。
未来音频传输走同一条 TCP 连接的多路复用通道。

> 协议状态：Request / Response / Event 的 payload，以及各方法的 `params` / `result`
> 和事件 `data` 均已改为 Protobuf 强类型消息；IPC 路径上不再编码 JSON。

## Architecture

```
┌─────────────┐         TCP localhost          ┌──────────────┐
│  GUI (Tauri) │ ◄──── len+type+payload ────► │   daemon     │
│  singleton   │                               │  后台服务     │
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
| `0x00` | Request | GUI → daemon | `proto::Request` |
| `0x01` | Response | daemon → GUI | `proto::Response` |
| `0x02` | Event | daemon → GUI | `proto::Event` |
| `0x03` | Audio | daemon → GUI | Raw Opus frame (future) |

- **Request (0x00)** — 带 `id` 字段，需要 daemon 回复对应的 Response (0x01)。
- **Response (0x01)** — `id` 匹配对应的 Request；包含 `result` 或 `error`。
- **Event (0x02)** — 无 `id`，daemon 主动推送，GUI 不回复。
- **Audio (0x03)** — 预留类型，payload 为原始 Opus 编码音频帧。不做缓冲/排序/重传，直接 pipe；170ms 解码缓冲由 GUI 音频层负责。
- 帧层有 `MAX_FRAME_SIZE = 16 MiB` 上限，超限直接断连，防止异常客户端导致内存膨胀。

## Protobuf Messages

完整定义见 `sync-core/src/ipc/proto.rs`。核心结构如下：

```proto
message Request {
  uint64 id = 1;
  string method = 2;

  oneof params {
    DeviceSerial device_connect    = 10; // device.connect
    DeviceSerial device_disconnect = 11; // device.disconnect
    SessionUpdate session_update    = 12; // session.update
    UuidParam session_retry         = 13; // session.retry
    DeviceSerial session_start      = 14; // session.start
  }
}

message Response {
  uint64 id = 1;

  oneof result {
    DeviceList device_list   = 10; // device.list
    DeviceList adb_list      = 11; // device.adb_list
    ClipboardGet clipboard_get = 12; // clipboard.get
    PairingInfo pairing_info = 13; // pairing.info
  }

  RpcError error = 3;
}

message Event {
  oneof data {
    DeviceListData device_updated    = 10;
    DeviceListData adb_updated       = 11;
    SessionListData session_updated  = 12;
    ClipboardData clipboard_changed  = 13;
    NotificationData notification    = 14;
  }
}
```

- `DeviceSerial { string serial = 1; }`
- `UuidParam { string uuid = 1; }`
- `SessionUpdate { string uuid = 1; optional bool clipboard_sync = 2; optional bool notification_sync = 3; optional bool audio_sync = 4; optional uint32 volume = 5; }`
- `DeviceList { repeated Device devices = 1; }`
- `ClipboardGet { string text = 1; }`
- `PairingInfo { string dns_id = 1; string psk = 2; string wifi_string = 3; }`
- `DeviceListData { repeated Device devices = 1; }`
- `SessionListData { repeated SessionSummary sessions = 1; }`
- `ClipboardData { string text = 1; string serial = 2; }`
- `NotificationData { string serial = 1; string title = 2; string text = 3; string app = 4; }`

`Device` / `DeviceIdentity` / `SessionSummary` 字段与 `sync-core` 内部类型一一对应；
`Device.state` 在线路上保留为调试文本（`Device` / `Offline` / `Unauthorized` / `Unknown(...)`），
与 UI 现有展示逻辑兼容。

## Methods

### device.list
```
Request  { id: 1, method: "device.list" }
Response { id: 1, result: DeviceList { devices: [Device, ...] } }
```

### device.adb_list
返回 adb 原始设备列表（含 Offline/Unauthorized）。
```
Request  { id: 2, method: "device.adb_list" }
Response { id: 2, result: AdbList { devices: [Device, ...] } }
```

### device.connect
```
Request  { id: 3, method: "device.connect", params: DeviceSerial { serial: "..." } }
Response { id: 3 }
```

### device.disconnect
```
Request  { id: 4, method: "device.disconnect", params: DeviceSerial { serial: "..." } }
Response { id: 4 }
```

### session.start
点击 ADB 列表设备建立会话。Core 侧校验：该设备无活跃 session（connecting/running）才创建；
failed 墓碑视为可重建；非 Device 状态或已有活跃 session → no-op。
```
Request  { id: 5, method: "session.start", params: DeviceSerial { serial: "..." } }
Response { id: 5 }
```

### session.update
更新剪贴板/通知/音频开关与音量。bool 和 volume 为 optional，未提供时沿用默认值。
```
Request  {
  id: 6,
  method: "session.update",
  params: SessionUpdate {
    uuid: "...",
    clipboard_sync: true,
    notification_sync: true,
    audio_sync: false,
    volume: 80
  }
}
Response { id: 6 }
```

### session.retry
```
Request  { id: 7, method: "session.retry", params: UuidParam { uuid: "..." } }
Response { id: 7 }
```

### clipboard.get
```
Request  { id: 8, method: "clipboard.get" }
Response { id: 8, result: ClipboardGet { text: "phone clipboard content" } }
```

### pairing.info
```
Request  { id: 9, method: "pairing.info" }
Response { id: 9, result: PairingInfo { dns_id: "...", psk: "...", wifi_string: "..." } }
```

### clipboard.set *(未来)*
预留方法，当前未实现。

### Error format
```
Response { id: N, error: RpcError { code: -1, message: "device not found" } }
```

## Events (daemon → GUI)

### device.updated
```
Event { data: DeviceUpdated { devices: [Device, ...] } }
```
合并设备列表发生变化（新设备上线 / 设备下线 / 状态变更）时推送全量列表。

### adb.updated
```
Event { data: AdbUpdated { devices: [Device, ...] } }
```
adb 原始设备列表变化时推送全量列表（含 Offline/Unauthorized）。

### session.updated
```
Event { data: SessionUpdated { sessions: [SessionSummary, ...] } }
```
session 列表变化时推送全量列表（含 failed 墓碑）。

### clipboard.changed
```
Event { data: ClipboardChanged { text: "...", serial: "..." } }
```
手机剪贴板变化时推送。`serial` 标识来源设备。

### notification
```
Event { data: Notification { serial: "...", title: "...", text: "...", app: "..." } }
```
手机新通知到达时推送。

## Connection Lifecycle

1. daemon 启动，`TcpListener::bind("127.0.0.1:0")` → OS 自动分配端口。
2. 端口号写入 `%TEMP%/sync-daemon.port`。
3. daemon 启动后台 accept 循环；每接受一个 TCP 连接就 spawn 独立 handler（GUI 当前只建立一个连接）。
4. GUI 断开时，对应 handler 退出，daemon 继续 accept 新连接。
5. daemon 退出时（正常或 Ctrl+C），删除 `%TEMP%/sync-daemon.port`。

### GUI 端口发现

- 启动时读取 `%TEMP%/sync-daemon.port`。
- 文件不存在 → 提示用户启动 daemon。
- 连接失败 → 端口可能过时（daemon 崩溃残留），提示重启 daemon。
- 连接成功后先执行 `device.list` + `pairing.info` 初始握手，再开放 Tauri command 请求。

## Module Layout

```
sync-core/src/ipc/
├── mod.rs     # pub mod proto / server / types
├── proto.rs   # 所有 Protobuf 消息、oneof、Core 类型转换
├── server.rs  # IpcServer、accept 循环、连接处理、方法路由
└── types.rs   # FrameCodec、帧类型常量、MAX_FRAME_SIZE、SessionSummary
```

`ui/src-tauri/src/lib.rs` 使用 `sync_core::ipc::proto` 中同一套消息定义编码/解码，
不再在 UI 侧手写 JSON 结构。

## Core Integration

`Core::run` 启动 IPC 服务端时传入各 watch/broadcast 通道：

```rust
let server = crate::ipc::server::IpcServer::bind().await?;
crate::ipc::server::serve(
    server,
    token,
    cmd_tx,
    device_watch,
    merged_watch,
    session_watch,
    clip_broadcast,
    notif_broadcast,
    pair_info,
);
```

`handle_ipc_connection` 内部：

1. `Framed<TcpStream, FrameCodec>` 将 stream 拆为 frame。
2. `tokio::select!` 多个分支：
   - 读 Request 帧 → `dispatch_request` → 写 Response 帧。
   - `merged_watch` / `device_watch` / `session_watch` 变化 → 编码对应 Event 帧。
   - 剪贴板 / 通知 broadcast → 编码对应 Event 帧。
3. 任一方向出错或关闭 → 断开当前连接，外层 accept 循环继续等待。

## Dependencies

```toml
# sync-core/Cargo.toml
prost = { version = "0.13", features = ["derive"] }
tokio-util = { version = "0.7", features = ["codec"] }

# ui/src-tauri/Cargo.toml
prost = { version = "0.13", features = ["derive"] }
sync-core = { path = "../../sync-core" }
```

## Non-Goals (this phase)

- ❌ 认证/Token（后续按需加）
- ❌ 多客户端互斥（GUI 自身为 singleton，daemon 当前允许重复连接）
- ❌ 音频帧编解码（仅预留 type byte）
- ❌ PC→手机剪贴板写入（仅预留 `clipboard.set` method）
- ❌ TLS 加密（本地回环无中间人风险）
