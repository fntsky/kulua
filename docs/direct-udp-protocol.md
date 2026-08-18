# Kulua Direct UDP Protocol（kulua-server ↔ daemon / fusion-viewer）

> 目标：把 kulua-server（phone 端 Java）↔ daemon / fusion-viewer（PC 端 Rust）的
> 直接通信从「adb forward + localabstract（TCP scrcpy 风格字节协议）」彻底重写为
> **phone 在 Wi-Fi 网络端口直连**：传输 = 纯 UDP 数据报，协议 = protobuf（`.proto`
> 为唯一事实来源，protoc 生成 Java 与 Rust），可靠性 = 轻量（control 可靠、
> media 容忍丢包 + 分片）。
>
> 范围：control / audio / video 全部走端口直连，`adb forward` 不再使用。
> 不涉及 GUI ↔ daemon 的本地 IPC（`sync-core/src/ipc/` 维持不变）。

## 为什么不用 UDP 之外的东西 / 关键决策

| 决策 | 原因 |
|------|------|
| 传输 = 纯 UDP | 用户明确要求；phone 端 `app_process` 环境无法跑成熟 QUIC（需 native/TLS，违反单文件 jar 约束） |
| 协议 = protobuf | 用户已装 protoc（Windows PATH `protoc.exe`）；`.proto` 一份定义，protoc 生成 Java + prost 生成 Rust |
| control 可靠 | 剪贴板/输入注入/启动应用必须不丢、有序；采用**滑动窗口 + 累积 ACK + 超时重传** |
| audio/video 容忍丢包 | 媒体对丢帧容忍（视频靠关键帧重同步、音频 20ms 帧丢失可掩蔽）；乱序/重复直接丢弃 |
| 分片 | Wi-Fi 安全 UDP payload ≈ 1200B；视频关键帧 / raw PCM 音频帧远超此值，需要按 datagram 分片 + 重装 |
| 单 socket 多流 | 一个 UDP 端口承载 control/audio/video 全部流（协议内 `stream` 字段区分），复用现有 session 端口号 |
| phone 绑定 0.0.0.0:port | shell 用户可 bind UDP；PC 端用 phone 的 Wi-Fi IP（来自 `adb connect` serial 或 `ip route` 解析）直连 |

## 数据报格式

**每个 UDP datagram = 一个 protobuf `Frame`**（无额外二进制帧头；datagram 边界即消息边界）。
`Frame` 全字段定义见 `proto/direct.proto`，要点：

```
message Frame {
  Stream stream     // CTRL / AUDIO / VIDEO
  Type   type       // DATA / ACK / HELLO / HELLO_ACK / HEARTBEAT / BYE
  uint32 seq        // 每流单调递增（control 用于 ACK/去重，media 用于排序）
  uint32 msg_id     // 分片归属（同一消息的所有分片共享）
  uint32 frag       // 分片索引（0 起）
  uint32 frag_total // 总分片数（0 = 单包完整）
  uint64 media_pts     // 仅 media DATA 首分片有效：PTS(μs)
  uint32 media_flags   // 仅 media DATA 首分片有效：bit0=config bit1=keyframe bit2=session
  oneof payload {
    bytes  data        // media DATA 分片负载（或 control 原字节，见下）
    CtrlMsg ctrl       // control 消息（可靠 DATA）
    uint32 ack_seq     // ACK：确认到该序号（control 累积）
    Hello  hello       // PC→phone 会话打开
    HelloAck hello_ack // phone→PC
  }
}
```

`CtrlMsg` 覆盖所有 control 命令（`inject_keycode/text/touch/scroll`、
`back_or_screen_on`、`set_clipboard`、`start_app`、`resize_display`、
`create/destroy_display`、`clipboard_changed`、`ack_clipboard`、
`display_ready`、`audio_ready`）。字段沿用原字节协议的语义（displayId 前缀、
pointerId、压力定点等均以原生类型表达，Java/Rust 侧转换职责分离：Rust 生成
protobuf，Java 消费 protobuf 转 Android 注入）。

## 会话模型

UDP 无连接概念，phone 端按 **源地址（IP:port）解复用**客户端：

```
phone (0.0.0.0:port, 单 DatagramSocket)
  └─ receive 循环 → 按 src addr 路由到 ClientConnection
       ├─ daemon 客户端：control(双向) + audio(推流)
       └─ 每个 fusion-viewer 客户端：control(双向) + video(含各自虚拟显示器)
```

### 握手

1. PC（session 或 viewer）把 socket bind 到随机本地端口，向 `phone_ip:port` 发
   `HELLO{scid}`，每 500ms 重发（最多 20 次 → 判死）。
2. phone 校验 `scid` 与自身一致后回 `HELLO_ACK`（附带 `audio_codec`
   = 0 关闭 / 4B codec id），并在本客户端会话内注册该源地址。
3. 后续文件：daemon 发 control 信息直接走 `Frame{stream=CTRL, type=DATA, ctrl=...}`；
   audio 编码协商：`AudioReady.codec_id` 经 HELLO_ACK 携带（无需二次握手）。
4. viewer 创建显示器：发 `CtrlMsg.create_display{width,height,dpi}`（可靠），
   phone 创建 VirtualDisplay + 编码器，回 `CtrlMsg.display_ready{display_id, codec_id, ...}`。

### 生命周期与保活

- phone 侧：某客户端会话 **60s 未收到其任何 datagram → 拆除**该客户端
  （销毁其虚拟显示器/音频）；收到 `BYE` 立即拆除。
- PC 侧：session/viewer 每 5s 发 `HEARTBEAT`（control 流、可靠），并处理
  `HELLO_ACK` 后的超时：媒体流 / control 长期无响应 → 判断 server 不在线 →
  走 scid kill（沿用 `kill_by_scid`）+ 标记 failed。
- 会话停止：PC 发 `BYE` + `kill_by_scid`（既有清理路径不变）。

## 可靠性（control 流）

- 每方向一个单调 `seq`，窗口 = 16（N）：
  - 发送：`next_seq` 递增；未确认帧缓存于滑动窗口；收到 `ACK{ack_seq}` 后
    `ack_seq` 之前全部释放。
  - 接收：`expected_seq` 连续性检测；乱序帧缓冲至就绪后按序投递；重复丢。
  - 重传：首包重传定时器 ~200ms，指数退避至 1.6s，丢包连续 8 次 → 判连接失效。
- `ACK` 本身不占 seq、不重传（累积确认天然抗丢）。
- 分片消息：全部收齐（continuity of frag 0..frag_total-1）并 `msg_id` 匹配，
  且所有分片 seq 连续才投递；任一缺失 → 整条丢弃（control 由重传兜底，
  media 由解码器重同步兜底）。

## 媒体流策略

- **audio**：`Frame{stream=AUDIO, type=DATA, seq, frag..., media_pts, media_flags, data}`
  - config 包（AAC AudioSpecificConfig / FLAC STREAMINFO）：**可靠发送**（走 control
    语义或 media 首帧重复多次）——丢失无法初始化解码器。
  - 普通帧：不可靠；接收端按 `seq` 丢弃乱序/重复（20ms 帧丢失可接受）。
- **video**：`Frame{stream=VIDEO, type=DATA, ...}`（首分片带 media_flags）
  - config(SPS/PPS) 可靠发送；关键帧尽量可靠（不可靠时丢帧等下一个关键帧）。
  - 分片：任一 fragment 丢失 → 整个视频帧丢弃（解码器下个关键帧重同步）。

> 实现简化：config 帧通过 control 流承载一个专用 `CtrlMsg.media_config`
> （或复用 media DATA + 强制重传），本设计文档以「config 帧可靠发送」为准，
> 具体打包方式见 `.proto` 与代码注释。

## 端口 / 地址 / 发现

- phone 绑定 UDP `0.0.0.0:<session_port>`，`session_port` 沿用 `Core::run` 的
  `port_counter`（27183 起，融合窗口区间 27200:27299 的约定不再适用，因为
  不再有 adb forward 端口）。
- PC 侧目标 IP 解析优先级：
  1. `device.serial` 含 `ip:port` → 取 IP（adb wireless 主路径）。
  2. USB/其他：`adb -s <serial> shell ip route` 解析默认路由 `src <ip>`。
  3. 失败 → 会话失败，提示需 Wi-Fi 直连。

## 模块布局

```
proto/direct.proto                    # 协议唯一事实来源
kulua-proto/                          # 新 crate：protobuf 生成 + UDP 传输层客户端
  ├─ Cargo.toml                       # prost / prost-build
  ├─ build.rs                         # .proto → Rust（OUT_DIR）
  └─ src/
     ├─ lib.rs                        # pub mod generated / client / framing
     ├─ generated.rs                  # (build.rs 生成, include!)
     ├─ codec.rs                      # Frame 编解码便捷函数 / 分片 / 重装
     ├─ reliable.rs                   # 滑动窗口 + ACK + 重传（Rust 侧）
     └─ session.rs                    # UdpSession：握手 / 心跳 / 会话对象（供 daemon & viewer 复用）
sync-core/src/session/runner.rs       # 改用 kulua_proto session（control+audio）
sync-core/src/scrcpy.rs               # 去掉 adb forward；新增端口直连部署参数（udp_port）
sync-core/src/fusion.rs               # build_args 传 ip:port
fusion-viewer/src/server.rs / video.rs# 改用 kulua_proto（video + control 注入）
kulua-server/src/com/kulua/server/    # Java 侧重写为 UDP server（下述）
```

## Java（kulua-server）侧结构

```
UdpServer          # DatagramSocket + receive 循环 + 按 src 解复用 ClientConnection
ClientConnection   # 每客户端：scid / seq-ack 状态 / Displays / AudioCapture
ControlChannel     # 解析 CtrlMsg → Device 注入 / Clipboard 推送（逻辑移植，改 UDP 发送）
ReliableSender     # control 可靠发送（窗口 + 重传，Java 单线程足够：control 低速率）
AudioCapture       # MediaCodec 编码 → Frame(AUDIO,data) 推流（复用反射 loopback 逻辑）
DisplayEncoder     # MediaCodec 编码 → Frame(VIDEO,data) 分片推流
DisplayRegistry    # 不变（内部 id ↔ VirtualDisplay / encoder）
```

- 适配 `build.ps1`：protoc 生成 Java（`--java_out=lite`）→ 与 `protobuf-javalite`
  jar 一起 `d8` 打进 `classes.dex`（app_process 直接加载，自包含）。
- `options`：新增 `port=<n>`（UDP 端口）、保留 `scid`/`audio_codec`/`audio_bit_rate`；
  去掉 `list_apps` 之外的 socket 相关启动逻辑（listApps 一次性模式不涉及网络）。

## Non-Goals

- ❌ GUI ↔ daemon 本地 IPC 维持 protobuf 现状（不同协议域）
- ❌ 认证/加密（局域网本场景暂不需要；如需后补）
- ❌ 拥塞控制（媒体音频/视频为尽力而为；control 靠滑动窗口限速）
- ❌ USB-only 设备（无 Wi-Fi 场景无法直连 UDP；serial 无 IP 且 `ip route` 解析失败时提示）
