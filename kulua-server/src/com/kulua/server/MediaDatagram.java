package com.kulua.server;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * 媒体数据报 33B 大端定长头编解码（独立 UDP 端口承载，非 protobuf）。
 *
 * 布局与 proto/direct.proto 头部注释、Rust 侧（kulua-proto）严格一致，不可偏离：
 *
 * <pre>
 *   offset  size  field
 *   0       4     client_id  （HELLO_ACK 分配，phone 按它路由媒体分片）
 *   4       4     msg_id     （一条媒体帧一个 id，分片共享）
 *   8       4     seq        （每流单调递增）
 *   12      2     frag       （分片索引，0 起）
 *   14      2     frag_total （0 = 单包完整；0xFFFF 且负载为空 = OPEN 注册包）
 *   16      8     pts        （微秒）
 *   24      8     revision   （音频代次 = 本帧所属采集器的 SetAudio.revision；视频恒 0）
 *   32      1     flags      （bit0=config bit1=keyframe）
 * </pre>
 */
final class MediaDatagram {

    /** 头长度（字节）。 */
    static final int HEADER_SIZE = 33;

    /** frag_total 的 OPEN 注册包标记（PC → phone，负载为空）。 */
    static final int FRAG_TOTAL_OPEN = 0xFFFF;

    private MediaDatagram() {
        // not instantiable
    }

    /**
     * 编码一个媒体数据报：33B 头 + 负载切片 {@code payload[off, off+len)}。
     *
     * {@code revision} 是该帧所属音频代次（视频传 0）：PC 只接受当前代次，
     * 避免开关/编码切换时上一代媒体串入新播放队列。
     */
    static byte[] encode(int clientId, int msgId, int seq, int frag, int fragTotal,
                         long pts, long revision, int flags, byte[] payload, int off, int len) {
        ByteBuffer buf = ByteBuffer.allocate(HEADER_SIZE + len).order(ByteOrder.BIG_ENDIAN);
        buf.putInt(clientId);
        buf.putInt(msgId);
        buf.putInt(seq);
        buf.putShort((short) frag);
        buf.putShort((short) fragTotal);
        buf.putLong(pts);
        buf.putLong(revision);
        buf.put((byte) flags);
        buf.put(payload, off, len);
        return buf.array();
    }

    /**
     * 解析数据报头中的 client_id；数据报长度不足头长（头不合法）返回 -1。
     *
     * 负载不消费：phone 是媒体源，收到数据报只需登记端点，不重装媒体。
     */
    static int parseClientId(byte[] buf, int len) {
        if (len < HEADER_SIZE) {
            return -1;
        }
        return ByteBuffer.wrap(buf, 0, 4).order(ByteOrder.BIG_ENDIAN).getInt();
    }
}
