# Kulua

通过 Wi-Fi 无线管理 Android 设备并同步剪贴板——**手机上无需安装任何软件**。

## How It Works

利用 Android 系统自带的 ADB 调试功能，`adb push` 自研 `kulua-server.jar`（`kulua-server/` 目录的 Java 工程，`kulua-server/build.sh`（Linux）/ `build.ps1`（Windows）构建）到设备后通过 `app_process` 启动，实现"零安装"的剪贴板读取、多窗口融合视频与音频回传。

```
┌──────────────────────────────────────────────────────┐
│                      Desktop                          │
│  ┌──────────┐   TCP IPC    ┌────────────┐            │
│  │  GUI      │◄───────────►│   daemon   │            │
│  │ (Tauri)   │ Protobuf    │  (Rust)    │            │
│  └──────────┘              └──────┬─────┘            │
│                                   │                  │
│                          ┌────────┴────────┐        │
│                          │ session/udp     │        │
│                          │ (UDP 直连客户端) │        │
│                          │ clipboard sync  │        │
│                          │ notification sync│       │
│                          │ device mgmt     │        │
│                          └────────┬────────┘        │
│                                   │                  │
│                       Wi-Fi UDP（phone IP:port 直连） │
├──────────────────────────────────────────────────────┤
│                      Phone                            │
│            ┌──────────────────────────┐               │
│            │  kulua-server via app_process │          │
│            │  UDP 端口监听（control/audio/video）       │
│            │  clipboard listener      │               │
│            │  notification forward    │               │
│            └──────────────────────────┘               │
└──────────────────────────────────────────────────────┘
```

> **直连协议**：daemon / 融合窗口与手机上的 `kulua-server` 通过 **UDP 网络端口直连**
> (phone 绑定 Wi-Fi IP 端口，无 `adb forward`)，线协议由 `proto/direct.proto`
> (protobuf) 定义：control 流可靠（滑动窗口 + ACK + 重传），audio/video 尽力而为
> （分片 + 容忍丢包）。详见 `docs/direct-udp-protocol.md`。

## Features

- **无线配对** — mDNS 发现设备 + QR 码配对，无需 USB 线
- **剪贴板同步** — 手机↔PC 双向实时同步（含防回环）
- **设备管理** — 按 UUID 索引设备，自动合并多地址（USB / mDNS / IP）设备身份
- **通知转发** — 手机通知实时推送到桌面
- **Session 配置** — 每设备独立开关（剪贴板同步 / 通知同步 / 音频开关）
- **跨平台 GUI** — Tauri v2 + Vue 3 原生桌面界面
- **音频转发** — 手机系统声音回传 PC 播放（Opus / AAC / FLAC / RAW，可热切换）
- **融合模式** — session 运行中可从 GUI 选择手机应用，在自研虚拟显示器上
  打开独立窗口（可同时开多个、独立关闭）；完全自研：设备端
  `kulua-server.jar` 创建虚拟显示器 + H.264 编码，PC 端 Tauri WebviewWindow +
  WebCodecs 硬解（无 FFmpeg 依赖），不依赖官方 scrcpy

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
| `adb_cmd` | ADB CLI 进程调用（`devices`, `pair`, `push`, `shell`, `getprop`） |
| `app` / `Core` | 设备发现、身份合并、连接调度、Session 编排 |
| `session` | 单设备声明周期管理（kulua-server 部署、UDP 直连会话、剪贴板 I/O、通知轮询、音频） |
| `kulua-proto` | 直连线协议：`proto/direct.proto` 生成的 protobuf + UDP 传输层（分片 / ACK / 重传 / 多流会话） |
| `scrcpy` | kulua-server 部署/启停（scid 会话隔离 + 精准 kill）+ 设备直连地址解析（模块名沿用历史） |
| `apps` | 设备应用枚举（server 一次性模式 `list_apps=true`，60s 缓存） |
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
- 端口号写入 `%TEMP%/sync-daemon.port`（Linux 为 `/tmp/sync-daemon.port`）

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
| `app.open` | Request | 以融合模式窗口打开应用（返回 window_id + 设备直连地址 addr，UI 自建视频客户端） |

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
- [adb](https://developer.android.com/tools/adb)（`PATH` 中，或项目目录下的 `adb` / `adb.exe`）
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

首次使用前需要将 `kulua-server.jar`（`kulua-server/build.sh`（Linux）/ `build.ps1`（Windows）构建产出）放在项目根目录或 daemon 同级目录。

- **Windows** 发布包（`build.ps1 -Release`）自动打包 daemon、GUI 与自研 `kulua-server.jar` 到 `dist/kulua/`
- **Linux** 发布包（`./build.sh release`）同样打包到 `dist/kulua/`（含自动捆绑 `adb` 与 jar）
- 融合模式无额外运行时依赖（视频解码由 WebView2 / WebKitGTK 内置 WebCodecs 硬解完成，无 FFmpeg 依赖）

## Linux 支持（feat/linux-port）

基于 `main` 分支的 Linux 移植，构建与运行全部在 Linux 上完成：

### 系统依赖（Arch / CachyOS）

```bash
sudo pacman -S rustup nodejs npm adb webkit2gtk-4.1 javascriptcoregtk-4.1 \
    gtk3 libsoup3 dbus glib2 openssl alsa-lib cmake java-openjdk
```

> Debian/Ubuntu 对应：`libwebkit2gtk-4.1-dev libjavascriptcoregtk-4.1-dev libgtk-3-dev libsoup-3.0-dev libdbus-1-dev libssl-dev libasound2-dev cmake default-jdk`
> 托盘图标需要 `libappindicator` 运行时（可选）。

### 构建

```bash
./build.sh            # debug 构建（target/debug/daemon、target/debug/sync-ui）
./build.sh release    # release 构建 + 打包 dist/kulua/
```

设备端 jar 在 Linux 上构建（需 Android SDK，`d8` 与 `android.jar`）：

```bash
ANDROID_HOME=~/Android/Sdk ./kulua-server/build.sh   # 产出 kulua-server/build/kulua-server.jar
```

### 运行

```bash
./dist/kulua/daemon        # 发布包：daemon 自动拉起 sync-ui 窗口
# 或开发模式：
cargo run --package daemon
```

- adb 查找顺序：daemon 同目录 `./adb` → `PATH`
- config 文件在 `~/.config/kulua/config.json`
- **开机自启动**：Linux 走 XDG autostart（`~/.config/autostart/kulua-daemon.desktop`），
  GUI 设置面板开关与 Windows 注册表版本等效；reconcile 会自愈不一致的条目
- 桌面通知经 D-Bus（libnotify）弹出
- **图标主题**：设置面板可在深色/浅色间切换软件窗口图标与托盘图标
  （黑/白两版 logo 内嵌在 daemon 与 GUI 中，切换即时生效，托盘热切换≤1s）

## Project Structure

```
kulua/
├── daemon/              # 后台系统托盘服务 (Rust)
│   └── src/main.rs
├── sync-core/           # 核心库 (Rust)
│   └── src/
│       ├── app.rs       # 设备管理主循环 + insert_device
│       ├── session/     # 会话生命周期（含 config / handle / runner / proto）
│       ├── scrcpy.rs    # kulua-server 部署/启停（scid 会话隔离）
│       ├── apps.rs      # 设备应用枚举（server 一次性模式 + 缓存）
│       ├── adb_cmd.rs   # ADB CLI 封装
│       ├── ipc/         # TCP Protobuf IPC 服务
│       ├── wireless_pair.rs
│       ├── device_refresh.rs
│       ├── audio_player.rs
│       └── notification.rs
├── ui/                  # 桌面 GUI (Tauri v2 + Vue 3)
│   ├── src/             # Vue 3 前端（App.vue）
│   ├── fusion.html      # 融合窗口页面（WebCodecs 解码渲染，无构建依赖）
│   └── src-tauri/       # Tauri Rust 后端（IPC 客户端 + fusion_* 窗口命令）
├── kulua-server/        # 自研设备端 server（Java：多虚拟显示器 + 剪贴板 + 音频回传）
│   ├── build.ps1        # javac + d8 + jar 构建脚本 (Windows)
│   └── build.sh         # javac + d8 + jar 构建脚本 (Linux)
├── build.sh             # 构建脚本 (Linux/macOS)
├── build.ps1            # 构建脚本 (Windows)
└── IPC-DESIGN.md        # IPC 协议设计文档
```

## Core Constraints

- **手机上绝不安装任何 APK** — 所有功能通过 `adb push` + `adb shell app_process` 实现
- 依赖自研 `kulua-server.jar`（`kulua-server/build.ps1` 构建，替代官方 scrcpy-server）
- 需要本地有 `adb` 可执行文件
- 融合窗口由 UI 进程内嵌实现（Tauri WebviewWindow + WebCodecs 硬解），
  设备端由 `kulua-server.jar` 提供虚拟显示器，不依赖官方 scrcpy；手机上仍不装任何 APK；
  融合窗口与 Kulua session 共用同一 server（scid 会话隔离）

## Status

- ✅ mDNS 发现配对设备
- ✅ QR 码配对
- ✅ 自动配对 + 连接已发现的设备
- ✅ Push 自研 kulua-server.jar 到手机并启动（单进程多虚拟显示器 + 剪贴板 + 音频）
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
- ✅ 融合模式窗口（WebviewWindow + WebCodecs 硬解：虚拟显示器 + 输入注入，
  可多开、独立关闭；无 FFmpeg 依赖；不依赖官方 scrcpy）
- ✅ 音频转发（Opus / AAC / FLAC / RAW 解码 + rodio 播放，编码可热切换）

## License
Apache 2.0
