# ADB Wireless Tool — Project Guide

## Project Overview

Rust 实现的 ADB 无线配对 & 剪贴板同步工具，通过 Wi-Fi 与 Android 手机互通，**手机上无需安装任何软件**。

**核心思路：** 利用 Android 系统自带的 ADB 调试功能 + `adb push` 自研 `kulua-server.jar`（`kulua-server/` 目录，Java 源码 + build.ps1 构建）到 `/data/local/tmp/` 后运行，实现"无安装"的剪贴板读取 / 多窗口融合视频 / 音频回传。

## Architecture

```
wireless_pair (mDNS discover + QR pairing)
    ↓ MdnsEvent
Core (device mgmt + session orchestration)
    ├─ AdbCmd (adb cli wrapper)
    ├─ Session (per-device lifecycle)
    │    └─ ScrcpyServer (push&run jar, UDP port deploy, kill_by_scid)
    └─ cli (QR terminal output)
```

## Core Constraints (‼️ 不可违背)

- **手机上绝不安装任何 APK** — 所有功能必须通过 `adb push` + `adb shell app_process` 实现
- 依赖自研 `kulua-server.jar`（项目根目录或同级需存在此文件；`kulua-server/build.ps1` 构建，替代官方 scrcpy-server.jar）
- 需要本地有 `adb` 可执行文件（`./adb.exe` 或 `PATH` 中）

## Code Conventions

### Rust Style

- **Error Handling:** 统一使用 `crate::types::AdbError` enum，优先用 `?` 传播，避免 `.unwrap()` / `.expect()`（仅在不可能是 Err 的地方使用）
- **Imports:** 按 `std → external crate → crate` 三级分组，每组按字母序
- **Naming:** 常规 Rust 命名法 — `snake_case` for fn/var, `CamelCase` for type/enum, `SCREAMING_CASE` for const
- **Comments:** 中文注释 OK，关键逻辑要写注释说明 WHY 而不是 WHAT
- **Dead code:** 无用代码立即删除或标记 `#[allow(dead_code)]` 并附理由；不遗留编译 warning
- **`use`:** 只导入用到的符号，不用 glob `use crate::*;`，不用多余的 `use xxx::{self, ...}`
- **`match`:** 穷尽匹配；对 `AdbError` 等 enum 尽可能处理所有变体，而非通配 `_ => {}`

### Module Boundaries

| Module | Responsibility | Must NOT |
|--------|---------------|----------|
| `adb_cmd` | ADB CLI 进程调用（`devices`, `pair`, `push`, `shell`, `getprop`） | 不包含业务逻辑，不处理直连协议 |
| `core` | 设备发现/连接/会话调度 | 不直接解析 scrcpy 协议 |
| `session` | 单设备生命周期管理 | 不直接调用 adb 进程 |
| `scrcpy` | kulua-server 部署/启停（scid 会话隔离 + 精准 kill）+ 直连地址解析 | 不管理设备列表 |
| `apps` | 设备应用枚举（server 一次性模式 `list_apps=true`，解析 + 60s 缓存） | 不管理窗口进程 |
| `fusion` | 融合窗口管理（fusion-viewer.exe 查找/启动/回收/优雅关闭） | 不解析 scrcpy 协议 |
| `wireless_pair` | mDNS 发现 + QR 码生成 + 配对信息 | 不发起 adb 连接 |
| `cli` | 终端交互界面 | 不包含业务逻辑 |
| `types` | 共享类型定义（`Device`, `AdbError`, `DeviceState`） | 不包含实现逻辑 |

### Functions: Single Responsibility

- 一个函数只做一件事。超过 30 行的函数考虑拆分
- `pub fn` 必须写 doc comment（中文或英文均可）
- 返回 `Result<_, AdbError>` 而非 `Result<_, Box<dyn Error>>`

### Thread Safety

- Session 中的 scrcpy 监听器是 `JoinHandle` 线程，通过 channel 通信
- 跨线程共享数据优先用 `mpsc::channel`，而非 `Arc<Mutex<>>`
- 全局状态集中在 `Core` 中管理，避免分散的 `static`

## Git Conventions

- **分支:** 默认 `main`，功能开发开 `feat/<name>`，修复开 `fix/<name>`
- **Commit message:** 英文，前缀式：`fix:`, `feat:`, `refactor:`, `docs:`, `chore:`, `cleanup:`
- **Commit body:** 说明 WHY 而不是 WHAT
- **提交前:** 确保 `cargo check` 无 warning，`cargo fmt` 已运行

## Tests

- 尚无测试，但 `adb_cmd::parse_devices` 等纯函数适合加单元测试
- 测试文件放在 `src/` 对应模块的 `#[cfg(test)] mod tests` 中
- 集成测试涉及 adb 真实设备/进程，在 `tests/` 目录

## Current State (2026-08-16)

- ✅ mDNS 发现配对设备（`_adb-tls-pairing._tcp.local.`）
- ✅ 生成并终端打印配对 QR 码
- ✅ 自动配对 + 连接已发现的设备
- ✅ Push 自研 kulua-server.jar 到手机并启动（`kulua-server/` Java 工程：单进程多虚拟显示器 + 剪贴板 + 音频回传）
- ✅ 剪贴板双向同步（手机→PC + PC→手机）
- ✅ scid 会话隔离（`0x4B4Cxxxx`，server 参数 `scid=<hex>`，按 scid 精准 kill）
- ✅ 应用列表（`apps.rs`，server 一次性模式 `list_apps=true`，60s 缓存）
- ✅ 融合模式窗口（`fusion-viewer` crate：winit 窗口 + FFmpeg 软解 + 自研控制协议，
  完全自研不依赖官方 scrcpy.exe；连接模式 `--connect ip:port` 直连 session 的
  kulua-server，多窗口 = 多个 UDP 客户端各自 CreateDisplay 虚拟显示器；`fusion.rs`
  管理其进程生命周期）
- ✅ GUI 应用选择器（搜索 / 隐藏系统应用 / 刷新 / 打开反馈）
- ✅ 系统剪贴板集成（Windows 使用 `clipboard-win`，跨平台抽象待补）
- ❌ 交互式 CLI
- ❌ `adb connect` 阶段 mDNS 监听（`_adb-tls-connect._tcp.local.`）被注释掉
- ⚠️ 仍有历史 dead code 待清理（`protocol/clipboard.rs` 旧分析实现）

## Key Dependencies

- `rqrr` / `image` / `qrcode` — QR 码生成与渲染
- `mdns-sd` — mDNS 服务发现
- `arboard` — 系统剪贴板（已依赖但未使用）
- `rand` — 配对码生成

## Hidden Pitfalls / Known Decisions

- 会话端口分配从 27183 开始递增（`Core::run` 中的 `port_counter`）——既是 scid 输入又是
  phone 上 kulua-server 绑定的 **UDP 端口**（不再有 adb forward / 本地 TCP 端口）
- 每个 Kulua session 有确定性 scid：`0x4B4C0000 | port`，server 加 `scid=<8位hex>` +
  `port=<n>` 参数；**直连协议见 `proto/direct.proto` 与 `docs/direct-udp-protocol.md`**：
  - 传输：phone 绑定 Wi-Fi 网络 UDP 端口（0.0.0.0:port），PC 端以
    `resolve_device_ip`（serial IP 优先，USB 走 `ip route` 的 src）直连，**无 adb forward**
  - 每个 datagram = 一个 protobuf `Frame`：流（CTRL/AUDIO/VIDEO）+ 类型（DATA/ACK/HELLO
    /HELLO_ACK/HEARTBEAT/BYE）+ seq/msg_id/frag + 媒体 pts/flags
  - 可靠性：CTRL 流滑动窗口(32) + 累积 ACK + 超时重传（RTO 200ms 指数退避），两端
    实现一致（`kulua-proto/src/reliable.rs` ↔ `kulua-server/.../ReliableControl.java`）
  - HELLO{scid,audio} → HELLO_ACK；媒体 config（SPS/PPS、AudioSpecificConfig）走可靠
    MediaConfig；audio/video 数据帧尽力而为（≤1200B 分片，丢片整帧丢弃）
  - server 按源地址（IP:port）解复用客户端会话，60s 空闲拆除；PC 发 HEARTBEAT 保活
  - 音频编码热切换：daemon 发 `SetAudioCodec` → phone 重起捕获 → 回 `AudioReady`
- 控制消息语义沿用 scrcpy v4.0 + 多显示器扩展：touch/scroll/start_app/resize 带 displayId
  （protobuf 明确的字段，不再手工 u32be 前缀）
- **禁止**恢复 broad kill（`grep com.genymobile.scrcpy` 全量击杀）——会误杀融合窗口；
  清理一律用 `scrcpy::kill_by_scid`（遍历 /proc cmdline 匹配 scid）
- **kulua-server 自愈退出**（v0.8.0+）：不做任何主动清理——最后一个客户端断开
  （BYE / 心跳连续 10 拍未收到 ≈55s）后 server 进程自动退出释放端口；启动后 30s
  无客户端接入同样自动退出。不残留进程，EADDRINUSE 自然消失；多窗口 = 多个 viewer
  连同一个 server，互不影响
- 融合窗口由 `fusion-viewer` crate 自研实现：直连 session 已部署的 kulua-server
  （`--connect ip:port`，scid 由端口推导），CreateDisplay 创建虚拟显示器（连接驱动，
  server 回 DisplayReady）；config 帧 avcC→Annex-B 后作为 extradata 喂给 FFmpeg
  （vivo 等设备 IDR 不带 SPS/PPS）
- 构建 fusion-viewer 需要干净 PATH（msys2/mingw64 会污染 ffmpeg-sys-next 的 C 编译），
  统一用 `scripts\build-viewer.cmd` 执行 cargo（含 anaconda 的 protoc，供 prost-build）
- viewer 只做视频 + 输入注入：音频/剪贴板继续由 Kulua session 负责（session 的
  control/audio 连接与 viewer 的 control/video 连接共存于同一 server 进程，互不干扰）
- 应用列表解析：kulua-server（AppLister.java）输出 `List of apps:` 后每行 ` * `（系统）/ ` - `（普通）
  + 名称补位 30 列 + 包名；名称超 30 字符时包名在下一行（续行）；格式与官方 scrcpy 一致
- 融合模式要求 Android 10+（API 29），`app.open` 前先校验 `ro.build.version.sdk`
- `deploy_scrcpy` 部署参数：`scid=<hex>` + `port=<n>`（UDP 端口）+ 可选
  `audio_codec` / `audio_bit_rate` / `video_bit_rate`，server 端 `Options` 解析
  （不再有 localabstract）
- kulua-server 日志 tag 带 版本（`kulua-server/<version>`，`Server.VERSION` 手动递增），
  用于确认手机上实际部署的构建；每 10s 每客户端一条 `stats` 健康日志（media_msgs /
  ctrl_pending / STALLED / 端点状态），是判断「编码器死了 vs 可靠层锁存」的首要依据

## Skills

- `/cmt` — 自动分析 git diff 并生成符合规范的 commit message（英文标题 + WHY 正文）
