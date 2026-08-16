# Kulua

通过 Wi-Fi 无线管理 Android 设备并同步剪贴板——**手机上无需安装任何软件**。

## How It Works

利用 Android 系统自带的 ADB 调试功能，`adb push scrcpy-server.jar` 到设备后通过 `app_process` 启动，实现"零安装"的剪贴板读取和设备管理。

```
┌──────────────────────────────────────────────────────┐
│                      Desktop                          │
│  ┌──────────┐   TCP IPC    ┌────────────┐            │
│  │  GUI      │◄───────────►│   daemon   │            │
│  │ (Tauri)   │ Protobuf    │  (Rust)    │            │
│  └──────────┘              └──────┬─────┘            │
│                                    │                  │
│                           ┌────────┴────────┐        │
│                           │  adb / scrcpy    │        │
│                           │  clipboard sync   │        │
│                           │  notification sync│        │
│                           │  device mgmt      │        │
│                           └────────┬────────┘        │
│                                    │                  │
│                          Wi-Fi / USB                  │
├──────────────────────────────────────────────────────┤
│                      Phone                            │
│            ┌──────────────────────────┐               │
│            │  scrcpy-server (via adb) │               │
│            │  clipboard listener      │               │
│            │  notification forward    │               │
│            └──────────────────────────┘               │
└──────────────────────────────────────────────────────┘
```

## Features

- **无线配对** — mDNS 发现设备 + QR 码配对，无需 USB 线
- **剪贴板同步** — 手机↔PC 双向实时同步（含防回环）
- **设备管理** — 按 UUID 索引设备，自动合并多地址（USB / mDNS / IP）设备身份
- **通知转发** — 手机通知实时推送到桌面
- **Session 配置** — 每设备独立开关（剪贴板同步 / 通知同步 / 音频开关）
- **跨平台 GUI** — Tauri v2 + Vue 3 原生桌面界面
- **音频转发** — 预留 scrcpy 音频协议支持（实验性）
- **融合模式** — session 运行中可从 GUI 选择手机应用，以 scrcpy 4.0 融合模式
  （`--new-display` + `-x` 弹性显示器）打开独立原生窗口，可同时开多个、独立关闭

## Architecture

```
wireless_pair (mDNS discover + QR pairing)
    ↓ MdnsEvent
Core (device mgmt + session orchestration)
    ├─ AdbCmd (adb cli wrapper)
    ├─ Session (per-device lifecycle)
    │    └─ ScrcpyServer (push & run jar, clipboard listener)
    ├─ IpcServer (TCP Protobuf IPC for GUI)
    └─ cli (QR terminal output)
```

| Module | Responsibility |
|--------|---------------|
| `adb_cmd` | ADB CLI 进程调用（`devices`, `pair`, `push`, `forward`, `shell`, `getprop`） |
| `app` / `Core` | 设备发现、身份合并、连接调度、Session 编排 |
| `session` | 单设备声明周期管理（scrcpy 部署、剪贴板 I/O、通知轮询、音频） |
| `scrcpy` | scrcpy-server 部署/启停（scid 会话隔离 + 精准 kill）+ 剪贴板协议解析 |
| `apps` | 设备应用枚举（server 一次性模式 `list_apps=true`，60s 缓存） |
| `fusion` | 融合窗口管理（scrcpy.exe 进程生命周期：启动 / 回收 / 优雅关闭） |
| `wireless_pair` | mDNS 发现 + QR 码生成 + 配对信息 |
| `ipc` | TCP Protobuf IPC 服务（GUI 通信） |
| `cli` | 终端交互界面 |
| `types` | 共享类型定义（`Device`, `DeviceEntry`, `PendingEntry`） |
| `device_refresh` | 后台 adb track-devices 长连接，推送设备状态变更 |
| `audio_player` | 音频解码与播放（集成 rodio + Opus） |
| `notification` | 桌面通知展示 |

### Core Data Model

```
DeviceEntry { device, session?, config }
    │
    ├─ Device { uuid, id, serial, state, name, identity }
    │     uuid  : 不可变主键（生成时分配）
    │     id    : adb getprop ro.serialno（硬件序列号）
    │     serial: 当前最佳连接地址
    │     identity: 三层地址集（USB / mDNS / IP）
    │
    ├─ session::Handle (Option)
    │     └─ 每个 Device 至多一个 session
    │
    └─ SessionConfig { clipboard_sync, notification_sync, audio_enabled }
```

设备发现与合并流程：

1. mDNS 发现 → 加入 `pending_serials` 环（round-robin，失败 5 次自动丢弃）
2. Tick 循环 → `adb connect` → 设备出现在 `adb track-devices`
3. `insert_device()` → `adb shell getprop ro.serialno` → 匹配或创建 `DeviceEntry`
4. 同一硬件设备的不同地址（USB / mDNS / IP）自动合并到同一个 UUID 条目

### IPC Protocol

GUI 与 daemon 之间通过 **TCP 本地回环 + Protobuf** 通信：

```
┌────────────────┬──────┬─────────────────────┐
│  length (u32)  │ type │     payload (N)     │
│  little-endian │ (u8) │                     │
└────────────────┴──────┴─────────────────────┘
```

- GUI 侧按单客户端使用
- Request / Response / Event / Audio 四种帧类型，RPC 载荷为 Protobuf 强类型消息
- 端口号写入 `%TEMP%/sync-daemon.port`

#### RPC 方法

| Method | Direction | Description |
|--------|-----------|-------------|
| `device.list` | Request | 获取合并后设备列表（含 uuid / serial / state / name） |
| `device.adb_list` | Request | 获取 adb 原始设备列表（含 Offline/Unauthorized） |
| `device.connect` | Request | 发起 adb connect |
| `device.disconnect` | Request | 断开设备连接 |
| `session.start` | Request | 点击 ADB 设备建立 session（daemon 侧幂等校验） |
| `session.update` | Request | 更新 session 配置（按 uuid 标识设备） |
| `session.retry` | Request | 重试 failed 状态的 session（按 uuid 标识设备） |
| `clipboard.get` | Request | 读取 PC 端剪贴板 |
| `pairing.info` | Request | 获取配对二维码信息 |
| `app.list` | Request | 枚举设备可启动应用（60s 缓存，`force` 可刷新） |
| `app.open` | Request | 以融合模式窗口打开应用（返回 window_id） |

#### 推送事件

| Event | Payload | Description |
|-------|---------|-------------|
| `device.updated` | `Vec<Device>` | 合并后设备列表变更（实时推送） |
| `adb.updated` | `Vec<Device>` | adb 原始设备列表变更（含 Offline/Unauthorized） |
| `session.updated` | `SessionListData`（Protobuf） | Session 状态/配置变更（含 uuid / session_state / clipboard_sync / notification_sync / audio_enabled / audio_buffer_ms） |
| `clipboard.changed` | `ClipboardData` | 剪贴板变更 |
| `notification` | `NotifData` | 手机通知推送 |
| `app.windows-updated` | `AppWindowsUpdated` | 融合窗口启动 / 退出 / 失败 |

详见 [IPC-DESIGN.md](./IPC-DESIGN.md)。

## Getting Started

### Prerequisites

- Rust 2024 edition toolchain
- [adb](https://developer.android.com/tools/adb)（`PATH` 中或项目目录下的 `adb.exe`）
- Node.js 18+（构建 GUI）
- Android 设备（Android 11+ 推荐无线调试）
- scrcpy-win64 v4.0 运行时（融合模式需要；`vendor/scrcpy-win64`、`SCRCPY_HOME` 或 PATH 中）

### Build & Run

```bash
# debug 构建
./build.sh debug

# release 构建
./build.sh release
```

或手动分步构建：

```bash
# 后台服务
cargo build --package daemon

# GUI
cd ui
npm install
npm run tauri dev
```

首次使用前需要将 `scrcpy-server` jar 文件放在项目根目录或 daemon 同级目录。

Windows 发布包（`build.ps1 -Release`）会自动从 `vendor/scrcpy-win64`、`$env:SCRCPY_HOME`
或 PATH 查找并打包 scrcpy 融合窗口运行时（`scrcpy.exe` + `SDL3.dll` + FFmpeg DLL），
也可用 `-ScrcpyDir <目录>` 显式指定。缺少运行时不影响基础功能，仅融合模式不可用。

## Project Structure

```
kulua/
├── daemon/              # 后台系统托盘服务 (Rust)
│   └── src/main.rs
├── sync-core/           # 核心库 (Rust)
│   └── src/
│       ├── app.rs       # 设备管理主循环 + insert_device
│       ├── session/     # 会话生命周期（含 config / handle / runner / proto）
│       ├── scrcpy.rs    # scrcpy-server 交互（scid 会话隔离）
│       ├── apps.rs      # 设备应用枚举（server 一次性模式 + 缓存）
│       ├── fusion.rs    # 融合窗口管理（scrcpy.exe 进程）
│       ├── adb_cmd.rs   # ADB CLI 封装
│       ├── ipc/         # TCP Protobuf IPC 服务
│       ├── wireless_pair.rs
│       ├── device_refresh.rs
│       ├── audio_player.rs
│       └── notification.rs
├── ui/                  # 桌面 GUI (Tauri v2 + Vue 3)
│   ├── src/             # Vue 3 前端（App.vue）
│   └── src-tauri/       # Tauri Rust 后端（复用 sync-core IPC 消息）
├── vendor/scrcpy-win64/ # scrcpy v4.0 融合窗口运行时（打包用）
├── build.sh             # 构建脚本 (Linux/macOS)
├── build.ps1            # 构建脚本 (Windows)
├── scrcpy-server        # scrcpy server jar
└── IPC-DESIGN.md        # IPC 协议设计文档
```

## Core Constraints

- **手机上绝不安装任何 APK** — 所有功能通过 `adb push` + `adb shell app_process` 实现
- 依赖 `scrcpy-server.jar`（scrcpy 官方的 server jar）
- 需要本地有 `adb` 可执行文件
- 融合窗口复用官方 `scrcpy.exe`（不在 Rust 中实现视频解码/渲染/输入注入），
  手机上仍不装任何 APK；融合窗口与 Kulua session 通过 scid 隔离互不干扰

## Status

- ✅ mDNS 发现配对设备
- ✅ QR 码配对
- ✅ 自动配对 + 连接已发现的设备
- ✅ Push scrcpy-server 到手机并启动
- ✅ 剪贴板监听（手机↔PC 双向）
- ✅ 通知转发与桌面展示
- ✅ TCP IPC 服务（daemon ↔ GUI）
- ✅ Tauri v2 GUI（设备列表、连接管理、配置开关）
- ✅ UUID 主键设备索引（多地址自动合并）
- ✅ Session 配置（每设备独立开关，IPC 实时同步 UI）
- ✅ Session 状态机（connecting/running/failed 反馈 UI，failed 可重试）
- ✅ 音频缓冲延迟显示（audio_buffer_ms，UI 显示播放队列积压 ms）
- ✅ ADB 连接页面（tab 切换，显示 adb 原始设备列表含离线/未授权）
- ✅ pending_serials 环（自动重试，失败 5 次丢弃）
- ✅ scid 会话隔离（session 重启/清理按 scid 精准 kill，不误杀融合窗口）
- ✅ 应用列表（GUI 选择设备应用，server 一次性模式枚举 + 缓存）
- ✅ 融合模式窗口（scrcpy.exe --new-display -x 独立窗口，可多开、独立关闭）
- ⏳ 音频转发（实验性，支持 Opus 解码 + rodio 播放）

## License
Apache 2.0
