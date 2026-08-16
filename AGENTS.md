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
    │    └─ ScrcpyServer (push&run jar, port forward, clipboard listener)
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
| `adb_cmd` | ADB CLI 进程调用（`devices`, `pair`, `push`, `forward`, `shell`） | 不包含业务逻辑，不处理剪贴板协议 |
| `core` | 设备发现/连接/会话调度 | 不直接解析 scrcpy 协议 |
| `session` | 单设备生命周期管理 | 不直接调用 adb 进程 |
| `scrcpy` | scrcpy-server 部署/启停（scid 会话隔离 + 精准 kill）+ 剪贴板协议解析 | 不管理设备列表 |
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
- ✅ scid 会话隔离（`0x4B4Cxxxx`，server socket `scrcpy_<hex>`，按 scid 精准 kill）
- ✅ 应用列表（`apps.rs`，server 一次性模式 `list_apps=true`，60s 缓存）
- ✅ 融合模式窗口（`fusion-viewer` crate：winit 窗口 + FFmpeg 软解 + 自研控制协议，
  完全自研不依赖官方 scrcpy.exe；连接模式 `--connect <port>` 复用 session 的 kulua-server，
  多窗口 = 多个 video 连接各自创建虚拟显示器；`fusion.rs` 管理其进程生命周期）
- ✅ GUI 应用选择器（搜索 / 隐藏系统应用 / 刷新 / 打开反馈）
- ✅ 系统剪贴板集成（Windows 使用 `clipboard-win`，跨平台抽象待补）
- ❌ 交互式 CLI
- ❌ `adb connect` 阶段 mDNS 监听（`_adb-tls-connect._tcp.local.`）被注释掉
- ⚠️ 仍有历史 dead code 待清理（`scrcpy.rs` 旧同步实现、`protocol/clipboard.rs`）

## Key Dependencies

- `rqrr` / `image` / `qrcode` — QR 码生成与渲染
- `mdns-sd` — mDNS 服务发现
- `arboard` — 系统剪贴板（已依赖但未使用）
- `rand` — 配对码生成

## Hidden Pitfalls / Known Decisions

- 端口分配从 27183 开始递增（`Core::run` 中的 `port_counter`），融合窗口用 27200:27299 避开
- 每个 Kulua session 有确定性 scid：`0x4B4C0000 | port`，server 加 `scid=<8位hex>` 参数，
  ADB forward 目标为 `localabstract:scrcpy_<8位hex>`；kulua-server 连接握手：每个连接
  首字节为类型（0x01=control / 0x02=audio / 0x03=video），video 连接携带 WxH/DPI
  创建请求，server 回 displayId + codecId 后推流；控制消息 touch/scroll/start_app/resize
  带 displayId u32be 前缀（多显示器扩展）
- **禁止**恢复 broad kill（`grep com.genymobile.scrcpy` 全量击杀）——会误杀融合窗口；
  清理一律用 `scrcpy::kill_by_scid`（遍历 /proc cmdline 匹配 scid）
- 融合窗口由 `fusion-viewer` crate 自研实现：连接 session 已部署的 kulua-server，
  video 连接握手时携带 WxH/DPI 创建虚拟显示器（连接驱动，server 回 displayId）；
  视频流为 12 字节帧头（bit63=session 元数据 / bit62=config / bit61=keyframe）+ payload，
  config 帧 avcC→Annex-B 后作为 extradata 喂给 FFmpeg（vivo 等设备 IDR 不带 SPS/PPS）；
  控制协议字段布局对照 kulua-server ControlChannel.java 与 scrcpy v4.0 源码
- 构建 fusion-viewer 需要干净 PATH（msys2/mingw64 会污染 ffmpeg-sys-next 的 C 编译），
  统一用 `scripts\build-viewer.cmd` 执行 cargo；FFmpeg 头文件/导入库在
  `vendor/ffmpeg-7.1`（FFMPEG_DIR），libclang 在 `.tools/libclang`（LIBCLANG_PATH）
- viewer 只做视频 + 输入注入：音频/剪贴板继续由 Kulua session 负责（session 的
  control/audio 连接与 viewer 的 control/video 连接共存于同一 server 进程，互不干扰）
- 应用列表解析：kulua-server（AppLister.java）输出 `List of apps:` 后每行 ` * `（系统）/ ` - `（普通）
  + 名称补位 30 列 + 包名；名称超 30 字符时包名在下一行（续行）；格式与官方 scrcpy 一致
- 融合模式要求 Android 10+（API 29），`app.open` 前先校验 `ro.build.version.sdk`
- `ScrcpyServer::deploy_and_start` 与 `deploy_clipboard_only` 几乎重复，未来考虑合并

## Skills

- `/cmt` — 自动分析 git diff 并生成符合规范的 commit message（英文标题 + WHY 正文）
