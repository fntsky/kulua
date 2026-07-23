# Session 实现 vs 官方 scrcpy — 协议差异分析

> 对比基准: `E:/code/ref/scrcpy` (官方仓库)
> 你的实现: `sync-core/src/session.rs`
> 最后更新: 2026-07-23

---

## 1. 协议层缺失 (最高严重度)

### 1.1 codec ID 头

| | 官方 | 你的实现 |
|---|---|---|
| 音频流开头 | 4-byte big-endian codec ID (`writeAudioHeader`) | 跳过 (`send_stream_meta=false`) |
| 视频流开头 | 4-byte big-endian codec ID (`writeVideoHeader`) | 不涉及 (`video=false`) |

`send_stream_meta=false` 导致 server 端不发 codec ID。你无法确认设备端实际用的编码器 (OPUS/AAC/RAW)，只能从启动参数猜测。

### 1.2 帧头格式

官方每一帧都带 **12-byte header** (Streamer.java):

```
[0-7]   PTS (8 bytes, big-endian) + 标志位
        bit 63: PACKET_FLAG_SESSION
        bit 62: PACKET_FLAG_CONFIG
        bit 61: PACKET_FLAG_KEY_FRAME
[8-11]  packet size (4 bytes, big-endian)
```

你的实现传了 `send_frame_meta=false`，但 reader 仍尝试解析帧结构：

| | 官方 | 你的实现 |
|---|---|---|
| 帧头大小 | 12 bytes (8+4) | 自定义格式 (8+4) |
| size 字节序 | big-endian | **little-endian** (错误) |
| PTS 标志位 | 高位标记 config/keyframe | 无标志位处理 |
| Session 元数据 | `writeSessionMeta` (width/height/flags) | 无 |

**后果**: 双方协议不一致，size 解码错误会导致帧边界错位。

### 1.3 stream disable 信号

官方协议中 codec_id=0 表示"流被设备端显式禁用" (如音频无法捕获)，codec_id=1 表示"配置错误"。你的实现在 `send_stream_meta=false` 下永远收不到这些信号。

---

## 2. 连接架构 (严重)

### 2.1 3-socket vs 1-socket

```
官方 (DesktopConnection.java)            你的实现
┌──────────────────────────┐            ┌─────────────┐
│ video LocalSocket (fd)   │ 独立线程   │ TCP:port    │
│ audio LocalSocket (fd)   │ 独立线程   │ 单连接      │
│ control LocalSocket (fd) │ 独立线程   │ 音频+控制   │
└──────────────────────────┘            └─────────────┘
```

官方用 ADB tunnel 连接 3 次同一个 TCP 端口，server 端 accept 3 次。你只连接 1 次。

### 2.2 关键后果: 音频启用时服务端死锁

```java
// DesktopConnection.java 在 tunnel_forward 模式下
if (audio) {
    audioSocket = localServerSocket.accept(); // 你的1次连接被这里接受
    audioSocket.getOutputStream().write(0);   // 发 dummy byte
}
if (control) {
    controlSocket = localServerSocket.accept(); // ⚠️ 死锁！没人连第二次
    ...
}
```

- `audio_enabled=false` 时只连 1 次控制通道 → 碰巧能工作
- `audio_enabled=true` 时 server 需要 2 个 accept → **永远阻塞在 control accept** → `scrcpy()` 方法永远不会执行到实际的音频编码/发送逻辑

**也就是说，当前音频功能实际上不工作。**

---

## 3. 消息处理 (中)

### 3.1 盲吞未知消息

```rust
// session.rs:275-277
_ => {
    let mut dump = [0u8; 8192];
    if reader.read(&mut dump).await.unwrap_or(0) == 0 { break; }
}
```

官方协议是确定性的，遇到未知 type 直接 LOGW + break。盲吞 8192 字节：
- 如果消息 > 8192 → 截断，后续解析永久错位
- 可能吃掉下一条消息的开头

### 3.2 缺少的消息类型

| 消息类型 | type | 状态 |
|---|---|---|
| TYPE_CLIPBOARD | 0x00 | 已处理 (不全) |
| TYPE_ACK_CLIPBOARD | 0x01 | **缺失** |
| TYPE_UHID_OUTPUT | 0x02 | **缺失** |

clipboard 设置流程缺少 sequence number 确认机制。

### 3.3 PC→Device 剪贴板缺少 paste 标记处理

官方 `set_clipboard` 消息:

```
[0]    type: 0x09
[1-8]  sequence: uint64 BE (固定0)
[9]    paste: 0=不自动粘贴, 1=自动粘贴
[10-13] text length: uint32 BE
[14+]  UTF-8 text
```

你的 `build_clipboard_frame` 固定 paste=0，但 server 返回的 ACK_CLIPBOARD 不会处理。

---

## 4. 清理机制 (中)

| | 官方 | 你的实现 |
|---|---|---|
| 步骤 | `shutdownInput/Output` → `AsyncProcessor.stop()` → `processor.join()` → watchdog 1s → `process_terminate()` | `kill -9 $(ps grep ... \| awk ...)` + `process.kill()` + `adb forward --remove` |
| 进程监控 | `sc_process_observer` 监听终止，中断 accept/read | 无 (不感知 server 异常退出) |
| 超时 | 1秒 watchdog | 无 |

`kill -9` 依赖 `ps` 命令格式，不同 Android 版本/厂商输出可能不一致。不给进程任何清理机会。

---

## 5. 参数配置 (低)

| 参数 | 官方默认值 | 你的固定值 | 影响 |
|---|---|---|---|
| `tunnel_forward` | false | true | 占用本地 TCP 端口 vs reverse tunnel 不占端口 |
| `send_device_meta` | true | true | 一致 |
| `send_dummy_byte` | true | true | 一致 |
| `send_frame_meta` | true | **false** | 导致#1.2 问题 |
| `send_stream_meta` | true | **false** | 导致#1.1 和#1.3 问题 |
| `scid` | 随机 31-bit | 固定 | 多实例冲突风险 |
| `video` | true | **false** | 设计意图，可以接受 |

---

## 6. 修复优先级

| 优先级 | 问题 | 原因 |
|---|---|---|
| P0 | #2.2 双连接死锁 | 音频启用时 server 根本跑不起来 |
| P0 | #1.2 帧头字节序错误 | 数据损坏，目前音频数据全是错的 |
| P0 | #1.1 缺少 codec ID | 无法确认编码类型 |
| P1 | #1.2 缺少 PTS 标志位 | 无法区分 config/keyframe 包 |
| P1 | #3.1 盲吞未知消息 | 协议不可恢复 |
| P1 | #4 清理和进程监控 | 优雅退出、异常感知 |
| P2 | #3.2 缺少 ACK_CLIPBOARD 和 UHID_OUTPUT | 未来兼容性 |
| P2 | #5 使用 scid | 多实例共存 |

---

## 7. 关闭流程 (Stop / Cleanup)

### 7.1 官方 client 侧 (`app/src/server.c`)

```c
void sc_server_stop(struct sc_server *server) {
    sc_mutex_lock(&server->mutex);
    server->stopped = true;            // (1) 设标志
    sc_cond_signal(&server->cond_stopped);  // (2) 唤醒 run_server 线程
    sc_intr_interrupt(&server->intr);       // (3) 中断 ADB 操作
    sc_mutex_unlock(&server->mutex);
}
```

`run_server` 线程被 condvar 唤醒后:

```
(4) cond_wait 返回 → 持有 mutex
(5) net_interrupt(video_socket)   ← ┐
(6) net_interrupt(audio_socket)   ← ├── shutdown(SHUT_RDWR)
(7) net_interrupt(control_socket) ← ┘    每个 socket
(8) 1s 看门狗 (sc_process_observer_timedwait)
(9) 超时未退 → sc_process_terminate(pid)  ← Unix: kill(SIGKILL), Win: TerminateProcess
(10) sc_process_observer_join / destroy
(11) sc_process_close(pid)               ← waitpid() 回收
(12) (可选) sc_adb_kill_server()
```

### 7.2 官方 server 侧 (Java `Server.java`)

服务端的主线程阻塞在 `Looper.loop()`:

```java
// Server.scrcpy():
Looper.loop();  // ← 阻塞, 靠 Looper.quitSafely() 退出
```

各 `AsyncProcessor` 都在设备 socket 上阻塞:
- `SurfaceEncoder` → `streamer.writePacket()` → `IO.writeFully(fd, buffer)` → 写入 FileDescriptor
- `AudioEncoder` → 同上
- `Controller` → `ControlChannel.recv()` → 读取 control socket

关闭链:

```
Client net_interrupt(socket)
     ↓
Server 端 LocalSocket 的 FileDescriptor 收到 EOF / Broken pipe
     ↓
IO.writeFully() 抛出 IOException
     ↓
AsyncProcessor 线程退出 → listener.onTerminated(fatalError)
     ↓
Completion.addCompleted() → running 归零
     ↓
Looper.getMainLooper().quitSafely()
     ↓
finally 块执行:
  1. cleanUp.interrupt()           ← 清理线程退出
  2. for each processor: stop()   ← 设 stopped=true
  3. connection.shutdown()        ← shutdownInput/Output 三个 socket
  4. cleanUp.join()               ← 等清理线程
  5. processor.join()              ← 等所有线程
  6. OpenGLRunner.shutdown()
  7. connection.close()           ← close() 三个 socket
```

### 7.3 你的实现 (`Handle::stop()`)

```rust
// Handle::stop()
pub async fn stop(&mut self, adb: &dyn AdbOps) {
    if let Some(tx) = self.stop_tx.take() {
        let _ = tx.send(());                    // (1) 发停止信号
    }
    // 3s 超时等 task 退出
    if tokio::time::timeout(3s, &mut self.task).await.is_err() {
        self.task.abort();                      // (2) abort tokio task
        // 远程: adb shell kill -9 $(ps | grep ...)   (3)
        // 本地: process.kill() + wait()              (4)
        // adb forward --remove                        (5)
    }
}
```

### 7.4 差距分析

| 阶段 | 官方 | 你 | 后果 |
|---|---|---|---|
| **Socket 断连** | `net_interrupt()` 每个 socket → server 端 IO 立刻失败 | **无** — 仅发 `stop_tx` 信号 | server 不知道连接断开，继续在 native MediaCodec 或 writePacket 上 block |
| **Server 退出路径** | 走 Java finally → `processor.stop()` / `shutdown()` / `close()` / jar cleanup | **不走 finally** — 被 kill -9 直接毙掉 | jar 残留设备、socket 不关闭、清理线程被截断 |
| **等待策略** | 1s watchdog（socket interrupt 后 server 秒退） | 3s timeout（无 interrupt，server 在 native 调用里，等 OS 回收） | 慢 3 倍，且即使等到也是硬杀 |
| **进程终止** | `kill(SIGKILL)` / `TerminateProcess` | 同左 + `adb shell kill -9 $(ps ...)` | 多一次 adb shell 开销，ps 格式依赖厂商 |
| **observer** | `sc_process_observer` 监控 server 进程意外死亡 → 中断 accept/read | 无 | server 意外退出不被感知 |

### 7.5 修复要点

正确的关闭顺序应该是:

```
(1) stop_tx.send(())           ← 让 session 主循环退出
(2) 关闭 control socket         ← shutdown(SHUT_RDWR)
(3) 关闭 audio socket           ← shutdown(SHUT_RDWR)
(4) 等待 task 退出 (短超时 1-2s)  ← server 收到 IO 错误后应快退
(5) 没退出 → process.kill()     ← 最后手段
(6) adb forward --remove
```

核心改动: **在发 stop 信号后立即 shutdown 两路 TCP socket**，这样 server 端的 `Streamer.writePacket()` 和 `ControlChannel.recv()` 会立刻因 `BrokenPipeException` / `IOException` 退出，走 Java 的 finally 清理路径。
