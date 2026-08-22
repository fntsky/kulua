package com.kulua.server;


import com.google.protobuf.ByteString;

import java.net.InetSocketAddress;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

import kulua.direct.CtrlMsg;
import kulua.direct.Frame;
import kulua.direct.HelloAck;

/**
 * 一个 UDP 客户端会话（按源地址解复用）。
 *
 * 职责：
 * - 与 PC 端 control 流的可靠收发（Receiver 处理 PC→phone，Sender 推送 phone→PC）
 * - 本客户端的虚拟显示器（DisplayRegistry）+ 音频捕获（AudioCapture）
 * - 空闲超时 / BYE 拆除
 *
 * 媒体（audio/video）由各自编码线程通过 {@link #sendMedia} 推给 PC；
 * 关键帧 / config 走可靠 control 流（{@code MediaConfig}）。
 */
final class ClientConnection {

    /** 视频 codec id（4B ASCII "h264"）。 */
    static final int VIDEO_CODEC_ID = 0x68323634;

    /** 视频编码码率（Options.video_bit_rate，编码器创建用）。 */
    int videoBitRate() {
        return options.videoBitRate;
    }

    /** 媒体流标识：audio（与 MediaConfig.stream 数值一致，Rust 侧同值）。 */
    static final int MEDIA_STREAM_AUDIO = 1;

    /** 媒体流标识：video。 */
    static final int MEDIA_STREAM_VIDEO = 2;

    private final UdpServer server;
    private final InetSocketAddress addr;
    /** phone 分配的客户端 id（1 起单调）；媒体数据报头携带，phone 按它路由。 */
    private final int clientId;
    private final Options options;

    /** PC 媒体接收端点（媒体端口数据报源地址登记；null = 未登记，发送丢弃）。 */
    private volatile InetSocketAddress videoEndpoint;
    private volatile InetSocketAddress audioEndpoint;

    private final ReliableControl.Sender ctrlSender;
    private final ReliableControl.Receiver ctrlReceiver = new ReliableControl.Receiver();

    private final DisplayRegistry displays = new DisplayRegistry();
    private AudioCapture audio;
    private final boolean wantAudio;

    private volatile long lastActivityNanos = System.nanoTime();
    private volatile boolean closed;

    // ── 心跳巡检（UdpServer 每 5s 一拍调用 tickHeartbeat）──
    /** 本拍内收到过心跳（onFrame 置位，tick 消费）。 */
    private final AtomicBoolean heartbeatSeen = new AtomicBoolean(false);
    /** 连续多少拍没收到心跳。 */
    private final AtomicInteger heartbeatMisses = new AtomicInteger();

    private final AtomicInteger mediaMsgId = new AtomicInteger(1);
    /** 每流递增的媒体消息序号（音频/视频各自调用）。 */
    private final AtomicInteger mediaSeq = new AtomicInteger(1);

    // ── 诊断计数/限频（只用于日志，不参与协议）──
    /** 已发送的媒体消息数（stats 日志用）。 */
    private final AtomicLong mediaMsgsSent = new AtomicLong();
    /** 媒体端点未注册丢弃的上次告警时间。 */
    private volatile long lastMediaDropWarnNanos;
    private Thread statsThread;

    private final Clipboard.ChangeListener clipboardListener = this::onClipboardChanged;

    ClientConnection(UdpServer server, InetSocketAddress addr, int clientId,
                     boolean wantAudio, Options options) {
        this.server = server;
        this.addr = addr;
        this.clientId = clientId;
        this.wantAudio = wantAudio;
        this.options = options;
        this.ctrlSender = new ReliableControl.Sender(server.sink(addr));
        this.ctrlSender.start();
        watchdog();
        startStats();
        sendHelloAck();
        Clipboard.addChangeListener(clipboardListener);
        if (wantAudio) {
            startAudio(options.audioCodec, false);
        }
    }

    /** 启动空闲超时看门狗：60s 无任何 datagram → 拆除本客户端。 */
    private void watchdog() {
        Thread t = new Thread(() -> {
            while (!closed) {
                try {
                    Thread.sleep(1000);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    return;
                }
                if (System.nanoTime() - lastActivityNanos >= UdpServer.SESSION_TIMEOUT_NANOS) {
                    Server.i("client idle timeout: " + addr);
                    close(ClientCloseReason.REASON_IDLE);
                    return;
                }
            }
        }, "client-watchdog-" + addr.getPort());
        t.setDaemon(true);
        t.start();
    }

    /**
     * 每 10s 打一条会话健康统计（诊断用，量级极低）：
     * 媒体消息数 / ctrl 窗口积压 / 是否锁存 / 媒体端点是否已注册。
     * 卡死后对照：media_msgs 不再增长 = 编码器死了；STALLED = 可靠层锁存。
     */
    private void startStats() {
        statsThread = new Thread(() -> {
            while (!closed) {
                try {
                    Thread.sleep(10_000);
                } catch (InterruptedException e) {
                    return;
                }
                if (closed) {
                    return;
                }
                Server.i("stats cid=" + clientId + " " + addr
                        + " media_msgs=" + mediaMsgsSent.get()
                        + " ctrl_pending=" + ctrlSender.pending()
                        + (ctrlSender.isStalled() ? " STALLED" : "")
                        + " v_ep=" + (videoEndpoint != null)
                        + " a_ep=" + (audioEndpoint != null));
            }
        }, "client-stats-" + addr.getPort());
        statsThread.setDaemon(true);
        statsThread.start();
    }

    /** 会话关闭原因（日志用）。 */
    enum ClientCloseReason {
        REASON_IDLE,
        REASON_BYE,
        REASON_STALLED,
        REASON_CTRL_DEAD,
        REASON_NO_HEARTBEAT,
    }

    InetSocketAddress addr() {
        return addr;
    }

    int clientId() {
        return clientId;
    }

    /**
     * 媒体端口数据报登记入口（UdpServer 媒体接收线程调用，含 OPEN）：
     * 把源地址登记为对应流的发送端点并刷新活跃时间。
     */
    void registerMediaEndpoint(int stream, InetSocketAddress src) {
        if (stream == MEDIA_STREAM_VIDEO) {
            if (!src.equals(videoEndpoint)) {
                // 诊断：端点首次注册/变更（变更 = 客户端换端口/网络切换，值得记录）
                Server.i("video endpoint cid=" + clientId + " -> " + src
                        + (videoEndpoint == null ? " (first)" : " (changed)"));
            }
            videoEndpoint = src;
        } else if (stream == MEDIA_STREAM_AUDIO) {
            if (!src.equals(audioEndpoint)) {
                Server.i("audio endpoint cid=" + clientId + " -> " + src
                        + (audioEndpoint == null ? " (first)" : " (changed)"));
            }
            audioEndpoint = src;
        }
        touch();
    }

    void touch() {
        lastActivityNanos = System.nanoTime();
    }

    /**
     * 5s 心跳巡检（UdpServer 主循环每拍调用）：本拍内收到过心跳则清零计数，
     * 否则连续未收次数 +1。返回当前连续未收次数（超过 MAX_HEARTBEAT_MISS
     * 由调用方拆除会话）。
     */
    int tickHeartbeat() {
        if (heartbeatSeen.compareAndSet(true, false)) {
            heartbeatMisses.set(0);
            return 0;
        }
        return heartbeatMisses.incrementAndGet();
    }

    long lastActivityNanos() {
        return lastActivityNanos;
    }

    /** 处理收到的 datagram。UdpServer 已按地址路由到本客户端。 */
    void onFrame(Frame frame) {
        if (closed) {
            return;
        }
        touch();
        switch (frame.getType()) {
            case HELLO:
                // 客户端重启/重连：幂等回 ACK
                sendHelloAck();
                break;
            case ACK:
                ctrlSender.onAck(frame.getAckSeq());
                break;
            case BYE:
                close(ClientCloseReason.REASON_BYE);
                break;
            case HEARTBEAT:
                // 保活（touch 已刷新）+ 心跳巡检计数清零依据
                heartbeatSeen.set(true);
                break;
            case DATA:
                onData(frame);
                break;
            default:
                Server.w("unhandled frame type from " + addr + ": " + frame.getType());
                break;
        }
    }

    private void onData(Frame frame) {
        if (frame.getStream() != Frame.Stream.CTRL) {
            // phone 是媒体源，PC 不应向其发 audio/video DATA
            return;
        }
        List<byte[]> delivered = ctrlReceiver.onFrame(frame);
        if (ctrlReceiver.isDead()) {
            close(ClientCloseReason.REASON_CTRL_DEAD);
            return;
        }
        // 回累积 ACK：PC 的可靠发送靠 ACK 释放滑动窗口。若不回，PC 窗口积压 →
        // 重传超限判死 → 连接关闭（实测输入注入 / RESIZE / 剪贴板全部失效）。
        // 单线程接收循环里处理完一条即回一次（累积确认天然抗 ACK 丢失）。
        sendAck();
        for (byte[] bytes : delivered) {
            try {
                CtrlMsg msg = CtrlMsg.parseFrom(bytes);
                ControlChannel.handle(this, msg);
            } catch (Exception e) {
                Server.w("bad ctrl msg from " + addr, e);
            }
        }
    }

    /** 发送控制流累积 ACK（确认到 ackSeq()，含）。 */
    private void sendAck() {
        int ack = ctrlReceiver.ackSeq();
        server.sendDatagramRaw(addr, Frame.newBuilder()
                .setStreamValue(Frame.Stream.CTRL.getNumber())
                .setTypeValue(Frame.Type.ACK.getNumber())
                .setAckSeq(ack)
                .build()
                .toByteArray());
    }

    /** 回复 HELLO_ACK（初始 codec 状态）。audio_codec=0 表示未请求/禁用。 */
    private void sendHelloAck() {
        int audioCodec = wantAudio ? codecIdFor(options.audioCodec) : 0;
        HelloAck ack = HelloAck.newBuilder()
                .setScid(options.scid)
                .setAudioCodec(audioCodec)
                .setVideoCodec(VIDEO_CODEC_ID)
                .setClientId(clientId)
                .build();
        server.sendDatagramRaw(addr, Frame.newBuilder()
                .setStreamValue(Frame.Stream.CTRL.getNumber())
                .setTypeValue(Frame.Type.HELLO_ACK.getNumber())
                .setHelloAck(ack)
                .build()
                .toByteArray());
    }

    /** codec 名称 → 4B codec id（与 AudioCapture.codecIdBytes 一致）。 */
    static int codecIdFor(String codecName) {
        switch (codecName) {
            case "opus": return 0x6f707573;
            case "aac":  return 0x00616163;
            case "flac": return 0x666c6163;
            default:     return 0x00726177; // raw
        }
    }

    /** 启动音频捕获（wantAudio 客户端的音频流/热切换）。 */
    void startAudio(String codecName, boolean hotSwitch) {
        if (audio != null) {
            audio.stop();
            audio = null;
        }
        audio = new AudioCapture(codecName, options.audioBitRate, this);
        audio.start();
        if (hotSwitch) {
            // codec 热切换后回执（首次由 HELLO_ACK 带出）
            sendCtrlMsg(CtrlMsg.newBuilder()
                    .setAudioReady(kulua.direct.AudioReady.newBuilder()
                            .setCodecId(codecIdFor(codecName)))
                    .build());
        }
    }

    /** 创建视频虚拟显示器（fusion-viewer 的 CreateDisplay）。 */
    void createDisplay(int width, int height, int dpi) {
        long t0 = android.os.SystemClock.elapsedRealtime();
        DisplayRegistry.DisplayEntry entry = displays.create(width, height, dpi);
        DisplayEncoder encoder = new DisplayEncoder(entry.id, this, displays, width, height, dpi);
        displays.attachEncoder(entry.id, encoder);
        try {
            encoder.start();
        } catch (Exception e) {
            Server.e("start display encoder failed #" + entry.id, e);
            displays.destroy(entry.id);
            return;
        }
        // WHY：DisplayReady 必须等 VirtualDisplay 真正 attach——viewer 收到即发
        // START_APP/触摸，早于 attach 会被 systemDisplayId 兜底成 0（真屏）。
        // 编码线程创建成功或失败都会放行 latch，失败用 isRunning 区分。
        boolean vdInTime = encoder.awaitVirtualDisplay(2000);
        // 诊断：这段等待发生在共享 ctrl 循环线程上，耗时直接决定其他客户端
        // 的 ctrl 停摆时长（输入失效/ACK 延迟），必须可观测。
        Server.i("display #" + entry.id + " encoder wait="
                + (android.os.SystemClock.elapsedRealtime() - t0) + "ms"
                + " vdInTime=" + vdInTime + " running=" + encoder.isRunning());
        if (!encoder.isRunning()) {
            Server.e("display #" + entry.id + " encoder failed before ready");
            displays.destroy(entry.id);
            return;
        }
        if (!vdInTime) {
            Server.w("display #" + entry.id + " virtual display attach slow (>2s)");
        }
        sendCtrlMsg(CtrlMsg.newBuilder()
                .setDisplayReady(kulua.direct.DisplayReady.newBuilder()
                        .setDisplayId(entry.id)
                        .setCodecId(VIDEO_CODEC_ID)
                        .setWidth(width)
                        .setHeight(height)
                        .setDpi(dpi))
                .build());
        Server.i("display #" + entry.id + " created for " + addr
                + " total=" + (android.os.SystemClock.elapsedRealtime() - t0) + "ms");
    }

    DisplayRegistry displays() {
        return displays;
    }

    /** 可靠发送一条 CtrlMsg（phone → PC）。 */
    void sendCtrlMsg(CtrlMsg msg) {
        ctrlSender.sendCtrlMsg(msg);
    }

    /** 可靠发送媒体 config 帧（AAC ADTS / FLAC STREAMINFO / H.264 SPS+PPS）。 */
    void sendMediaConfig(int stream, byte[] data) {
        sendCtrlMsg(CtrlMsg.newBuilder()
                .setMediaConfig(kulua.direct.MediaConfig.newBuilder()
                        .setStream(stream)
                        .setData(ByteString.copyFrom(data)))
                .build());
    }

    /**
     * 尽力而为发送媒体分片（audio/video）：25B 定长头 + ≤1200B 负载，
     * 从对应媒体端口 socket 发往登记端点。
     *
     * 每条消息共享 {@code msgSeq}/{@code msgId}，分片携带 pts/flags（接收侧以
     * frag==0 的值为准）；端点未登记（OPEN 未到/已拆除）时静默丢弃。
     */
    void sendMedia(int stream, int msgSeq, long pts, int flags, byte[] data) {
        InetSocketAddress dst =
                stream == MEDIA_STREAM_AUDIO ? audioEndpoint : videoEndpoint;
        if (dst == null) {
            // 诊断：OPEN 未到/已拆除时静默丢帧是协议行为，但持续发生说明
            // 端点注册链路有问题（viewer OPEN 没到/被防火墙拦），限频告警
            long now = System.nanoTime();
            if (now - lastMediaDropWarnNanos >= 3_000_000_000L) {
                lastMediaDropWarnNanos = now;
                Server.w("media dropped (endpoint not registered): cid=" + clientId
                        + " stream=" + stream);
            }
            return;
        }
        mediaMsgsSent.incrementAndGet();
        int msgId = mediaMsgId.getAndIncrement();
        int total = (data.length + ReliableControl.MAX_FRAGMENT - 1)
                / ReliableControl.MAX_FRAGMENT;
        for (int i = 0; i < total; i++) {
            int start = i * ReliableControl.MAX_FRAGMENT;
            int len = Math.min(data.length - start, ReliableControl.MAX_FRAGMENT);
            server.sendMediaRaw(stream, dst, MediaDatagram.encode(
                    clientId, msgId, msgSeq, i, total, pts, flags, data, start, len));
        }
    }

    /** 每流递增的媒体消息序号（音频/视频各自调用）。 */
    int nextMediaSeq() {
        return mediaSeq.getAndIncrement();
    }

    private void onClipboardChanged(String text) {
        sendCtrlMsg(CtrlMsg.newBuilder()
                .setClipboardChanged(kulua.direct.ClipboardChanged.newBuilder()
                        .setText(text))
                .build());
    }

    /** 关闭本客户端会话并回收资源。 */
    void close(ClientCloseReason reason) {
        if (closed) {
            return;
        }
        closed = true;
        if (statsThread != null) {
            statsThread.interrupt();
        }
        // 诊断：关闭时带上可靠层现场，区分「正常 BYE」和「锁存后变僵尸被拆」
        Server.i("close client " + addr + " cid=" + clientId + " (" + reason + ")"
                + " ctrl_pending=" + ctrlSender.pending()
                + (ctrlSender.isStalled() ? " SENDER_STALLED" : ""));
        ctrlSender.stop();
        Clipboard.removeChangeListener(clipboardListener);
        if (audio != null) {
            audio.stop();
            audio = null;
        }
        displays.destroyAll();
        server.removeClient(this);
    }

    boolean isClosed() {
        return closed;
    }
}
