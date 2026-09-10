package com.kulua.server;

import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;

import kulua.direct.CtrlMsg;

/**
 * 单连接的音频生命周期控制面（从 ClientConnection 拆出，纯搬家）。
 *
 * 职责：串行执行 SetAudio 启停、维护当前代次/编码，并把采集线程的就绪 / 失败 /
 * 自愈回执转成 AudioState 发回 PC。
 *
 * 启停串行在专用单线程执行器上：join 采集线程（最长 ~2s）跑在 ctrl 线程上会阻塞
 * 所有客户端的心跳巡检与输入注入。
 */
final class AudioLink {

    /** 所属会话：回执走 sendCtrlMsg，采集器以它作为媒体发送出口。 */
    private final ClientConnection client;
    /** 音频编码码率（Options.audioBitRate，采集器创建用）。 */
    private final int bitRate;

    /**
     * 音频生命周期执行器（单线程）：启停要 join 采集线程（最长 ~2s），跑在 ctrl
     * 线程上会阻塞所有客户端的心跳巡检与输入注入，必须串行挪到专用线程。
     * 它同时是所有 SetAudio 的串行化点：连续命令按到达顺序执行，代次更大的生效。
     */
    private final ExecutorService executor;
    /** 已执行的音频代次（SetAudio.revision；初始态为 0 = HELLO 带出的会话初始配置）。 */
    private volatile long currentRevision;
    /** 当前音频是否启用（诊断日志用；真实状态以采集器回执为准）。 */
    private volatile boolean enabled;
    /** 当前音频编码名（AudioState.codec_id 由它推导）。 */
    private volatile String codecName;
    /** 当前采集器（null = 未启用 / 已停止）。 */
    private AudioCapture capture;

    AudioLink(ClientConnection client, boolean wantAudio, int bitRate, String codecName) {
        this.client = client;
        this.bitRate = bitRate;
        this.executor = Executors.newSingleThreadExecutor(r -> {
            Thread t = new Thread(r, "audio-lifecycle-" + client.addr().getPort());
            t.setDaemon(true);
            return t;
        });
        this.currentRevision = 0;
        this.enabled = wantAudio;
        this.codecName = codecName;
    }

    /**
     * 启动 HELLO 带出的会话初始采集器（代次 0，不可用时为空操作）。
     *
     * WHY 不在构造里启动：采集线程的就绪回执要经 {@link ClientConnection} 转发，
     * 构造期间该会话的 audio 字段尚未赋值（回执会打到 null）。
     */
    void start() {
        if (!enabled) {
            return;
        }
        capture = new AudioCapture(codecName, bitRate, 0, client);
        capture.start();
    }

    /**
     * 应用音频目标（SetAudio）：开关 + 编码 + 代次一起。
     *
     * 串行在 {@link #executor} 上执行（启停要 join 采集线程，不能占 ctrl 线程）。
     * 代次 `<=` 当前值的命令直接丢弃：PC 每次重试都会下发更大的代次，因此重复投递
     * （可靠层重传）天然幂等，而重试一定生效。
     */
    void applyAudio(boolean enabled, int codecIndex, long revision) {
        String name = ControlChannel.codecNameForIndex(codecIndex);
        try {
            executor.execute(() -> {
                if (client.isClosed() || revision <= currentRevision) {
                    return;
                }
                stopAudio();
                currentRevision = revision;
                this.enabled = enabled;
                this.codecName = name;
                if (!enabled) {
                    Server.i("audio off rev=" + revision + " (" + client.addr() + ")");
                    sendAudioState(revision, false, 0, null);
                    return;
                }
                Server.i("audio on rev=" + revision + " codec=" + name + " (" + client.addr() + ")");
                // 回执由采集线程在真正就绪/失败时发（传输 ACK 不代表启停完成）
                capture = new AudioCapture(name, bitRate, revision, client);
                capture.start();
            });
        } catch (RejectedExecutionException ignored) {
            // 会话已关闭：忽略
        }
    }

    /** 停止并释放当前采集器（只允许在 {@link #executor} 上调用）。 */
    private void stopAudio() {
        AudioCapture current = capture;
        capture = null;
        if (current != null) {
            current.stop();
        }
    }

    /**
     * 采集线程的音频状态变化（就绪 / 失败 / 自愈恢复）→ 转成 AudioState 回执。
     *
     * 只转发当前代次：被 SetAudio 取代的旧采集器，其迟到回执必须丢弃。
     */
    void onAudioCaptureState(long revision, boolean enabled, String error) {
        if (revision != currentRevision || client.isClosed()) {
            return;
        }
        int codecId = enabled ? ClientConnection.codecIdFor(codecName) : 0;
        sendAudioState(revision, enabled, codecId, error);
    }

    /** 发送音频状态回执（业务结果；`error` 非空表示本次启用失败）。 */
    private void sendAudioState(long revision, boolean enabled, int codecId, String error) {
        client.sendCtrlMsg(CtrlMsg.newBuilder()
                .setAudioState(kulua.direct.AudioState.newBuilder()
                        .setRevision(revision)
                        .setEnabled(enabled)
                        .setCodecId(codecId)
                        .setError(error == null ? "" : error))
                .build());
    }

    /** 诊断日志用：当前音频是否启用。 */
    boolean enabled() {
        return enabled;
    }

    /** 诊断日志用：已执行的音频代次。 */
    long revision() {
        return currentRevision;
    }

    /** 会话关闭时的音频收尾：排在在途切换之后（可能正在重启采集），且不阻塞 ctrl/看门狗线程。 */
    void close() {
        try {
            executor.execute(this::stopAudio);
        } catch (RejectedExecutionException ignored) {
            // 执行器已关闭
        }
        executor.shutdown();
        enabled = false;
    }
}
