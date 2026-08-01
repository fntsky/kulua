# Session 状态机设计（连接中状态反馈 UI）

> 状态：**已实现**（2026-08-01，grilling 收敛后落地）
> 日期：2026-08-01

## 1. 背景

现状：`SessionSummary.state` 存的是 **ADB 设备状态**（`format!("{:?}", device.state)`），
不是 session 生命周期状态。`start_session()` spawn 任务后立刻把 Handle 挂进 `DeviceEntry`，
但 scrcpy deploy（push + app_process 启动）→ TCP 重试（500ms 无限循环）→ 握手可能要数秒，
UI 在这段时间内看到的是"设备在但没有任何反馈"。

另有现存缺陷：session task 意外结束（server 崩溃）后，Handle 仍挂在 `DeviceEntry.session` 上，
`sync_sessions` 只在 `session.is_none()` 时重启 → **僵尸 handle 永不清理、永不重启**，同步无声死掉。

## 2. 状态机（SessionState）

```
idle ──start_session──▶ connecting ──握手完成──▶ running
                            │                       │
                            ├─ deploy 失败 ────────▶ failed ──session.retry──▶ connecting
                            ├─ TCP 超时(10s/20次)─▶ failed ──设备掉线──▶ handle 被拔(idle)
                            └─ task 崩溃 ─────────▶ failed（墓碑）──重连──▶ 自动恢复
                            │
                            └─ stop ──▶ stopped（仅内部，handle 立即被拔）
```

| 状态 | 含义 | UI 可见 | 进入条件 | 退出条件 |
|------|------|---------|----------|----------|
| `idle` | 无 session | 否（推送只含有 session 设备） | 设备无 session | `start_session` |
| `connecting` | session 启动中（deploy + TCP 握手） | 是（"连接中"） | `start_session` | 握手完成 / 失败 / stop |
| `running` | 功能完整运行 | 是（"运行中"） | dummy byte 读成功（server 就绪） | task 崩溃 / stop |
| `failed` | 连接失败，终态 | 是（"连接失败" + 重试按钮） | deploy 失败 / TCP 超时 / task 崩溃 | `session.retry` / 设备掉线拔墓碑 |
| `stopped` | 正常停止（瞬态） | 否 | stop 信号后 task 退出 | 立即被 Core 拔除 |

- **degraded 删除**：唯一入口（deploy 失败降级 notification-only）已被"判死"决策堵死，不留死状态。
- **deploy 失败 = 判死**：不再降级 `run_notification_only`，通知同步也随之停止（行为变更，已确认）。
- **stopped 仅内部存在**：正常停止时 Core 主动 `take` handle，状态来不及推送，UI 以"设备不在 session 列表"表达停止。

## 3. 机制设计

### 3.1 状态存储与写入

- `Handle` 增加 `session_state: Arc<AtomicU8>`（0=idle, 1=connecting, 2=running, 3=failed, 4=stopped）。
- `Session` 在阶段边界写入：
  - `new()` → `connecting`
  - dummy byte 读成功（control-only 路径）或 codec header 处理完（audio 路径，含 codec_id==0 的设备禁音频）→ `running`
  - deploy 失败 / TCP 重试耗尽 / codec_id==1 → `failed`（session 自知的原因，尽早写）
- **墓碑规则（Core 权威）**：每 tick 检查 `handle.task.is_finished()`，若 handle 仍在（Core 未主动拔）且状态非 `stopped` → 强制写 `failed`。此规则兜底 panic / 其他未覆盖退出路径，不依赖 session 内部写状态。

### 3.2 墓碑（failed 保留 handle）

- 崩溃后 handle **不清理**，状态 `failed`，占着 `session` 位。
- 推送过滤不变（只推有 session 的）→ 墓碑保证 failed 对 UI 可见。
- `sync_sessions` 因 `session.is_some()` 不重启 → 墓碑本身是重启闸门，杜绝每 150ms 的崩溃重启风暴。
- 设备掉线：现有 `to_stop` 逻辑 `take + stop` 墓碑（stop 已结束的 task 无副作用）→ 重连后自动 `start_session` 恢复。
- 音频开关重启、disconnect：走现有 stop 流程，与墓碑无冲突。

### 3.3 连接超时（10s 判死）

- `connect_socket` 循环从无限改为**次数上限 20 次 × 500ms = 10s**，超时 → `server.stop()` + 写 `failed`。
- deploy 本身不计入超时（Wi-Fi 下 push 700KB 可能 5-10s），超时从 deploy 成功后起算。
- 计数与 stop 信号并行监听（现有 `select!` 结构保留）。

### 3.4 重试链路（UI 按钮）

```
UI 重试按钮（仅 failed 可见）
  → Tauri retry_session(uuid) [新 command]
  → IPC "session.retry" {uuid} [新 method]
  → Core: 拔墓碑 + start_session → connecting
```

- 复用 `device.connect` 被否决（职责是 adb 连接，与 session 无关）。
- 防连点：`start_session` 现有 guard（`session.is_some()` 不重复启动）天然生效——拔墓碑后到 spawn 完成之间原子。

## 4. IPC 变更

```rust
// SessionSummary 新增字段
pub struct SessionSummary {
    // ... 现有字段 ...
    /// session 生命周期状态: "connecting" | "running" | "failed" | "stopped"
    pub session_state: String,
}

// 新 method
// → {"id": N, "method": "session.retry", "params": {"uuid": "..."}}
// ← {"id": N, "result": {}}
```

## 5. UI 变更（App.vue）

- **双状态分开展示**（已确认）：
  - 主状态：ADB 状态（`d.state`，来自 `devices-updated`，维持现状）
  - 副状态：session 状态小字（来自 `sessions-updated` 新增字段）
- 文案映射（中文，已确认）：`connecting`→连接中，`running`→运行中，`failed`→连接失败，`stopped`→已停止。
- `sessions-updated` listener 增加 `session_state` 缓存（与 configs 同表按 uuid 存）。
- failed 时卡片显示"重试"按钮 → `retry_session(uuid)`。

## 6. 文件改动点

| 文件 | 改动 |
|------|------|
| `sync-core/src/session/handle.rs` | `Handle` 加 `session_state: Arc<AtomicU8>`（含 setter/状态常量） |
| `sync-core/src/session/runner.rs` | `Session` 加状态字段并写状态；`connect_socket` 加 20 次上限；deploy 失败路径改 `failed`，删除 `run_notification_only` 调用路径 |
| `sync-core/src/app.rs` | `Command` 加 `Retry(Uuid)`；tick 加墓碑检测（`is_finished` 强制 failed）；`push_session_list` 输出 `session_state`；`on_command` 处理 retry |
| `sync-core/src/ipc/types.rs` | `SessionSummary.session_state` |
| `sync-core/src/ipc/server.rs` | dispatch 加 `"session.retry"` |
| `ui/src-tauri/src/lib.rs` | 新 command `retry_session` |
| `ui/src/App.vue` | 副状态文字 + 重试按钮 + listener 缓存 |

## 7. 边界情况

- 重试连点 → start_session guard 防重复。
- 音频开关切换 → stop + start_session，状态重新走 connecting（预期）。
- 崩溃风暴 → failed 墓碑闸门，无自动重启；恢复靠设备重连或 UI 重试。
- panic 兜底 → Core `is_finished` 规则覆盖。
- 设备掉线 → 拔墓碑 → 重连自动恢复，无需 UI 操作。
- 推送延迟 ≤150ms（tick 轮询），对状态文字展示足够（已确认不做即时推送）。

## 8. 明确不做（本阶段）

- 不做 degraded 状态。
- 不做 adb connect 阶段（pending_serials）的状态反馈——UI 盲区维持现状。
- 不做 stopped/idle 的推送可见性。
- 不做失败自动重试（无退避机制）。

## 9. 音频缓冲延迟显示（2026-08-01 追加）

### 背景

音频延迟高。调查结论（`audio_player.rs`）：rodio 播放队列**无上限、无丢帧**，
任何一次消费慢于喂入（Windows 音频驱动 hiccup / CPU 竞争）都会让积压永久累积、永不回落。
实测：设备 V2352A 音频开启时 `audio_buffer_ms` 稳定在 ~270-290ms 平台（曾一次性累积，之后不再回落），
总听感延迟 ≈ 积压 + 基线（server buffer 50ms + 编码 20-40ms + WiFi/ADB 传输 20-50ms + WASAPI 10-30ms）≈ 400ms 级。

### 实现（方案 A：队列深度显示）

- `PcmSource` 增 `played: Arc<AtomicU64>`，`next()` 累加（被音频线程消费时自然递增）。
- `AudioPlayer` 增 `fed_samples` / `buffer_ms()` = (已喂入 − 已播放) × 1000 / 96000。
- `clear()` 将 fed 对齐到 played，避免清空后虚高。
- audio_task 每帧上报 `audio_latency`（Handle 共享 Arc），Core tick 读 → `SessionSummary.audio_buffer_ms` → IPC → UI。
- UI：音频 toggle 行旁显示“缓冲 Nms”，≥200ms 标红。
- daemon 日志每 5s 打点 `[audio] {serial} buffer: N ms`。

### 待定修复（2026-08-01 已实现：阈值丢帧）

- **已实现**：`AUDIO_BUFFER_MAX_MS = 150`，audio_task 每帧检查 `buffer_ms() > 150` → `clear()` 丢帧清空。
  - `AudioPlayer::clear()` 补 `play()`——rodio 的 `clear()` 会 pause 播放，不恢复则永久静音。
  - 实测：V2352A 上阈值触发两次（154/152ms），5s 后打点 71ms，积压回落；修复前稳定 280ms 平台且持续累积到 1s+。
  - 代价：每次清空丢失 ~20ms 音频（轻微断音）。若消费长期慢于喂入，会周期性触发（延迟保持低位，断音成为常态信号）。
- 可选后续：scrcpy server 传 `audio_buffer` 参数降低基线（默认 50ms）。
- 可选后续：PTS 时钟对齐（方案 B，工作量大）。
