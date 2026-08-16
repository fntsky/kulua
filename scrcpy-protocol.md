# scrcpy 通信协议文档

> 基于 scrcpy v3.2 参考实现分析  
> 覆盖：ADB 隧道建立、控制协议 (PC→Phone)、设备消息协议 (Phone→PC)、剪贴板同步流程

---

## 目录

1. [隧道层：ADB 转发](#1-隧道层adb-转发)
2. [连接建立流程](#2-连接建立流程)
3. [控制协议 (PC → Phone)](#3-控制协议-pc--phone)
4. [设备消息协议 (Phone → PC)](#4-设备消息协议-phone--pc)
5. [剪贴板同步详解](#5-剪贴板同步详解)
6. [完整数据流](#6-完整数据流)

---

## 1. 隧道层：ADB 转发

### 1.1 两种隧道模式

| 模式 | ADB 命令 | 适用场景 | Server 端行为 |
|------|----------|----------|--------------|
| `adb reverse` | `adb reverse localabstract:<name> tcp:<port>` | USB 连接（优先） | Server 作为 TCP **客户端**，主动连接 PC 监听的端口 |
| `adb forward` | `adb forward tcp:<port> localabstract:<name>` | TCP/IP 无线连接（降级） | Server 创建 `LocalServerSocket`，PC 作为 TCP **客户端**连接 |

> **scrcpy 优先尝试 `adb reverse`，失败后降级为 `adb forward`**（`adb_tunnel.c:124-145`）

### 1.2 Socket 命名规则

```
socket_name = "scrcpy" + "_" + format("%08x", scid)
```

- `scid` 是一个随机生成的 32 位 ID，每次运行不同
- `scid == -1` 时只用 `"scrcpy"`（简化模式）
- 通过 `adb forward tcp:<port> localabstract:<name>` 暴露到 PC

### 1.3 端口分配

- 默认端口范围 `27183:27199`（最多 16 次重试）
- `adb forward` 模式：PC 指定端口 → adb 映射到设备端的 `localabstract:<name>`
- `adb reverse` 模式：设备端 `localabstract:<name>` → 映射到 PC 本地端口
- 所有通道（video/audio/control）共用**同一个端口**，通过重复 `connect()`/`accept()` 多路复用

---

## 2. 连接建立流程

### 2.1 时序图

```
PC (scrcpy)                           Device (adb + scrcpy-server)
    │                                        │
    │  1. adb push scrcpy-server.jar         │
    │───────────────────────────────────────>│
    │                                        │
    │  2. adb forward tcp:27183              │
    │     localabstract:scrcpy_xxxxxxxx      │
    │───────────────────────────────────────>│   ← 创建 adb 隧道
    │                                        │
    │  3. adb shell CLASSPATH=...            │
    │     app_process / ... Server 4.0       │
    │───────────────────────────────────────>│   ← 启动 server
    │                                        │
    │                          ┌─────────────┘
    │                          │ LocalServerSocket.accept()
    │                          │ socket_name = "scrcpy_xxxxxxxx"
    │                          ▼
    │  ┌─────── 同一个端口 多路复用 ───────┐
    │                                        │
    │  4. TCP connect :27183                 │
    │  ─────────────────────────────────────>│  accept() → video_socket
    │  ◄─── dummy byte (0x00) ──────────────│  ← 验证隧道连通性
    │                                        │
    │  5. TCP connect :27183                 │
    │  ─────────────────────────────────────>│  accept() → audio_socket
    │                                        │
    │  6. TCP connect :27183                 │
    │  ─────────────────────────────────────>│  accept() → control_socket
    │  ◄─── 64 bytes device_name ───────────│  ← 设备信息
    │                                        │
    │  ◄──── video stream ──────────────────│  视频流 (H264/H265)
    │  ◄──── audio stream ──────────────────│  音频流 (Opus)
    │  ◄──── device messages ───────────────│  设备消息 (剪贴板等)
    │  ──── control messages ──────────────>│  控制指令
    │                                        │
```

### 2.2 Dummy Byte 验证

`tunnel_forward=true` 时，Server 为每个新接受的连接写一个 `0x00` 字节（`sendDummyByte`），目的是让 TCP **客户端可以通过 `read()` 检测到连接已建立**（解决 adb forward 隧道中 `connect()` 成功但服务端还没 accept 的问题）。

关键代码路径：
```java
// DesktopConnection.java:68-72
videoSocket = localServerSocket.accept();
if (sendDummyByte) {
    videoSocket.getOutputStream().write(0);  // 写 dummy byte
    sendDummyByte = false;
}
```

```c
// server.c:483-498 (connect_and_read_byte)
bool ok = net_connect_intr(intr, socket, tunnel_host, tunnel_port);
char byte;
if (net_recv_intr(intr, socket, &byte, 1) != 1) {
    // 读不到 dummy byte → 服务端还没就绪，重试
    return false;
}
```

### 2.3 设备信息交换

连接建立后，Server 通过**第一个 Socket**（video/audio/control 中优先级最高的）发送设备名称：

- 固定 64 字节 UTF-8 缓冲区
- 不足部分填 `\0`
- Field `SC_DEVICE_NAME_FIELD_LENGTH = 64`

```c
// server.c:589-601
uint8_t buf[64];
net_recv_all_intr(intr, device_socket, buf, sizeof(buf));
buf[63] = '\0';
info->device_name = (char *)buf;
```

---

## 3. 控制协议 (PC → Phone)

### 3.1 协议概览

- **方向**：PC → Phone（通过 `control_socket`）
- **传输**：TCP（同一端口，通过 `adb forward` 隧道）
- **最大消息大小**：`256 KiB`（`SC_CONTROL_MSG_MAX_SIZE = 1 << 18`）
- **Nagle 算法**：对 control socket 禁用（`TCP_NODELAY`）

### 3.2 消息格式通用结构

```
Offset  Size  Field
─────── ───── ─────────────────────────────────
 0       1    type      — 消息类型 (enum)
 1       N    payload   — 消息体，格式由 type 决定
```

### 3.3 消息类型列表

| type | 名称 | Payload 大小 | 用途 |
|------|------|-------------|------|
| `0x00` | `INJECT_KEYCODE` | 13 | 注入按键事件 |
| `0x01` | `INJECT_TEXT` | 可变 | 注入文本 |
| `0x02` | `INJECT_TOUCH_EVENT` | 31 | 注入触摸事件 |
| `0x03` | `INJECT_SCROLL_EVENT` | 20 | 注入滚动事件 |
| `0x04` | `BACK_OR_SCREEN_ON` | 1 | 返回键 / 亮屏 |
| `0x05` | `EXPAND_NOTIFICATION_PANEL` | 0 | 展开通知栏 |
| `0x06` | `EXPAND_SETTINGS_PANEL` | 0 | 展开快捷设置 |
| `0x07` | `COLLAPSE_PANELS` | 0 | 收起面板 |
| `0x08` | `GET_CLIPBOARD` | 1 | 请求读取设备剪贴板 |
| `0x09` | **`SET_CLIPBOARD`** | 14+N | **设置设备剪贴板** |
| `0x0A` | `SET_DISPLAY_POWER` | 1 | 设置屏幕电源 |
| `0x0B` | `ROTATE_DEVICE` | 0 | 旋转屏幕 |
| `0x0C` | `UHID_CREATE` | 可变 | 创建 UHID 设备 |
| `0x0D` | `UHID_INPUT` | 可变 | UHID 输入 |
| `0x0E` | `UHID_DESTROY` | 2 | 销毁 UHID 设备 |
| `0x0F` | `OPEN_HARD_KEYBOARD_SETTINGS` | 0 | 打开实体键盘设置 |
| `0x10` | `START_APP` | 可变 | 启动应用 |
| `0x11` | `RESET_VIDEO` | 0 | 重置视频编码 |
| `0x12` | `CAMERA_SET_TORCH` | 1 | 设置手电筒 |
| `0x13` | `CAMERA_ZOOM_IN` | 0 | 相机变焦+ |
| `0x14` | `CAMERA_ZOOM_OUT` | 0 | 相机变焦- |
| `0x15` | `RESIZE_DISPLAY` | 4 | 调整显示尺寸 |

### 3.4 SET_CLIPBOARD 消息格式 (type=0x09)

**这是剪贴板 PC→Phone 方向的消息。**

```
Byte 0:  type            = 0x09
Byte 1-8:  sequence       = uint64 (大端序)
Byte 9:    paste           = 0 (不自动粘贴) / 1 (自动粘贴)
Byte 10-13: text_length    = uint32 (大端序), 文本 UTF-8 字节数
Byte 14+:  text            = UTF-8 文本内容 (不含尾随 \0)
```

- `sequence`：序列号，用于 ACK 确认（`0` 表示不需要确认）
- `paste`：Android >= 7 时，为 `1` 则注入 PASTE 键事件
- `text_length` 最大值限制：`256 KiB - 14 = 262,144 B`
- Java 端解析 (ControlMessageReader.java:138-143):
  ```java
  case TYPE_SET_CLIPBOARD:
      long sequence = dis.readLong();
      boolean paste = dis.readByte() != 0;
      String text = parseString(); // readInt(length) + readFully(bytes)
      return ControlMessage.createSetClipboard(sequence, text, paste);
  ```

### 3.5 GET_CLIPBOARD 消息格式 (type=0x08)

```
Byte 0:  type     = 0x08
Byte 1:   copy_key = 0 (不操作) / 1 (模拟 COPY 键) / 2 (模拟 CUT 键)
```

- `copy_key`: Android >= 7 时，先注入 COPY/CUT 按键事件再读取剪贴板
- 服务器端处理 (Controller.java:682-700):
  1. 如果 `copy_key` 非 0 → 注入 COPY/CUT 键（等待完成）
  2. 如果 `clipboardAutosync` 为 false → 主动读取剪贴板并发送回 PC
  3. 如果 `clipboardAutosync` 为 true → **不发送**（因为监听器会自动同步）

---

## 4. 设备消息协议 (Phone → PC)

### 4.1 协议概览

- **方向**：Phone → PC（通过 `control_socket` 的相反方向）
- **传输**：同一 TCP 连接双向复用
- **最大消息大小**：`256 KiB`（`DEVICE_MSG_MAX_SIZE = 1 << 18`）
- 服务端使用独立的 `DeviceMessageSender` 线程发送，`ControlMessageReader` 线程接收

### 4.2 消息通用结构

```
Offset  Size  Field
─────── ───── ─────────────────────────────────
 0       1    type      — 消息类型 (enum)
 1       N    payload   — 消息体
```

### 4.3 消息类型列表

| type | 名称 | Payload 大小 | 用途 |
|------|------|-------------|------|
| `0x00` | **`TYPE_CLIPBOARD`** | 4+N | 设备剪贴板文本 |
| `0x01` | `TYPE_ACK_CLIPBOARD` | 8 | 剪贴板设置确认 |
| `0x02` | `TYPE_UHID_OUTPUT` | 4+N | UHID 设备输出 |

### 4.4 TYPE_CLIPBOARD 消息格式 (type=0x00)

```
Byte 0:  type          = 0x00
Byte 1-4: text_length   = uint32 (大端序), 文本 UTF-8 字节数
Byte 5+:  text          = UTF-8 文本内容 (不含尾随 \0)
```

- `text_length` 最大值限制：`256 KiB - 5 = 262,139 B`
- Java 端序列化 (DeviceMessageWriter.java:27-31):
  ```java
  case TYPE_CLIPBOARD:
      byte[] raw = text.getBytes(StandardCharsets.UTF_8);
      int len = StringUtils.getUtf8TruncationIndex(raw, CLIPBOARD_TEXT_MAX_LENGTH);
      dos.writeInt(len);
      dos.write(raw, 0, len);
  ```
- C 端反序列化 (device_msg.c:19-39): 读 1 byte type → 4 bytes length → N bytes text

### 4.5 TYPE_ACK_CLIPBOARD 消息格式 (type=0x01)

```
Byte 0:  type     = 0x01
Byte 1-8: sequence = uint64 (大端序)
```

- 服务端执行 `setClipboard()` 后，如果请求的 `sequence != 0`，则回复 ACK
- PC 端收到 ACK 后可以执行依赖剪贴板的后续操作（如 Ctrl+V）

---

## 5. 剪贴板同步详解

### 5.1 架构概览

```
 ┌─────────────────────────────────────────┐
 │               PC (scrcpy)               │
 │                                         │
 │  ┌──────────────────────────────────┐   │
 │  │      SDL 系统剪贴板               │   │
 │  └──────┬──────────────▲────────────┘   │
 │         │              │                │
 │    clipboard      get_clipboard         │
 │    changed         event from           │
 │    detected        device               │
 │    (Ctrl+V /                            │
 │     autosync)        ┌──────────────────┤
 │         │            │  receiver thread │
 │         ▼            │  (读 device msgs)│
 │  ┌─────────────────┐ │                  │
 │  │ controller_push │ │  control socket  │
 │  │ (写控制消息)     │ │  (双向复用)      │
 │  └────────┬────────┘ │                  │
 └───────────┼──────────┴──────────────────┘
             │ adb forward / reverse tunnel
┌────────────┼─────────────────────────────────┐
│  Device    │                                  │
│            ▼                                  │
│  ┌─────────────────┐   ┌───────────────────┐  │
│  │  ControlChannel  │   │ LocalServerSocket │  │
│  │  (双向)          │   │ scrcpy_xxxxxxxx   │  │
│  └───┬─────────┬────┘   └───────────────────┘  │
│      │         │                                │
│      ▼         ▼                                │
│  recv thread  send thread                       │
│  (读控制消息)  (发设备消息)                      │
│      │         │                                │
│      │    ┌────┴──────────┐                    │
│      │    │ DeviceMsgQueue │                    │
│      │    └───────────────┘                    │
│      ▼                                         │
│  Controller.handleEvent()                      │
│      │                                         │
│      ├── TYPE_SET_CLIPBOARD → setClipboard()   │
│      ├── TYPE_GET_CLIPBOARD → getClipboard()   │
│      └── ...                                   │
│                                                │
│  ClipboardManager (系统监听器)                  │
│  当设备剪贴板变化时 → sender.send(CLIPBOARD)   │
│                                                │
└────────────────────────────────────────────────┘
```

### 5.2 Phone → PC 流程 (clipboard_autosync)

```
步骤 1: 用户在手机上复制文本
步骤 2: Android 系统回调 ClipboardManager.OnPrimaryClipChangedListener
         (Controller.java:146-157)
步骤 3: 检查 isSettingClipboard 标志 → 如果是自身写入则不触发
步骤 4: Device.getClipboardText() 读取设备剪贴板
步骤 5: 构造 DeviceMessage.TYPE_CLIPBOARD → sender.send(msg)
步骤 6: DeviceMessageSender 线程从 BlockingQueue 取出 → 写入 control socket
         → adb 隧道 → PC
步骤 7: PC 端 Receiver 线程 (receiver.c:184-220) 读取 TCP 数据
步骤 8: process_msg() → task_set_clipboard() (投递到主线程)
步骤 9: SDL_GetClipboardText() 检查是否与当前相同
         → 不同则 SDL_SetClipboardText() 写入系统剪贴板
```

**关键代码 (Controller.java:146-157)**：
```java
clipboardManager.addPrimaryClipChangedListener(() -> {
    if (isSettingClipboard.get()) {
        // 这是自身写入触发的通知，忽略
        return;
    }
    String text = Device.getClipboardText();
    if (text != null) {
        DeviceMessage msg = DeviceMessage.createClipboard(text);
        sender.send(msg);
    }
});
```

**PC 端防重复写入 (receiver.c:46-66)**：
```c
static void task_set_clipboard(void *userdata) {
    char *text = userdata;
    char *current = SDL_GetClipboardText();
    bool same = current && !strcmp(current, text);
    SDL_free(current);
    if (same) {
        LOGD("Computer clipboard unchanged");  // ← 跳过写入
    } else {
        SDL_SetClipboardText(text);
    }
    free(text);
}
```

### 5.3 PC → Phone 流程 (clipboard_autosync)

**触发条件：用户在 PC 上按 Ctrl+V（或通过 scrcpy 快捷键主动发送）**

```
步骤 1: PC 键盘事件 → input_manager.c:682-706
步骤 2: 检测到 Ctrl+V
步骤 3: SDL_GetClipboardText() 读取 PC 剪贴板
步骤 4: 构造 SC_CONTROL_MSG_TYPE_SET_CLIPBOARD 消息
步骤 5: sc_controller_push_msg() → 送入 controller 队列
步骤 6: controller 线程 (run_controller) 取出消息
步骤 7: process_msg() → sc_control_msg_serialize() 序列化
步骤 8: net_send_all() → control socket → adb 隧道 → 设备
步骤 9: 设备端 ControlMessageReader 解析出 TYPE_SET_CLIPBOARD
步骤 10: Controller.setClipboard(text, paste, sequence)
         (Controller.java:702-722)
步骤 11: isSettingClipboard.set(true)
步骤 12: Device.setClipboardText(text) 写入 Android 剪贴板
步骤 13: isSettingClipboard.set(false)
步骤 14: 如果 paste=true → 注入 KEYCODE_PASTE
步骤 15: 如果 sequence != 0 → 发送 ACK_CLIPBOARD 回复
```

**防回环机制**：

```
PC 复制 "hello"  →  发送 TYPE_SET_CLIPBOARD  →  设备 setClipboard
                                                      │
                                          isSettingClipboard = true
                                                      │
                                                  写入成功
                                                      │
                                          isSettingClipboard = false
                                                      │
                                                   触发
                                          PrimaryClipChangedListener
                                                      │
                                          isSettingClipboard = true
                                                    跳过，不发送回 PC
```

**PC 端 Autosync + Ctrl+V 完整流程**（`input_manager.c:682-706`）：

```c
uint64_t sequence = ...;
if (clipboard_autosync && is_ctrl_v) {
    // 1. 将 PC 剪贴板同步到设备
    set_device_clipboard(im, false, sequence);  // paste=false
    // 2. 等待 ACK (如果 async_paste)
    ack_to_wait = sequence;
    // 3. 然后注入 Ctrl+V
}
```

### 5.4 PC→Phone 防回环总结

scrcpy 用了**双层**防回环：

| 层级 | 位置 | 机制 | 说明 |
|------|------|------|------|
| **设备端** | `Controller.isSettingClipboard` | AtomicBoolean 标志 | `setClipboard()` 执行期间忽略 `PrimaryClipChangedListener` |
| **PC 端** | `Receiver.task_set_clipboard` | 比较系统剪贴板 | 收到设备剪贴板后，与 PC 当前剪贴板比较，相同则不写 |

---

## 6. 完整数据流

### 6.1 PC 启动到运行概览

```
1. scrcpy 初始化 SDL、解析参数
2. 选择 ADB 设备 (serial/USB/TCPIP)
3. adb push scrcpy-server.jar /data/local/tmp/
4. adb forward (或 reverse) 建立隧道
5. adb shell app_process 启动 Server
6. Server 创建 LocalServerSocket → 等待 accept()
7. PC 端连接 socket → 读取 dummy byte → 读取设备名称
8. PC 端再连接 socket → 控制通道 ← 此后双向通信
9. Server 发送视频流 (PC 端解码渲染) + 设备消息 (PC 端处理)
10. PC 端发送控制指令 → 设备端执行
11. 剪贴板自动同步持续运行
```

### 6.2 剪贴板消息编码示例

**PC→Phone SET_CLIPBOARD (type=0x09)**:

```
设: sequence=42, text="hello", paste=false

byte[0]  = 0x09                           // type
byte[1]  = 0x00, 0x00, 0x00, 0x00,        // sequence (64-bit big-endian)
          0x00, 0x00, 0x00, 0x2A
byte[9]  = 0x00                            // paste = false
byte[10] = 0x00, 0x00, 0x00, 0x05          // text_length = 5
byte[14] = 0x68, 0x65, 0x6C, 0x6C, 0x6F   // "hello"
```

**Phone→PC TYPE_CLIPBOARD (type=0x00)**:

```
设: text="你好"

byte[0]  = 0x00                            // type
byte[1]  = 0x00, 0x00, 0x00, 0x06          // text_length = 6
byte[5]  = 0xE4, 0xBD, 0xA0, 0xE5,        // "你好" (UTF-8)
          0xA5, 0xBD
```

---

## 参考代码路径

| 组件 | C 端 (scrcpy) | Java 端 (scrcpy-server) |
|------|---------------|------------------------|
| 隧道管理 | `adb_tunnel.c` | `DesktopConnection.java` |
| 控制消息序列化 | `control_msg.c` | `ControlMessageReader.java` |
| 控制消息定义 | `control_msg.h` | `ControlMessage.java` |
| 设备消息序列化 | `device_msg.c` | `DeviceMessageWriter.java` |
| 设备消息定义 | `device_msg.h` | `DeviceMessage.java` |
| 控制收发 | `controller.c` | `Controller.java` |
| 设备消息接收 | `receiver.c` | `ControlChannel.java` |
| 设备消息发送 (服务端) | — | `DeviceMessageSender.java` |
| 剪贴板快捷键 | `input_manager.c` | — |
| 服务端启动 | `server.c` | `Server.java` |
