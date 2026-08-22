package com.kulua.server;


import java.io.IOException;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetSocketAddress;
import java.net.SocketTimeoutException;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;
import kulua.direct.Frame;
import kulua.direct.Hello;

/**
 * UDP 直连服务器：三端口分流 —— ctrl=P（protobuf Frame 控制流）、
 * video=P+1 / audio=P+2（25B 定长二进制头媒体数据报）。
 *
 * ctrl 按源地址解复用客户端；媒体端口只做一件事：按数据报头里的 client_id
 * 找到会话，把源地址登记为该流的发送端点（负载不消费，phone 不重装媒体）。
 * 分端口的 WHY：单 socket 时音视频分片共享发送缓冲（视频关键帧突发挤掉音频），
 * 且 ctrl 单循环要为每个媒体分片跑 protobuf 解析。
 */
public final class UdpServer {

    /** 客户端空闲超时（60s 无任何 datagram → 拆除，含虚拟显示器/音频）。 */
    static final long SESSION_TIMEOUT_NANOS = 60_000_000_000L;

    /** 存活输出 / 心跳巡检周期。 */
    static final long TICK_INTERVAL_NANOS = 5_000_000_000L;
    /** 连续 N 个周期没收到心跳 → 关闭该客户端（约 55s 静默）。 */
    static final int MAX_HEARTBEAT_MISS = 10;
    /** 启动后无任何客户端接入的自动退出宽限。 */
    static final long STARTUP_IDLE_NANOS = 30_000_000_000L;

    private final Options options;
    private final DatagramSocket ctrlSocket;
    private final DatagramSocket videoSocket;
    private final DatagramSocket audioSocket;

    /** ctrl 源地址 → 会话（close() 可能来自 watchdog 线程，需并发安全）。 */
    private final Map<InetSocketAddress, ClientConnection> clients = new ConcurrentHashMap<>();
    /** client_id → 会话（媒体接收线程按它路由，与 ctrl 线程并发访问）。 */
    private final Map<Integer, ClientConnection> clientById = new ConcurrentHashMap<>();
    /** client_id 分配器（1 起单调递增，HELLO_ACK 带出、媒体头回带）。 */
    private final AtomicInteger nextClientId = new AtomicInteger(1);

    // ── 5s 巡检状态（仅主循环线程访问）──
    private final long startNanos = System.nanoTime();
    private long lastTickNanos = System.nanoTime();
    /** 上一轮循环时刻（测 loop 间隔：正常 ≤500ms，被阻塞会显著变大）。 */
    private long lastIterNanos = System.nanoTime();
    private long maxGapNanos;
    /** 是否接入过客户端（区分「会话结束」和「从未有人来」两种退出条件）。 */
    private boolean hadClient;

    public UdpServer(Options options) throws IOException {
        this.options = options;
        this.ctrlSocket = bind(options.ctrlPort());
        this.videoSocket = bind(options.videoPort());
        this.audioSocket = bind(options.audioPort());
        // 500ms 接收超时 → ctrl 循环里周期性清扫空闲客户端（媒体循环无需清扫）
        this.ctrlSocket.setSoTimeout(500);
        Server.i("udp server listening on 0.0.0.0:" + options.ctrlPort()
                + " (video " + options.videoPort() + ", audio " + options.audioPort() + ")");
    }

    /** 绑定 0.0.0.0:port（失败报错带具体端口，便于排查占用/权限）。 */
    private static DatagramSocket bind(int port) throws IOException {
        try {
            return new DatagramSocket(new InetSocketAddress("0.0.0.0", port));
        } catch (IOException e) {
            throw new IOException("bind udp 0.0.0.0:" + port + " failed: " + e.getMessage(), e);
        }
    }

    /** 主循环：收包 → 路由 → 分发；每轮标记间隔，5s 一拍做存活输出 + 心跳巡检。
     *  客户端全部离开（心跳超时/BYE）后返回，进程随之退出、端口自动释放。 */
    public void loop() throws IOException {
        startMediaLoop("media-video", videoSocket, ClientConnection.MEDIA_STREAM_VIDEO);
        startMediaLoop("media-audio", audioSocket, ClientConnection.MEDIA_STREAM_AUDIO);
        byte[] buf = new byte[65536];
        while (!markIteration()) {
            DatagramPacket packet = new DatagramPacket(buf, buf.length);
            try {
                ctrlSocket.receive(packet);
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
                Server.w("ignoring bad datagram from " + src);
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
            // 诊断：单循环串行所有客户端，单条处理慢 = 全场 ctrl 停摆
            // （CreateDisplay/注入都在这里同步跑），>200ms 必须暴露
            long t0 = android.os.SystemClock.elapsedRealtime();
            client.onFrame(frame);
            long cost = android.os.SystemClock.elapsedRealtime() - t0;
            if (cost > 200) {
                Server.w("slow ctrl processing: cid=" + client.clientId()
                        + " type=" + frame.getType() + " cost=" + cost + "ms");
            }
        }
        Server.i("ctrl loop exited: no clients remaining");
    }

    /** 每轮循环调用：累计 loop 间隔；满 5s 触发存活输出 + 心跳巡检。
     *
     * @return true = 客户端已清空（或启动宽限期内无人接入），主循环应退出，
     *         进程随之结束、端口自动释放——server 的「自己死」机制。
     */
    private boolean markIteration() {
        long now = System.nanoTime();
        maxGapNanos = Math.max(maxGapNanos, now - lastIterNanos);
        lastIterNanos = now;
        if (now - lastTickNanos < TICK_INTERVAL_NANOS) {
            return false;
        }
        lastTickNanos = now;
        return tick(now);
    }

    /**
     * 5s 一拍：输出存活行（uptime / 客户端数 / ctrl 循环最大间隔），
     * 并对每个客户端做心跳计数——连续 MAX_HEARTBEAT_MISS 拍没收到心跳就拆除。
     *
     * WHY 用主循环输出：所有 ctrl 处理都在这一个线程上，这行日志消失或
     * loop_max_gap 异常增大 = 主循环被阻塞（卡死的最直接证据）。
     *
     * @return true = 客户端已清空，server 应退出。心跳超时/BYE 关掉最后一个
     *         客户端后走到这里；启动后 30s 无人接入同样退出，端口不残留。
     */
    private boolean tick(long now) {
        long gapMs = maxGapNanos / 1_000_000L;
        maxGapNanos = 0;
        // 快照迭代：close() 内会从 clients 移除，避免遍历时修改
        java.util.List<ClientConnection> snapshot =
                new java.util.ArrayList<>(clients.values());
        StringBuilder missed = new StringBuilder();
        for (ClientConnection c : snapshot) {
            int misses = c.tickHeartbeat();
            if (misses > MAX_HEARTBEAT_MISS) {
                Server.w("client cid=" + c.clientId() + " " + c.addr()
                        + " missed " + misses + " heartbeats (> " + MAX_HEARTBEAT_MISS
                        + "), closing");
                c.close(ClientConnection.ClientCloseReason.REASON_NO_HEARTBEAT);
            } else if (misses > 0) {
                missed.append(" cid=").append(c.clientId()).append(':')
                        .append(misses).append('/').append(MAX_HEARTBEAT_MISS);
            }
        }
        Server.i("alive uptime=" + ((now - startNanos) / 1_000_000_000L) + "s"
                + " clients=" + snapshot.size()
                + " loop_max_gap=" + gapMs + "ms"
                + (missed.length() > 0 ? missed.toString() : ""));
        if (snapshot.isEmpty() && (hadClient || now - startNanos >= STARTUP_IDLE_NANOS)) {
            return true;
        }
        return false;
    }

    /** 校验 scid 并建立客户端会话（校验失败返回 null）。 */
    private ClientConnection acceptSrc(InetSocketAddress src, Hello hello) {
        if (!options.scid.equalsIgnoreCase(hello.getScid())) {
            Server.w("reject hello scid mismatch from " + src + ": " + hello.getScid());
            return null;
        }
        // client_id 从 1 起单调分配；HELLO_ACK 带出，PC 媒体数据报头回带
        int clientId = nextClientId.getAndIncrement();
        hadClient = true;
        ClientConnection client = new ClientConnection(this, src, clientId, hello.getAudio(), options);
        clients.put(src, client);
        clientById.put(clientId, client);
        Server.i("client connected: " + src + " client_id=" + clientId
                + " audio=" + hello.getAudio());
        return client;
    }

    /** 供编码线程/可靠发送把 control datagram 发给指定客户端（走 ctrl socket）。 */
    void sendDatagramRaw(InetSocketAddress dst, byte[] wire) {
        try {
            ctrlSocket.send(new DatagramPacket(wire, wire.length, dst));
        } catch (IOException e) {
            Server.w("udp send to " + dst + " failed: " + e.getMessage());
        }
    }

    /**
     * 发送媒体数据报（25B 头已编码好）：按流选 socket，目标为登记端点。
     * WHY 必须从对应端口发出：PC 端 socket 是 connect() 的，源端口须匹配。
     */
    void sendMediaRaw(int stream, InetSocketAddress dst, byte[] wire) {
        DatagramSocket socket =
                stream == ClientConnection.MEDIA_STREAM_AUDIO ? audioSocket : videoSocket;
        try {
            socket.send(new DatagramPacket(wire, wire.length, dst));
        } catch (IOException e) {
            Server.w("media send to " + dst + " failed: " + e.getMessage());
        }
    }

    /** 绑定到某客户端的发送回调（ReliableControl.Sink 用）。 */
    ReliableControl.Sink sink(InetSocketAddress dst) {
        return wire -> sendDatagramRaw(dst, wire);
    }

    void removeClient(ClientConnection client) {
        clients.remove(client.addr());
        clientById.remove(client.clientId());
    }

    /** 启动一个媒体端口接收守护线程（video/audio 各一）。 */
    private void startMediaLoop(String name, DatagramSocket socket, int stream) {
        Thread t = new Thread(() -> mediaLoop(socket, stream), name);
        t.setDaemon(true);
        t.start();
    }

    /**
     * 媒体端口接收循环：解析 25B 头 → 按 client_id 查会话 → 登记源地址为
     * 该流发送端点并刷新活跃时间。OPEN（frag_total=0xFFFF 空负载）只是首次
     * 注册触发器，后续数据报同样刷新端点；负载不消费（phone 不重装媒体）。
     */
    private void mediaLoop(DatagramSocket socket, int stream) {
        byte[] buf = new byte[MediaDatagram.HEADER_SIZE + ReliableControl.MAX_FRAGMENT];
        while (true) {
            DatagramPacket packet = new DatagramPacket(buf, buf.length);
            try {
                socket.receive(packet);
            } catch (IOException e) {
                Server.w("media receive failed: " + e.getMessage());
                continue;
            }
            int clientId = MediaDatagram.parseClientId(buf, packet.getLength());
            if (clientId < 0) {
                continue; // 头不合法
            }
            ClientConnection client = clientById.get(clientId);
            if (client == null) {
                continue; // 未握手/已拆除的 client_id，忽略
            }
            client.registerMediaEndpoint(stream, (InetSocketAddress) packet.getSocketAddress());
        }
    }

    /** 清扫空闲客户端（watchdog 之外的双保险）。 */
    private void sweepIdle() {
        long now = System.nanoTime();
        // 快照迭代：close() 内会从 clients 移除，HashSet/HashMap 边遍历边改会抛 CME
        java.util.List<ClientConnection> snapshot =
                new java.util.ArrayList<>(clients.values());
        for (ClientConnection c : snapshot) {
            if (now - c.lastActivityNanos() >= SESSION_TIMEOUT_NANOS) {
                c.close(ClientConnection.ClientCloseReason.REASON_IDLE);
            }
        }
    }
}
