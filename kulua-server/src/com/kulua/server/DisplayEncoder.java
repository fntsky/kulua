package com.kulua.server;

import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.view.Surface;
import android.os.Bundle;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.Arrays;
import java.util.concurrent.CountDownLatch;

/**
 * 视频编码器 + 推流（每显示器一个实例）。
 *
 * 流程：MediaCodec(surface 输入) → VirtualDisplay 投屏 → 编码输出 →
 * 媒体端口 UDP 分片推送（25B 定长头）。config 帧（SPS/PPS）走可靠
 * control 流（MediaConfig），普通帧走尽力而为媒体分片（pts/flags 进媒体头）。
 */
public final class DisplayEncoder {


    /** 与 kulua-proto codec.rs 的 MEDIA_FLAG_* 对齐。 */
    static final int FLAG_CONFIG = 1 << 0;
    static final int FLAG_KEY_FRAME = 1 << 1;
    static final int FLAG_SESSION = 1 << 2;

    /**
     * 编码器饿死重发间隔（ms）≈ 30fps。
     *
     * WHY 需要这个：高通 c2.qti.avc.encoder（Codec 2.0）不支持
     * KEY_REPEAT_PREVIOUS_FRAME_AFTER——画面一旦静止（VirtualDisplay 上的 app
     * 不再重绘），Surface 无新帧入队，编码器永久饿死。
     *
     * 方案：缓存最后一帧编码数据，饿死时按 30fps 重发给 viewer（不经过编码器）。
     * 不碰 Surface / BufferQueue——setSurface(null)+reconnect 会 native crash
     * （实测高通平台 SIGABRT，无 Java 日志），VirtualDisplay.resize(±px) 会触发
     * app 重布局有副作用。
     */
    static final long RESEND_INTERVAL_MS = 33;

    private final int id;
    private final ClientConnection client;
    private final DisplayRegistry displayManager;
    private int widthField;
    private int heightField;

    /**
     * 上一个 DATA 帧产出的时刻（elapsedRealtime）。编码线程独占，无需同步。
     * 用于检测编码器饿死（高通 c2.qti.avc.encoder 不支持
     * KEY_REPEAT_PREVIOUS_FRAME_AFTER，画面静止后不再重发上一帧）。
     */
    private long lastFrameMs;
    /** 连续重发计数（编码线程独占）。 */
    private int resendCount;
    /** 最后一帧的编码数据 / flags（重发用；编码线程独占）。 */
    private byte[] lastFrameData;
    private int lastFrameFlags;

    private final Object resizeLock = new Object();
    private int pendingWidth;
    private int pendingHeight;
    private volatile boolean resizeRequested;

    /** VirtualDisplay 首次创建信号（成功 attach / 创建失败都放行，见 createCodec）。 */
    private final CountDownLatch vdReady = new CountDownLatch(1);

    private final int dpi;

    private volatile boolean running;
    private Thread thread;
    private MediaCodec codec;
    private Surface inputSurface;
    private VirtualDisplay virtualDisplay;

    /** 暂存的 codec config（等首帧时发出，见 sendConfig 注释）。 */
    private byte[] pendingConfig;
    /** 编码器累计吐出的 CSD 份数（诊断日志用）。 */
    private int csdSeq;

    public DisplayEncoder(int id, ClientConnection client, DisplayRegistry displayManager,
            int width, int height, int dpi) {
        this.id = id;
        this.client = client;
        this.displayManager = displayManager;
        this.widthField = width;
        this.heightField = height;
        this.pendingWidth = width;
        this.pendingHeight = height;
        this.dpi = dpi;
    }

    /** 启动编码与推流（创建编码器 + VirtualDisplay，独立线程推流）。 */
    public void start() throws IOException {
        running = true;
        resizeRequested = false;
        thread = new Thread(this::encodeLoop, "display-" + id);
        thread.start();
    }

    /** 等待 VirtualDisplay 首次创建完成；返回是否在超时内就绪。
     *  创建失败同样放行（配合 {@link #isRunning()} 区分成败）。 */
    boolean awaitVirtualDisplay(long timeoutMs) {
        try {
            return vdReady.await(timeoutMs, java.util.concurrent.TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return false;
        }
    }

    /** 编码器是否仍在运行（初始创建失败 / stop 后为 false）。 */
    boolean isRunning() {
        return running;
    }

    /** 弹性显示器：调整 VirtualDisplay 尺寸并重启编码器。 */
    public synchronized void resize(int newWidth, int newHeight) {
        synchronized (resizeLock) {
            pendingWidth = newWidth;
            pendingHeight = newHeight;
            resizeRequested = true;
        }
    }

    private void performResize() {
        int newWidth;
        int newHeight;
        synchronized (resizeLock) {
            if (!resizeRequested)
                return;
            newWidth = pendingWidth;
            newHeight = pendingHeight;
            resizeRequested = false;
        }

        releaseCodecForResize();

        widthField = newWidth;
        heightField = newHeight;

        try {
            createCodec();
        } catch (Exception e) {
            Server.e("resize failed #" + id + " " + newWidth + "x" + newHeight, e);
            running = false;
        }
    }

    /** 创建 codec + VirtualDisplay。必须在编码线程调用（MediaCodec 非线程安全）；
     *  resize 复用 VirtualDisplay 实例，仅换 surface。 */
    private void createCodec() throws IOException {
        // 诊断：分步计时——双开第二路时若卡死，能看出卡在 createEncoderByType
        // （codec 服务级 hang）还是 createVirtualDisplay（SurfaceFlinger 侧）
        long t0 = android.os.SystemClock.elapsedRealtime();
        try {
            codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC);
            long t1 = android.os.SystemClock.elapsedRealtime();
            MediaFormat format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC,
                    widthField, heightField);
            format.setInteger(MediaFormat.KEY_BIT_RATE, client.videoBitRate());
            format.setInteger(MediaFormat.KEY_FRAME_RATE, 60);
            // 10s（对齐 scrcpy）：IDR 是最大的带宽突发，2s 一发在多窗口下极易挤爆上行
            format.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 10);
            format.setInteger(MediaFormat.KEY_COLOR_FORMAT,
                    MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface);
            // 静态画面每 100ms 重发上一帧：否则画面一静止编码器就彻底断流，
            // viewer 无法区分「静止」和「卡死」（对齐 scrcpy SurfaceEncoder）
            format.setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 100_000L);
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
            inputSurface = codec.createInputSurface();
            codec.start();
            long t2 = android.os.SystemClock.elapsedRealtime();

            if (virtualDisplay == null) {
                // 首次创建 VirtualDisplay；resize 时复用已有实例（换 surface）。
                try {
                    java.lang.reflect.Constructor<android.hardware.display.DisplayManager> ctor = android.hardware.display.DisplayManager.class
                            .getDeclaredConstructor(android.content.Context.class);
                    ctor.setAccessible(true);
                    android.hardware.display.DisplayManager systemDisplayManager = ctor.newInstance(FakeContext.get());
                    virtualDisplay = systemDisplayManager.createVirtualDisplay(
                            "kulua-" + id, widthField, heightField, dpi, inputSurface,
                            DisplayRegistry.buildFlags());
                } catch (ReflectiveOperationException e) {
                    throw new IOException("cannot create DisplayManager: " + e, e);
                }
                displayManager.attachVirtualDisplay(id, virtualDisplay);
                long t3 = android.os.SystemClock.elapsedRealtime();
                Server.i("createCodec #" + id + " encoder=" + codec.getName()
                        + " new=" + (t1 - t0) + "ms start=" + (t2 - t1) + "ms vd=" + (t3 - t2) + "ms");
            } else {
                // WHY 先 setSurface 再 resize：resize 触发 SurfaceFlinger 重新合成，
                // 若此时 Surface 仍为 null（上轮 releaseCodecForResize 已 detach），
                // 合成事件空跑，之后 setSurface 挂上新 Surface 但不会再触发重绘，
                // 导致编码器饿死（media_msgs 冻结）。先挂 Surface 保证 resize
                // 引发的重绘有目标 Surface 可写入。
                virtualDisplay.setSurface(inputSurface);
                virtualDisplay.resize(widthField, heightField, dpi);
                // 兜底：部分设备 resize 后应用不重绘（无 onConfigurationChanged），
                // 强制请求一个 sync frame 让编码器在 Surface 有内容后立即输出 IDR。
                Bundle syncFrame = new Bundle();
                syncFrame.putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0);
                codec.setParameters(syncFrame);
                Server.i("createCodec #" + id + " (resize) encoder=" + codec.getName()
                        + " new=" + (t1 - t0) + "ms start=" + (t2 - t1) + "ms");
            }
            Server.i("display #" + id + " virtual display id="
                    + virtualDisplay.getDisplay().getDisplayId());
        } catch (IOException | RuntimeException e) {
            Server.e("create codec/display failed #" + id
                    + " at " + (android.os.SystemClock.elapsedRealtime() - t0) + "ms", e);
            releaseCodec();
            throw e;
        } finally {
            // 成功 attach / 创建失败都放行等待方；失败由 running=false 区分
            vdReady.countDown();
        }
    }

    /** 编码输出循环：dequeue → 推流（config 可靠 / 媒体分片）。
     *
     *  饿死自救：部分硬件编码器（实测高通 c2.qti.avc.encoder）不支持
     *  KEY_REPEAT_PREVIOUS_FRAME_AFTER，VirtualDisplay 画面静止后编码器永久
     *  无输出。缓存最后一帧编码数据，超过 RESEND_INTERVAL_MS 未收到新帧时
     *  按 30fps 重发给 viewer，不经过编码器。
     */
    private void encodeLoop() {
        String reason = "stopped";
        try {
            createCodec();
            lastFrameMs = android.os.SystemClock.elapsedRealtime();
            MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
            boolean nullCodecLogged = false;
            while (running) {

                if (resizeRequested) {
                    performResize();
                    // resize 重建了 codec，重置帧时间与缓存帧
                    lastFrameMs = android.os.SystemClock.elapsedRealtime();
                    resendCount = 0;
                    lastFrameData = null;
                }

                if (codec == null) {
                    if (!nullCodecLogged) {
                        nullCodecLogged = true;
                        Server.w("encode loop: codec==null spin #" + id);
                    }
                    continue;
                }

                int index = codec.dequeueOutputBuffer(info, 10_000);
                if (index >= 0) {
                    ByteBuffer buffer = codec.getOutputBuffer(index);
                    if (buffer != null) {
                        boolean config = (info.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0;
                        boolean key = (info.flags & MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0;
                        if (config) {
                            sendConfig(buffer, info.size);
                        } else {
                            int flags = key ? FLAG_KEY_FRAME : 0;
                            sendData(info.presentationTimeUs, flags, buffer, info.size);
                            lastFrameMs = android.os.SystemClock.elapsedRealtime();
                            if (resendCount > 0) {
                                Server.i("encoder recovered #" + id
                                        + " after " + resendCount + " resend(s)");
                                resendCount = 0;
                            }
                        }
                    }
                    codec.releaseOutputBuffer(index, false);
                } else {
                    // dequeueOutputBuffer 超时（TRY_AGAIN_LATER）：检查是否需要重发
                    resendLastFrameIfStarved();
                }
                if ((info.flags & MediaCodec.BUFFER_FLAG_END_OF_STREAM) != 0) {
                    reason = "eos";
                    break;
                }
            }
        } catch (IOException e) {
            reason = "io: " + e.getMessage();
            Server.e("create codec failed #" + id, e);
            running = false;
        } catch (IllegalStateException e) {
            // 正常关闭路径：resize/stop 时 codec.stop() 唤醒阻塞的 dequeueOutputBuffer
            // 并抛 IllegalStateException。
            reason = "codec-stopped: " + e.getMessage();
            Server.i("display #" + id + " codec stopped: " + e.getMessage());
        } catch (RuntimeException e) {
            reason = "runtime: " + e.getMessage();
            Server.i("display #" + id + " encode loop error: " + e.getMessage());
        } finally {
            Server.i("encode loop exited #" + id + " (" + reason + ")");
        }
    }

    /**
     * 编码器饿死自救：超过 RESEND_INTERVAL_MS（≈30fps）无新帧时，把缓存的
     * 最后一帧编码数据直接重发给 viewer（不经过编码器）。
     *
     * PTS 用 System.nanoTime()/1000（与 SurfaceFlinger 同域，单调递增），
     * 解码器不会因 PTS 回退/重复而丢帧。flags 保留原帧的 keyframe 标记，
     * viewer 侧统计不受影响。
     *
     * 不能用 setSurface(null)+setSurface(surface)：实测高通平台 native crash
     * （MediaCodec BufferQueue disconnect/reconnect 时 SIGABRT，无 Java 日志）。
     * 不能用 VirtualDisplay.resize(±px)：会触发 app 重布局，有可见副作用。
     */
    private void resendLastFrameIfStarved() {
        if (lastFrameData == null) {
            return;
        }
        long now = android.os.SystemClock.elapsedRealtime();
        long gap = now - lastFrameMs;
        if (gap < RESEND_INTERVAL_MS) {
            return;
        }
        long pts = System.nanoTime() / 1000;
        client.sendMedia(ClientConnection.MEDIA_STREAM_VIDEO,
                client.nextMediaSeq(), pts, lastFrameFlags, lastFrameData);
        lastFrameMs = now;
        resendCount++;
        if (resendCount == 1 || resendCount % 100 == 0) {
            Server.i("frame resend #" + id + " gap=" + gap
                    + "ms count=" + resendCount + " size=" + lastFrameData.length);
        }
    }

    /** 发送 config 帧（SPS/PPS，可靠 control 流）。
     *
     * WHY 不立即发送：部分编码器（实测 c2.qti.avc.encoder）会分多次吐 CSD，
     * 第一份可能不完整（缺 PPS），viewer 拿它打开解码器后所有帧都报
     * non-existing PPS 0。这里只保留最新一份，等首帧输出时随帧一起发出，
     * 保证 viewer 收到的 config 一定是最终完整版。
     */
    private void sendConfig(ByteBuffer buffer, int size) {
        buffer.position(0);
        buffer.limit(size);
        byte[] data = new byte[size];
        buffer.get(data);
        csdSeq++;
        Server.i("encoder csd #" + id + " seq=" + csdSeq + " size=" + size);
        pendingConfig = data;
    }

    /** 发送普通视频帧（尽力而为、分片）。首帧前先冲刷暂存的 codec config。 */
    private void sendData(long pts, int flags, ByteBuffer buffer, int size) {
        if (pendingConfig != null) {
            client.sendMediaConfig(ClientConnection.MEDIA_STREAM_VIDEO, pendingConfig);
            pendingConfig = null;
        }
        buffer.position(0);
        buffer.limit(size);
        byte[] data = new byte[size];
        buffer.get(data);
        if (data.length != size) {
            data = Arrays.copyOf(data, size);
        }
        client.sendMedia(ClientConnection.MEDIA_STREAM_VIDEO, client.nextMediaSeq(), pts, flags, data);
        // 缓存最后一帧，编码器饿死时 resendLastFrameIfStarved 会按 30fps 重发
        lastFrameData = data;
        lastFrameFlags = flags;
    }

    /** 停止编码器与推流。 */
    public void stop() {
        running = false;
        stopEncoderThread();
    }

    /**
     * 停掉编码线程并释放 codec（stop 路径；resize 走 releaseCodecForResize）。
     * 顺序：先 codec.stop() 唤醒 dequeue（抛 IllegalStateException，已捕获），
     * join 等线程结束，最后 release。
     */
    private void stopEncoderThread() {
        if (thread == null) {
            releaseCodec();
            return;
        }
        try {
            if (codec != null) {
                codec.stop();
            }
        } catch (Exception ignored) {
            // ignore
        }
        try {
            thread.join(2000);
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        }
        thread = null;
        releaseCodec();
    }

    private void releaseCodec() {
        try {
            if (codec != null) {
                codec.stop();
            }
        } catch (Exception ignored) {
            // ignore
        }
        try {
            if (codec != null) {
                codec.release();
            }
        } catch (Exception ignored) {
            // ignore
        }
        codec = null;
        if (inputSurface != null) {
            inputSurface.release();
            inputSurface = null;
        }
    }

    private void releaseCodecForResize() {
        if (virtualDisplay != null) {
            try {
                virtualDisplay.setSurface(null);
            } catch (Exception e) {
                Server.w("detach surface failed #" + id, e);
            }
        }
        if (codec != null) {
            try {
                codec.stop();
            } catch (Exception e) {
                Server.w("codec.stop failed #" + id, e);
            }
            // 必须 release 释放硬件编码器 slot——否则 createCodec 新建实例后
            // 老 codec 泄漏，高通 SoC 上硬件编码器数量有限（通常 4-8 路），
            // 泄漏后新 codec 可能拿不到硬件资源或行为异常
            try {
                codec.release();
            } catch (Exception e) {
                Server.w("codec.release failed #" + id, e);
            }
            codec = null;
        }

        if (inputSurface != null) {
            try {
                inputSurface.release();
            } catch (Exception e) {
                Server.w("surface.release failed #" + id, e);
            }
            inputSurface = null;
        }
    }
}
