package com.kulua.server;

import android.util.Log;

import com.google.protobuf.ByteString;

import java.net.InetSocketAddress;
import java.util.List;

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
    private static final String TAG = "kulua-server";

    /** 视频 codec id（4B ASCII "h264"）。 */
    static final int VIDEO_CODEC_ID = 0x68323634;

    private final UdpServer server;
    private final InetSocketAddress addr;
    private final Options options;

    private final ReliableControl.Sender ctrlSender;
    private final ReliableControl.Receiver ctrlReceiver = new ReliableControl.Receiver();

    private final DisplayRegistry displays = new DisplayRegistry();
    private AudioCapture audio;
    private final boolean wantAudio;

    private volatile long lastActivityNanos = System.nanoTime();
    private volatile boolean closed;

    private int mediaMsgId = 1;
    private final java.util.concurrent.atomic.AtomicInteger mediaSeq = new java.util.concurrent.atomic.AtomicInteger(1);

    private final Clipboard.ChangeListener clipboardListener = this::onClipboardChanged;

    ClientConnection(UdpServer server, InetSocketAddress addr, boolean wantAudio, Options options) {
        this.server = server;
        this.addr = addr;
        this.wantAudio = wantAudio;
        this.options = options;
        this.ctrlSender = new ReliableControl.Sender(server.sink(addr));
        this.ctrlSender.start();
        watchdog();
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
                    Log.i(TAG, "client idle timeout: " + addr);
                    close(ClientCloseReason.REASON_IDLE);
                    return;
                }
            }
        }, "client-watchdog-" + addr.getPort());
        t.setDaemon(true);
        t.start();
    }

    /** 会话关闭原因（日志用）。 */
    enum ClientCloseReason {
        REASON_IDLE,
        REASON_BYE,
        REASON_STALLED,
        REASON_CTRL_DEAD,
    }

    InetSocketAddress addr() {
        return addr;
    }

    void touch() {
        lastActivityNanos = System.nanoTime();
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
                // 保活（touch 已刷新）
                break;
            case DATA:
                onData(frame);
                break;
            default:
                Log.w(TAG, "unhandled frame type from " + addr + ": " + frame.getType());
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
        for (byte[] bytes : delivered) {
            try {
                CtrlMsg msg = CtrlMsg.parseFrom(bytes);
                ControlChannel.handle(this, msg);
            } catch (Exception e) {
                Log.w(TAG, "bad ctrl msg from " + addr, e);
            }
        }
    }

    /** 回复 HELLO_ACK（初始 codec 状态）。audio_codec=0 表示未请求/禁用。 */
    private void sendHelloAck() {
        int audioCodec = wantAudio ? codecIdFor(options.audioCodec) : 0;
        HelloAck ack = HelloAck.newBuilder()
                .setScid(options.scid)
                .setAudioCodec(audioCodec)
                .setVideoCodec(VIDEO_CODEC_ID)
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
        DisplayRegistry.DisplayEntry entry = displays.create(width, height, dpi);
        DisplayEncoder encoder = new DisplayEncoder(entry.id, this, displays, width, height, dpi);
        displays.attachEncoder(entry.id, encoder);
        try {
            encoder.start();
        } catch (Exception e) {
            Log.e(TAG, "start display encoder failed #" + entry.id, e);
            displays.destroy(entry.id);
            return;
        }
        sendCtrlMsg(CtrlMsg.newBuilder()
                .setDisplayReady(kulua.direct.DisplayReady.newBuilder()
                        .setDisplayId(entry.id)
                        .setCodecId(VIDEO_CODEC_ID)
                        .setWidth(width)
                        .setHeight(height)
                        .setDpi(dpi))
                .build());
        Log.i(TAG, "display #" + entry.id + " created for " + addr);
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
     * 尽力而为发送媒体分片（audio/video）。
     *
     * 每条消息共享 {@code msgSeq}/{@code msgId}，分片携带 pts/flags（首片为准）。
     */
    void sendMedia(int stream, int msgSeq, long pts, int flags, byte[] data) {
        int msgId = mediaMsgId++;
        int total = data.length == 0 ? 1 : (data.length + ReliableControl.MAX_FRAGMENT - 1)
                / ReliableControl.MAX_FRAGMENT;
        for (int i = 0; i < total; i++) {
            int start = i * ReliableControl.MAX_FRAGMENT;
            int end = Math.min(data.length, start + ReliableControl.MAX_FRAGMENT);
            Frame frame = Frame.newBuilder()
                    .setStreamValue(stream)
                    .setTypeValue(Frame.Type.DATA.getNumber())
                    .setSeq(msgSeq)
                    .setMsgId(msgId)
                    .setFrag(i)
                    .setFragTotal(total)
                    .setMediaPts(pts)
                    .setMediaFlags(flags)
                    .setData(ByteString.copyFrom(data, start, end - start))
                    .build();
            server.sendDatagramRaw(addr, frame.toByteArray());
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
        Log.i(TAG, "close client " + addr + " (" + reason + ")");
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
