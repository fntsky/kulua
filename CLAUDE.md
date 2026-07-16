# ADB Wireless Tool — Project Guide

## Project Overview

Rust 实现的 ADB 无线配对 & 剪贴板同步工具，通过 Wi-Fi 与 Android 手机互通，**手机上无需安装任何软件**。

**核心思路：** 利用 Android 系统自带的 ADB 调试功能 + `adb push scrcpy-server.jar` 到 `/data/local/tmp/` 后运行，实现"无安装"的剪贴板读取。

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
- 依赖 `scrcpy-server.jar`（scrcpy 官方的 server jar），项目根目录或同级需存在此文件
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
| `scrcpy` | scrcpy-server 部署/启停 + 剪贴板协议解析 | 不管理设备列表 |
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

## Current State (2026-07-12)

- ✅ mDNS 发现配对设备（`_adb-tls-pairing._tcp.local.`）
- ✅ 生成并终端打印配对 QR 码
- ✅ 自动配对 + 连接已发现的设备
- ✅ Push scrcpy-server 到手机并启动
- ✅ 剪贴板监听（手机→PC，仅控制台打印）
- ❌ 剪贴板写入（PC→手机）
- ❌ 系统剪贴板集成（`arboard` 已在 Cargo.toml 但未使用）
- ❌ 交互式 CLI
- ❌ `adb connect` 阶段 mDNS 监听（`_adb-tls-connect._tcp.local.`）被注释掉
- ⚠️ 20 个编译 warning（dead code 需要清理）

## Key Dependencies

- `rqrr` / `image` / `qrcode` — QR 码生成与渲染
- `mdns-sd` — mDNS 服务发现
- `arboard` — 系统剪贴板（已依赖但未使用）
- `rand` — 配对码生成

## Hidden Pitfalls / Known Decisions

- 端口分配从 27183 开始递增（`Core::run` 中的 `clipboard_port`）
- scrcpy 控制协议: 端口对应 `localabstract:scrcpy-control`，消息类型 `0x09` 为剪贴板事件
- scrcpy-server 用 `adb forward tcp:<port> localabstract:scrcpy-control` 映射
- 配对后需 sleep 10s 等待设备进入 `device` 状态再连接
- `ScrcpyServer::deploy_and_start` 与 `deploy_clipboard_only` 几乎重复，未来考虑合并

## Skills

- `/cmt` — 自动分析 git diff 并生成符合规范的 commit message（英文标题 + WHY 正文）
