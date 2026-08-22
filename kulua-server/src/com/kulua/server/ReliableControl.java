package com.kulua.server;

import com.google.protobuf.ByteString;


import java.io.ByteArrayOutputStream;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.TreeMap;
import java.util.concurrent.atomic.AtomicBoolean;

import kulua.direct.Frame;

/**
 * control 流可靠传输（phone 侧实现，算法字段与 kulua-proto/src/reliable.rs 对齐）。
 *
 * - Sender：单调 {@code seq}，每个 control 分片独占一个 seq（ACK 粒度 = 分片）；
 *   未确认帧保存在窗口内，由独立定时器线程超时重传（指数退避），丧失进展判死。
 * - Receiver：按 {@code seq} 缓冲，连续完整后按序投递整条消息，回复累积 ACK。
 */
public final class ReliableControl {


    public static final int WINDOW = 32;
    public static final long INITIAL_RTO_NANOS = 200_000_000L;
    public static final long MAX_RTO_NANOS = 1_600_000_000L;
    public static final int LOSS_LIMIT = 8;
    /** 单个 datagram 的安全负载上限（与 Rust MAX_FRAGMENT 一致）。 */
    public static final int MAX_FRAGMENT = 1200;
    public static final int MAX_RECV_BUFFER = 4096;

    private ReliableControl() {
        // not instantiable
    }

    /** 环形序比较（与 Rust seq_le 对齐）：`a <= b`（考虑 u32 回绕）。 */
    public static boolean seqLe(int a, int b) {
        if (a == b) {
            return true;
        }
        // a - b 以无符号 32 位解释，>= 0x80000000 表示 a 落后于 b
        return Integer.compareUnsigned(a - b, 0x8000_0000) >= 0;
    }

    /** 环形序：`seq` 是否落在 `[base, base+2^30)` 窗口内（太旧/太超前都丢弃）。 */
    public static boolean inWindow(int seq, int base) {
        return Integer.compareUnsigned(seq - base, 0x4000_0000) < 0;
    }

    private static long rtoFor(int loss) {
        long rto = INITIAL_RTO_NANOS;
        for (int i = 0; i < Math.min(loss, 4); i++) {
            rto *= 2;
        }
        return Math.min(rto, MAX_RTO_NANOS);
    }

    /** 发送侧写入 datagram 的回调（绑定到本客户端的 udp 发送）。 */
    public interface Sink {
        void sendDatagram(byte[] wire);
    }

    /** 窗口积压告警阈值（接近但未判死，提示 ACK 断供趋势）。 */
    private static final int BACKLOG_WARN = 24;
    /** 告警最小间隔（防刷屏）。 */
    private static final long WARN_INTERVAL_NANOS = 2_000_000_000L;

    /** 一条未确认分片。 */
    private static final class OutEntry {
        final int seq;
        final byte[] wire;
        long lastSentNanos;
        int lossCount;
        /** 是否已打过重传压力告警（每条只告警一次，防刷屏）。 */
        boolean warned;

        OutEntry(int seq, byte[] wire, long lastSentNanos) {
            this.seq = seq;
            this.wire = wire;
            this.lastSentNanos = lastSentNanos;
        }
    }

    /** 可靠发送器（phone → PC 的 control 推送）。 */
    public static final class Sender {
        private final Sink sink;
        private final java.util.ArrayDeque<OutEntry> window = new java.util.ArrayDeque<>();
        private final AtomicBoolean closed = new AtomicBoolean(false);
        private volatile boolean stalled;
        private int nextSeq = 1;
        private int nextMsgId = 1;
        private long lastWarnNanos;
        private Thread timer;

        public Sender(Sink sink) {
            this.sink = sink;
        }

        public void start() {
            timer = new Thread(() -> {
                while (!closed.get()) {
                    try {
                        Thread.sleep(50);
                    } catch (InterruptedException e) {
                        Thread.currentThread().interrupt();
                        return;
                    }
                    retransmit();
                }
            }, "reliable-send");
            timer.setDaemon(true);
            timer.start();
        }

        public void stop() {
            closed.set(true);
            if (timer != null) {
                try {
                    timer.join(1000);
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
                timer = null;
            }
        }

        /** 编码并可靠发送一条 CtrlMsg（分片，每条分片独占一个 seq）。 */
        public synchronized void sendCtrlMsg(kulua.direct.CtrlMsg msg) {
            sendCtrlBytes(msg.toByteArray());
        }

        /** 把任意字节（如已编码的 CtrlMsg）分片可靠发送。 */
        private synchronized void sendCtrlBytes(byte[] payload) {
            if (closed.get() || stalled) {
                return;
            }
            // 诊断：窗口积压说明 PC 端 ACK 断供（viewer 卡住/链路拥塞），提前告警
            long now = System.nanoTime();
            if (window.size() >= BACKLOG_WARN && now - lastWarnNanos >= WARN_INTERVAL_NANOS) {
                lastWarnNanos = now;
                Server.w("ctrl window backlog: pending=" + window.size()
                        + " (ACK 断供趋势，对端可能卡顿/丢包)");
            }
            int msgId = nextMsgId++;
            int total = payload.length == 0 ? 1
                    : (payload.length + MAX_FRAGMENT - 1) / MAX_FRAGMENT;
            for (int i = 0; i < total; i++) {
                int start = i * MAX_FRAGMENT;
                int end = Math.min(payload.length, start + MAX_FRAGMENT);
                int seq = nextSeq++;
                Frame frame = Frame.newBuilder()
                        .setStreamValue(Frame.Stream.CTRL.getNumber())
                        .setTypeValue(Frame.Type.DATA.getNumber())
                        .setSeq(seq)
                        .setMsgId(msgId)
                        .setFrag(i)
                        .setFragTotal(total)
                        .setData(ByteString.copyFrom(payload, start, end - start))
                        .build();
                byte[] wire = frame.toByteArray();
                window.addLast(new OutEntry(seq, wire, System.nanoTime()));
                sink.sendDatagram(wire);
            }
        }

        /** 处理累积 ACK：释放 {@code ackSeq} 之前（含）的所有分片。 */
        public synchronized void onAck(int ackSeq) {
            while (!window.isEmpty() && seqLe(window.peekFirst().seq, ackSeq)) {
                window.removeFirst();
            }
        }

        /** 定时器：到期分片重传；丧失进展判死。 */
        private synchronized void retransmit() {
            if (window.isEmpty() || stalled) {
                return;
            }
            long now = System.nanoTime();
            for (OutEntry e : window) {
                if (now - e.lastSentNanos >= rtoFor(e.lossCount)) {
                    e.lastSentNanos = now;
                    e.lossCount++;
                    if (e.lossCount > LOSS_LIMIT) {
                        stalled = true;
                        // 诊断：锁存现场（此后该客户端的 MediaConfig/DisplayReady/
                        // 剪贴板推送全部静默丢弃，必须能看到这一行才能定位僵尸会话）
                        OutEntry oldest = window.peekFirst();
                        Server.e("reliable sender STALLED: pending=" + window.size()
                                + " oldest_seq=" + (oldest == null ? -1 : oldest.seq)
                                + " oldest_age_ms=" + (oldest == null ? -1
                                        : (now - oldest.lastSentNanos) / 1_000_000)
                                + " loss=" + e.lossCount + " seq=" + e.seq);
                        return;
                    }
                    if (e.lossCount == LOSS_LIMIT / 2 && !e.warned) {
                        e.warned = true;
                        Server.w("reliable retransmit pressure: seq=" + e.seq
                                + " loss=" + e.lossCount + " pending=" + window.size());
                    }
                    sink.sendDatagram(e.wire);
                }
            }
        }

        /** 当前未确认分片数（诊断统计用）。 */
        public synchronized int pending() {
            return window.size();
        }

        public boolean isStalled() {
            return stalled;
        }
    }

    /** 接收方向的一条分片。 */
    private static final class RecvFragment {
        final int msgId;
        final int frag;
        final int total;
        final byte[] data;

        RecvFragment(int msgId, int frag, int total, byte[] data) {
            this.msgId = msgId;
            this.frag = frag;
            this.total = total;
            this.data = data;
        }
    }

    /** 可靠接收器（PC → phone 的 control DATA）。 */
    public static final class Receiver {
        private int nextSeq = 1;
        private final TreeMap<Integer, RecvFragment> buf = new TreeMap<>();
        private boolean dead;

        public synchronized boolean isDead() {
            return dead;
        }

        /** 输入一个 control DATA 分片帧，返回完整投递的 CtrlMsg 字节列表。 */
        public synchronized List<byte[]> onFrame(Frame frame) {
            if (dead || frame.getPayloadCase() != Frame.PayloadCase.DATA) {
                return Collections.emptyList();
            }
            byte[] data = frame.getData().toByteArray();
            int seq = frame.getSeq();
            if (!inWindow(seq, nextSeq)) {
                return Collections.emptyList(); // 太旧/重复/太超前
            }
            int total = frame.getFragTotal() == 0 ? 1 : frame.getFragTotal();
            int frag = frame.getFrag();
            if (frag >= total) {
                return Collections.emptyList();
            }
            buf.put(seq, new RecvFragment(frame.getMsgId(), frag, total, data));

            List<byte[]> delivered = new ArrayList<>();
            // 连续投递：单分片 + 完整多分片消息都可推进
            while (true) {
                RecvFragment first = buf.get(nextSeq);
                if (first == null) {
                    break;
                }
                if (first.frag == 0 && first.total == 1) {
                    buf.remove(nextSeq);
                    delivered.add(first.data);
                    nextSeq++;
                    continue;
                }
                if (first.frag != 0) {
                    break; // 等待消息起点
                }
                // 多分片：nextSeq..nextSeq+total-1 连续且 msgId 一致
                boolean complete = true;
                List<RecvFragment> run = new ArrayList<>(first.total);
                for (int i = 0; i < first.total; i++) {
                    RecvFragment fragEntry = buf.get(nextSeq + i);
                    if (fragEntry == null || fragEntry.msgId != first.msgId || fragEntry.frag != i) {
                        complete = false;
                        break;
                    }
                    run.add(fragEntry);
                }
                if (!complete) {
                    break;
                }
                try {
                    ByteArrayOutputStream out = new ByteArrayOutputStream();
                    for (RecvFragment fr : run) {
                        out.write(fr.data);
                    }
                    for (int i = 0; i < first.total; i++) {
                        buf.remove(nextSeq + i);
                    }
                    nextSeq += first.total;
                    delivered.add(out.toByteArray());
                } catch (java.io.IOException e) {
                    // ByteArrayOutputStream 不会抛
                }
            }
            if (buf.size() > MAX_RECV_BUFFER) {
                dead = true;
            }
            return delivered;
        }

        /** 累积 ACK 值 = 已确认的最大连续 seq（nextSeq - 1）。 */
        public synchronized int ackSeq() {
            return nextSeq - 1;
        }
    }
}
