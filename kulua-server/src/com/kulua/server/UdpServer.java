package com.kulua.server;

import android.util.Log;

import java.io.IOException;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetSocketAddress;
import java.net.SocketTimeoutException;
import java.util.HashMap;
import java.util.Map;

import kulua.direct.Frame;
import kulua.direct.Hello;

/**
 * UDP 直连服务器：绑定 phone 的 Wi-Fi 网络端口，按源地址解复用客户端。
 *
 * 替换旧的 ConnectionManager（adb forward + localabstract 多连接）。
 * 单一 DatagramSocket，receive 循环单线程处理 control；音频/视频编码线程
 * 通过本类的 `sendDatagramRaw` 推送媒体分片。
 */
public final class UdpServer {
    private static final String TAG = "kulua-server";

    /** 客户端空闲超时（60s 无任何 datagram → 拆除，含虚拟显示器/音频）。 */
    static final long SESSION_TIMEOUT_NANOS = 60_000_000_000L;

    private final Options options;
    private final DatagramSocket socket;
    private final Map<InetSocketAddress, ClientConnection> clients = new HashMap<>();

    public UdpServer(Options options) throws IOException {
        this.options = options;
        this.socket = new DatagramSocket(options.port);
        // 500ms 接收超时 → 循环里周期性清扫空闲客户端
        this.socket.setSoTimeout(500);
        Log.i(TAG, "udp server listening on 0.0.0.0:" + options.port);
    }

    /** 主循环：收包 → 路由 → 分发。 */
    public void loop() throws IOException {
        byte[] buf = new byte[65536];
        while (true) {
            DatagramPacket packet = new DatagramPacket(buf, buf.length);
            try {
                socket.receive(packet);
            } catch (SocketTimeoutException e) {
                sweepIdle();
                continue;
            }
            InetSocketAddress src = (InetSocketAddress) packet.getSocketAddress();
            Frame frame;
            try {
                frame = Frame.parseFrom(
                        com.google.protobuf.ByteString.copyFrom(
                                packet.getData(), 0, packet.getLength()));
            } catch (Exception e) {
                Log.w(TAG, "ignoring bad datagram from " + src);
                continue;
            }

            ClientConnection client = clients.get(src);
            if (client == null) {
                // 仅接受 scid 匹配的 HELLO 建立会话
                if (frame.getType() == Frame.Type.HELLO && frame.getPayloadCase() == Frame.PayloadCase.HELLO) {
                    client = acceptSrc(src, frame.getHello());
                    if (client == null) {
                        continue;
                    }
                } else {
                    continue; // 未握手，忽略
                }
            }
            client.onFrame(frame);
        }
    }

    /** 校验 scid 并建立客户端会话（校验失败返回 null）。 */
    private ClientConnection acceptSrc(InetSocketAddress src, Hello hello) {
        if (!options.scid.equalsIgnoreCase(hello.getScid())) {
            Log.w(TAG, "reject hello scid mismatch from " + src + ": " + hello.getScid());
            return null;
        }
        ClientConnection client = new ClientConnection(this, src, hello.getAudio(), options);
        clients.put(src, client);
        Log.i(TAG, "client connected: " + src + " audio=" + hello.getAudio());
        return client;
    }

    /** 供编码线程/可靠发送把 datagram 发给指定客户端。 */
    void sendDatagramRaw(InetSocketAddress dst, byte[] wire) {
        try {
            socket.send(new DatagramPacket(wire, wire.length, dst));
        } catch (IOException e) {
            Log.w(TAG, "udp send to " + dst + " failed: " + e.getMessage());
        }
    }

    /** 绑定到某客户端的发送回调（ReliableControl.Sink / media 用）。 */
    ReliableControl.Sink sink(InetSocketAddress dst) {
        return wire -> sendDatagramRaw(dst, wire);
    }

    void removeClient(ClientConnection client) {
        clients.remove(client.addr());
    }

    /** 清扫空闲客户端（watchdog 之外的双保险）。 */
    private void sweepIdle() {
        long now = System.nanoTime();
        for (ClientConnection c : clients.values()) {
            if (now - c.lastActivityNanos() >= SESSION_TIMEOUT_NANOS) {
                c.close(ClientConnection.ClientCloseReason.REASON_IDLE);
            }
        }
    }
}
