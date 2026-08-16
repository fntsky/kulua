package com.kulua.server;

import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.net.LocalSocket;
import android.os.SystemClock;
import android.util.Log;
import android.view.Surface;

import java.io.IOException;
import java.io.OutputStream;
import java.nio.ByteBuffer;

/**
 * 视频编码器 + 推流（每显示器一个实例）。
 *
 * 流程：MediaCodec(surface 输入) → VirtualDisplay 投屏 → 编码输出 → video socket。
 * 流格式（沿用 scrcpy）：先写 4B codec id（"h264"），再写 12B 帧头 + payload：
 * - 帧头：8B pts/flags（bit62=config，bit61=keyframe，pts 低 61 位）+ 4B size（大端）
 */
public final class DisplayEncoder {

    private static final String TAG = "kulua-server";

    private static final long FLAG_CONFIG = 1L << 62;
    private static final long FLAG_KEY_FRAME = 1L << 61;

    private final int id;
    private final LocalSocket socket;
    private final DisplayRegistry displayManager;
    private int widthField;
    private int heightField;
    private final int dpi;

    private volatile boolean running;
    private Thread thread;
    private MediaCodec codec;
    private Surface inputSurface;
    private VirtualDisplay virtualDisplay;

    public DisplayEncoder(int id, LocalSocket socket, DisplayRegistry displayManager,
                          int width, int height, int dpi) {
        this.id = id;
        this.socket = socket;
        this.displayManager = displayManager;
        this.widthField = width;
        this.heightField = height;
        this.dpi = dpi;
    }

    /** 启动编码与推流（创建编码器 + VirtualDisplay，独立线程推流）。 */
    public void start() throws IOException {
        running = true;
        createCodec();
        thread = new Thread(this::encodeLoop, "display-" + id);
        thread.start();
    }

    /** 弹性显示器：调整 VirtualDisplay 尺寸并重启编码器。 */
    public synchronized void resize(int newWidth, int newHeight) {
        widthField = newWidth;
        heightField = newHeight;
        running = false;
        if (thread != null) {
            try {
                thread.join(2000);
            } catch (InterruptedException ignored) {
                // ignore
            }
        }
        releaseCodec();
        try {
            createCodec();
            running = true;
            thread = new Thread(this::encodeLoop, "display-" + id);
            thread.start();
        } catch (IOException e) {
            Log.e(TAG, "resize restart failed #" + id, e);
        }
    }

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
                // 首次创建 VirtualDisplay；resize 时复用已有实例（换 surface）
                android.hardware.display.DisplayManager systemDisplayManager =
                        (android.hardware.display.DisplayManager) Clipboard.getContext()
                                .getSystemService(android.content.Context.DISPLAY_SERVICE);
                virtualDisplay = systemDisplayManager.createVirtualDisplay(
                        "kulua-" + id, widthField, heightField, dpi, inputSurface,
                        DisplayRegistry.buildFlags());
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
        }
    }

    /** 编码输出循环：dequeue → 写 video socket（scrcpy 帧格式）。 */
    private void encodeLoop() {
        try {
            OutputStream output = socket.getOutputStream();
            MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
            // 先写 codec id（4B ASCII "h264"，v4.0 格式）
            output.write(new byte[]{0x68, 0x32, 0x36, 0x34});
            output.flush();

            while (running) {
                int index = codec.dequeueOutputBuffer(info, 10_000);
                if (index >= 0) {
                    ByteBuffer buffer = codec.getOutputBuffer(index);
                    if (buffer != null) {
                        boolean config = (info.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0;
                        boolean key = (info.flags & MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0;
                        long pts = info.presentationTimeUs;
                        long ptsAndFlags = config ? FLAG_CONFIG : pts;
                        if (key) {
                            ptsAndFlags |= FLAG_KEY_FRAME;
                        }
                        writeFrame(output, ptsAndFlags, buffer, info.size);
                    }
                    codec.releaseOutputBuffer(index, false);
                }
                if ((info.flags & MediaCodec.BUFFER_FLAG_END_OF_STREAM) != 0) {
                    break;
                }
            }
        } catch (IOException e) {
            Log.i(TAG, "display #" + id + " stream ended: " + e.getMessage());
        } finally {
            Log.i(TAG, "display #" + id + " encode loop exited");
        }
    }

    private static void writeFrame(OutputStream output, long ptsAndFlags,
                                   ByteBuffer buffer, int size) throws IOException {
        byte[] header = new byte[12];
        header[0] = (byte) (ptsAndFlags >>> 56);
        header[1] = (byte) (ptsAndFlags >>> 48);
        header[2] = (byte) (ptsAndFlags >>> 40);
        header[3] = (byte) (ptsAndFlags >>> 32);
        header[4] = (byte) (ptsAndFlags >>> 24);
        header[5] = (byte) (ptsAndFlags >>> 16);
        header[6] = (byte) (ptsAndFlags >>> 8);
        header[7] = (byte) ptsAndFlags;
        header[8] = (byte) (size >>> 24);
        header[9] = (byte) (size >>> 16);
        header[10] = (byte) (size >>> 8);
        header[11] = (byte) size;
        output.write(header);
        buffer.position(0);
        buffer.limit(size);
        byte[] payload = new byte[size];
        buffer.get(payload);
        output.write(payload);
    }

    /** 停止编码器与推流。 */
    public void stop() {
        running = false;
        if (thread != null) {
            try {
                thread.join(2000);
            } catch (InterruptedException ignored) {
                // ignore
            }
        }
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
}
