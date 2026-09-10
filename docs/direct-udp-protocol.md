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
| 三端口分流 | ctrl=P / video=P+1 / audio=P+2：单 socket 时音视频分片共享发送缓冲（视频关键帧突发挤掉音频），且 ctrl 单循环要为每个媒体分片跑 protobuf 解析；分端口后三路缓冲独立、媒体路径零 protobuf（25B 定长头） |
| phone 绑定 0.0.0.0:port | shell 用户可 bind UDP；PC 端用 phone 的 Wi-Fi IP（来自 `adb connect` serial 或 `ip route` 解析）直连 |

## 数据报格式

**control 端口（P）**：每个 UDP datagram = 一个 protobuf `Frame`（datagram 边界即
消息边界）。媒体已迁出，`Frame.Stream` 只剩 `CTRL`。全字段定义见
`proto/direct.proto`，要点：

```
message Frame {
  Stream stream     // 仅 CTRL
  Type   type       // DATA / ACK / HELLO / HELLO_ACK / HEARTBEAT / BYE
  uint32 seq        // control 单调递增（ACK/去重/窗口滑动）
  uint32 msg_id     // 分片归属（同一消息的所有分片共享）
  uint32 frag / frag_total
  oneof payload {
    bytes  data        // control 负载（重装后为 CtrlMsg，可靠）
    uint32 ack_seq     // ACK：确认到该序号（control 累积）
    Hello  hello       // PC→phone 会话打开
    HelloAck hello_ack // phone→PC（含 client_id）
  }
}
```

**媒体端口（video=P+1 / audio=P+2）**：每个 UDP datagram = **33B 大端定长头 +
负载（≤1200B）**，非 protobuf：

```
offset  size  field
0      4     client_id  （HELLO_ACK 分配，phone 按它路由到会话）
4      4     msg_id     （一条媒体帧一个 id，分片共享）
8      4     seq        （每流单调递增，排序/去重用）
12     2     frag       （分片索引，0 起）
14     2     frag_total （0 = 单包完整；0xFFFF 且负载为空 = OPEN 注册包）
16     8     pts        （微秒；接收侧以 frag==0 的值为准）
24     8     revision   （音频代次 = 产生本帧的采集器对应的 SetAudio.revision；
                        视频恒 0。接收侧只接受当前代次，避免开关/编码切换时
                        上一代在途音频串入新播放队列）
32     1     flags      （bit0=config bit1=keyframe）
```

WHY 不进 protobuf：媒体分片速率高（8Mbps 视频 ≈170 片/s/viewer），定长头
解析 O(1) 且省去每片 20~35B 的 envelope；phone 端 app_process 环境也无需
引入任何新依赖。

`CtrlMsg` 覆盖所有 control 命令（`inject_keycode/text/touch/scroll`、
`back_or_screen_on`、`set_clipboard`、`start_app`、`resize_display`、
`create/destroy_display`、`clipboard_changed`、`ack_clipboard`、
`display_ready`、`set_audio`、`audio_state`）。字段沿用原字节协议的语义
（displayId 前缀、pointerId、压力定点等均以原生类型表达，Java/Rust 侧转换
职责分离：Rust 生成 protobuf，Java 消费 protobuf 转 Android 注入）。

## 会话模型

UDP 无连接概念，phone 端三个 socket 各司其职：

```
phone (0.0.0.0:P / P+1 / P+2, 三个 DatagramSocket)
  ├─ ctrl 循环      → 按 src addr 解复用 ClientConnection（protobuf Frame）
  ├─ video 循环     → 按 25B 头 client_id 找会话，登记源地址为 video 发送端点
  └─ audio 循环     → 同上，登记 audio 发送端点

客户端构成：
  ├─ daemon 客户端：ctrl(双向) + audio 端点（收音频推流）
  └─ 每个 fusion-viewer 客户端：ctrl(双向) + video 端点（含各自虚拟显示器）
```

### 握手

1. PC 把 ctrl socket bind 到随机本地端口，向 `phone_ip:P` 发 `HELLO{scid}`，
   每 500ms 重发（最多 20 次 → 判死）。
2. phone 校验 `scid` 后回 `HELLO_ACK`（附带 `audio_codec` + **`client_id`**，
   phone 侧 1 起单调分配），并注册该源地址为本客户端 ctrl 端点。
3. PC 打开媒体 socket（bind 随机本地端口、connect 到 `phone_ip:P+1/P+2`），
   在媒体端口周期发 **OPEN**（33B 头，frag_total=0xFFFF、无负载，携 client_id，
   500ms 重发直到收到首个媒体包）。phone 收到媒体端口上任何头合法且
   client_id 可识别的数据报，即把源地址登记为该流的发送端点——OPEN 只是
   首次注册的触发器。
   - daemon 会话**恒打开音频端口**（哪怕会话初始音频是关闭）：音频可在会话
     中途热开启，接收端点必须先就位，否则开启后 phone 无处发送、也无从再登记。
   - 融合窗口只收视频：不开音频端口，`Hello.audio=false`。
4. daemon 发 control 信息走 `Frame{CTRL, DATA, CtrlMsg}`；`Hello.audio` 只表达
   "握手时是否就让 phone 起采集"（= 会话初始音频目标，代次 0）。
5. viewer 创建显示器：发 `CtrlMsg.create_display{width,height,dpi}`（可靠），
   phone 创建 VirtualDisplay + 编码器，回 `CtrlMsg.display_ready{...}`。

### 音频：会话内独立启停（热切换）

音频链路可在会话存活期间独立开关与换编码，**不重启会话**（端口 / 剪贴板 /
通知 / 融合窗口全部保持）：

1. Core 把用户目标写进 `watch<AudioTarget>{enabled, codec}`（连续操作自动合并）。
2. Session 主循环发现目标变化 → 代次 +1 → 发 `SetAudio{enabled, codec, revision}`，
   并立即把它交给音频任务（切代次）：旧代次的 Config/Frame 全部丢弃，关闭时
   立刻释放播放资源（rodio sink + 输出设备）。
3. phone 侧由**专用单线程执行器**串行处理启停（启停要 join 采集线程，不能占
   ctrl 线程，否则阻塞所有客户端的心跳与输入注入）：先停旧链路，再按目标启动，
   然后回 `AudioState{revision, enabled, codec_id, error}`（传输 ACK 只代表命令
   收到，业务结果看这里）。
4. Session 依据回执维护运行态（off / starting / on / stopping / failed）推给 UI；
   等不到回执（2s）就换代次重试（最多 3 次），仍无回执 → failed + 原因。
5. phone 采集自愈后（重建采集器并真正送出帧）会再发一次同代次的 `AudioState`，
   PC 据此把 failed 翻回 on；PC 侧连续 3s 没有新帧也判断流（指标归零 + 提示），
   并按 10s 间隔换代次自动重发（最多 3 次）尝试恢复。
6. 开关与编码用同一条命令下发：分开下发会出现"先按旧 codec 起采集、再按新
   codec 重起"的中间态；`codec=0` 表示 raw，**不能**用来表示关闭。

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

- 媒体数据报格式见上文「数据报格式」；phone 从**对应端口的 socket**发送
  （PC 端 socket 是 connect() 的，源端口须匹配）；端点未登记时静默丢弃
  （OPEN 注册完成前的少量首帧丢失可接受，config 可靠兜底 + 关键帧重同步）。
- **audio**：尽力而为；乱序/重复按 seq 丢弃（20ms 帧丢失可掩蔽）；按媒体头
  `revision` 过滤，只接受当前代次。
- **video**：尽力而为；任一分片丢失 → 整帧丢弃（解码器下个关键帧重同步）；
  HUD 丢包率用重装器的 lost 计数。
- config 帧（SPS/PPS / AudioSpecificConfig）：**仍走 control 流可靠投递**
  （`CtrlMsg.media_config`，stream 字段 1=audio 2=video，音频另带 `revision`），
  确保解码器可在首帧前初始化；音频 config 只接受当前代次，AAC/FLAC 在拿到
  当前代次的 config 前不解码（否则解码器会一直失败）。

## 端口 / 地址 / 发现

- phone 绑定 UDP `0.0.0.0:P`（ctrl）、`P+1`（video）、`P+2`（audio）；`P` 沿用
  `Core::run` 的 `port_counter`（27183 起）。
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
