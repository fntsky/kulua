# Sync Workspace

通过 Wi-Fi 无线管理 Android 设备并同步剪贴板——**手机上无需安装任何软件**。

## How It Works

利用 Android 系统自带的 ADB 调试功能，`adb push scrcpy-server.jar` 到设备后通过 `app_process` 启动，实现"零安装"的剪贴板读取和设备管理。

```
┌──────────────────────────────────────────────────────┐
│                      Desktop                          │
│  ┌──────────┐   TCP IPC    ┌────────────┐            │
│  │  GUI      │◄───────────►│   daemon   │            │
│  │ (Tauri)   │ JSON-RPC    │  (Rust)    │            │
│  └──────────┘              └──────┬─────┘            │
│                                    │                  │
│                           ┌────────┴────────┐        │
│                           │  adb / scrcpy    │        │
│                           │  clipboard sync   │        │
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
- **剪贴板同步** — 手机→PC 实时同步（PC→手机 即将支持）
- **设备管理** — 列出已连接设备、查看状态、管理连接
- **通知转发** — 手机通知实时推送到桌面（WIP）
- **跨平台 GUI** — Tauri v2 + Vue 3 原生桌面界面

## Architecture

```
wireless_pair (mDNS discover + QR pairing)
    ↓ MdnsEvent
Core (device mgmt + session orchestration)
    ├─ AdbCmd (adb cli wrapper)
    ├─ Session (per-device lifecycle)
    │    └─ ScrcpyServer (push & run jar, clipboard listener)
    ├─ IpcServer (TCP JSON-RPC for GUI)
    └─ cli (QR terminal output)
```

| Module | Responsibility |
|--------|---------------|
| `adb_cmd` | ADB CLI 进程调用（`devices`, `pair`, `push`, `forward`, `shell`） |
| `app` / `Core` | 设备发现、连接、会话调度 |
| `session` | 单设备生命周期管理 |
| `scrcpy` | scrcpy-server 部署/启停 + 剪贴板协议解析 |
| `wireless_pair` | mDNS 发现 + QR 码生成 + 配对信息 |
| `ipc` | TCP JSON-RPC 服务（GUI 通信） |
| `cli` | 终端交互界面 |
| `types` | 共享类型定义（`Device`, `AdbError`, `DeviceState`） |

### IPC Protocol

GUI 与 daemon 之间通过 **TCP 本地回环 + JSON-RPC** 通信：

```
┌────────────────┬──────┬─────────────────────┐
│  length (u32)  │ type │     payload (N)     │
│  little-endian │ (u8) │                     │
└────────────────┴──────┴─────────────────────┘
```

- 单客户端独占模式
- 支持 Request/Response/Event/Audio 四种帧类型
- 端口号写入 `%TEMP%/sync-daemon.port`

详见 [IPC-DESIGN.md](./IPC-DESIGN.md)。

## Getting Started

### Prerequisites

- Rust 2024 edition toolchain
- [adb](https://developer.android.com/tools/adb)（`PATH` 中或项目目录下的 `adb.exe`）
- Node.js 18+（构建 GUI）
- Android 设备（Android 11+ 推荐无线调试）

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

## Project Structure

```
sync-workspace/
├── daemon/              # 后台系统托盘服务 (Rust)
│   └── src/main.rs
├── sync-core/           # 核心库 (Rust)
│   └── src/
│       ├── app.rs       # 设备管理主循环
│       ├── session.rs   # 会话生命周期
│       ├── scrcpy.rs    # scrcpy-server 交互
│       ├── adb_cmd.rs   # ADB CLI 封装
│       ├── ipc/         # TCP JSON-RPC 服务
│       └── wireless_pair.rs
├── ui/                  # 桌面 GUI (Tauri v2 + Vue 3)
│   ├── src/             # Vue 3 前端
│   └── src-tauri/       # Tauri Rust 后端
├── build.sh             # 构建脚本 (Linux/macOS)
├── build.ps1            # 构建脚本 (Windows)
├── scrcpy-server        # scrcpy server jar
└── IPC-DESIGN.md        # IPC 协议设计文档
```

## Core Constraints

- **手机上绝不安装任何 APK** — 所有功能通过 `adb push` + `adb shell app_process` 实现
- 依赖 `scrcpy-server.jar`（scrcpy 官方的 server jar）
- 需要本地有 `adb` 可执行文件

## Status

- ✅ mDNS 发现配对设备
- ✅ QR 码配对
- ✅ 自动配对 + 连接已发现的设备
- ✅ Push scrcpy-server 到手机并启动
- ✅ 剪贴板监听（手机→PC）
- ✅ TCP IPC 服务（daemon ↔ GUI）
- ✅ Tauri v2 GUI（设备列表、连接管理）
- ✅ 剪贴板写入（PC→手机）
- ✅ 系统剪贴板自动同步
- ❌ 音频转发（预留协议）

## License
Apache 2.0
