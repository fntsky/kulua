package com.kulua.server;

import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.util.Log;
import android.view.Surface;

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

    private static final String TAG = "kulua-server";

    /** 与 kulua-proto codec.rs 的 MEDIA_FLAG_* 对齐。 */
    static final int FLAG_CONFIG = 1 << 0;
    static final int FLAG_KEY_FRAME = 1 << 1;
    static final int FLAG_SESSION = 1 << 2;

    private final int id;
    private final ClientConnection client;
    private final DisplayRegistry displayManager;
    private int widthField;
    private int heightField;

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
            Log.e(TAG, "resize failed #" + id + " " + newWidth + "x" + newHeight, e);
            running = false;
        }
    }

    /** 创建 codec + VirtualDisplay。必须在编码线程调用（MediaCodec 非线程安全）；
     *  resize 复用 VirtualDisplay 实例，仅换 surface。 */
    private void createCodec() throws IOException {
        try {
            codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC);
            MediaFormat format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC,
                    widthField, heightField);
            format.setInteger(MediaFormat.KEY_BIT_RATE, 8_000_000);
            format.setInteger(MediaFormat.KEY_FRAME_RATE, 60);
            format.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2);
            format.setInteger(MediaFormat.KEY_COLOR_FORMAT,
                    MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface);
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
            inputSurface = codec.createInputSurface();
            codec.start();

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
            } else {
                virtualDisplay.resize(widthField, heightField, dpi);
                virtualDisplay.setSurface(inputSurface);
            }
            Log.i(TAG, "display #" + id + " virtual display id="
                    + virtualDisplay.getDisplay().getDisplayId());
        } catch (IOException | RuntimeException e) {
            Log.e(TAG, "create codec/display failed #" + id, e);
            releaseCodec();
            throw e;
        } finally {
            // 成功 attach / 创建失败都放行等待方；失败由 running=false 区分
            vdReady.countDown();
        }
    }

    /** 编码输出循环：dequeue → 推流（config 可靠 / 媒体分片）。 */
    private void encodeLoop() {
        try {
            createCodec();
            MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
            while (running) {

                if (resizeRequested) {
                    performResize();
                }

                if (codec == null) {
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
                        }
                    }
                    codec.releaseOutputBuffer(index, false);
                }
                if ((info.flags & MediaCodec.BUFFER_FLAG_END_OF_STREAM) != 0) {
                    break;
                }
            }
        } catch (IOException e) {
            Log.e(TAG, "create codec failed #" + id, e);
            running = false;
        } catch (IllegalStateException e) {
            // 正常关闭路径：resize/stop 时 codec.stop() 唤醒阻塞的 dequeueOutputBuffer
            // 并抛 IllegalStateException。
            Log.i(TAG, "display #" + id + " codec stopped: " + e.getMessage());
        } catch (RuntimeException e) {
            Log.i(TAG, "display #" + id + " encode loop error: " + e.getMessage());
        } finally {
            Log.i(TAG, "display #" + id + " encode loop exited");
        }
    }

    /** 发送 config 帧（SPS/PPS，可靠 control 流）。 */
    private void sendConfig(ByteBuffer buffer, int size) {
        buffer.position(0);
        buffer.limit(size);
        byte[] data = new byte[size];
        buffer.get(data);
        client.sendMediaConfig(ClientConnection.MEDIA_STREAM_VIDEO, data);
    }

    /** 发送普通视频帧（尽力而为、分片）。 */
    private void sendData(long pts, int flags, ByteBuffer buffer, int size) {
        buffer.position(0);
        buffer.limit(size);
        byte[] data = new byte[size];
        buffer.get(data);
        if (data.length != size) {
            data = Arrays.copyOf(data, size);
        }
        client.sendMedia(ClientConnection.MEDIA_STREAM_VIDEO, client.nextMediaSeq(), pts, flags, data);
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
                Log.w(TAG, "detach surface failed #" + id, e);
            }
        }
        if (codec != null) {
            try {
                codec.stop();
            } catch (Exception e) {
                Log.w(TAG, "codec.stop failed #" + id, e);
            }
        }

        if (inputSurface != null) {
            try {
                inputSurface.release();
            } catch (Exception e) {
                Log.w(TAG, "surface.release failed #" + id, e);
            }
            inputSurface = null;
        }
    }
}
