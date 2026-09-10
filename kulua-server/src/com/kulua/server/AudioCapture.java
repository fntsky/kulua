package com.kulua.server;

import android.media.AudioRecord;
import android.media.MediaCodec;
import android.media.MediaFormat;
import android.os.Build;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.Arrays;

/**
 * 音频捕获（系统播放声音，shell 权限下无需 MediaProjection 授权）。
 *
 * 实现对照 scrcpy AudioPlaybackCapture：反射构造 AudioPolicy + AudioMix
 * （ROUTE_FLAG_LOOP_BACK，构造见 {@link LoopbackRecorder}），AudioRecord
 * 读取混合后的 PCM。
 *
 * 输出（直接 UDP proto）：
 * - 编码：opus / aac / flac：AudioRecord PCM → MediaCodec 编码
 *   - config 包（AAC AudioSpecificConfig / FLAC STREAMINFO）→ 可靠 control 流
 *     （MediaConfig），确保解码器可在首帧前初始化
 *   - 普通帧 → 尽力而为媒体端口分片（33B 定长头，pts/revision 进媒体头）
 * - raw：PCM 直通（S16LE 48kHz 立体声，20ms 一帧），无 config
 */
public final class AudioCapture {


    private static final int SAMPLE_RATE = 48000;
    private static final int CHANNELS = 2;
    private static final int FRAME_MS = 20;
    private static final int FRAME_BYTES = SAMPLE_RATE * CHANNELS * 2 * FRAME_MS / 1000;

    /** 连续多久没送出音频帧判定为停摆（重建采集器 + 编码器）。 */
    private static final long STALL_TIMEOUT_NANOS = 5_000_000_000L;
    /** 重建前的退避，避免采集器持续不可用时空转刷日志。 */
    private static final long RESTART_BACKOFF_MS = 1000;
    /** 失败/重试日志限频（重建循环里会反复失败）。 */
    private static final long LOG_INTERVAL_NANOS = 5_000_000_000L;

    private final String codecName;
    private final int bitRate;
    /** 本采集器的音频代次（= 下发本配置的 SetAudio.revision）；媒体帧/config 都带上它。 */
    private final long revision;
    private final ClientConnection client;
    private volatile boolean running;
    private Thread thread;
    private volatile AudioRecord recorder;
    private MediaCodec codec;
    /** 当前采集器的构造器（持有 loopback policy，releaseAll 注销）。 */
    private LoopbackRecorder loopback;
    /** 最近一次成功送出音频帧的时刻（停摆看门狗基准）。 */
    private volatile long lastFrameNanos;
    /** 连续失败次数，送出首帧后清零（日志里区分「首次」与「持续」）。 */
    private int consecutiveFailures;
    /** 连续 read() 失败次数：瞬时失败只重试，不终止采集。 */
    private int consecutiveReadErrors;
    private long lastFailureLogNanos;
    private long lastReadWarnNanos;

    public AudioCapture(String codecName, int bitRate, long revision, ClientConnection client) {
        this.codecName = codecName;
        this.bitRate = bitRate;
        this.revision = revision;
        this.client = client;
    }

    public void start() {
        running = true;
        lastFrameNanos = System.nanoTime();
        thread = new Thread(this::runLoop, "audio-capture");
        thread.start();
    }

    /**
     * 采集线程主循环：单次采集的任何中断（异常 / 输入停摆）都重建采集器重试，
     * 直到 {@link #stop()} 被调用。
     *
     * WHY 不能像旧实现那样直接退出线程：AudioRecord 与 MediaCodec 会因平台侧
     * 音频动作失效（REMOTE_SUBMIX 被收走、蓝牙/锁屏改路由、编码器进入错误态），
     * 一次失败就永久静音，且 PC 端只表现为「没有声音 + 缓冲数字不动」，无自愈。
     */
    private void runLoop() {
        while (running) {
            try {
                captureLoop();
            } catch (IOException | RuntimeException e) {
                if (running) {
                    logFailure(e);
                }
            } finally {
                releaseAll();
            }
            if (!running) {
                break;
            }
            sleepBeforeRestart();
        }
        running = false;
    }

    /** 单次采集：建采集器 + 编码器，跑到异常或停摆为止。 */
    private void captureLoop() throws IOException {
        // 采集器构造（REMOTE_SUBMIX 优先，失败回退反射 loopback）与 policy 见 LoopbackRecorder
        loopback = new LoopbackRecorder();
        recorder = loopback.create(SAMPLE_RATE, FRAME_BYTES);
        if (!running) {
            return;
        }
        recorder.startRecording();
        if (recorder.getRecordingState() != AudioRecord.RECORDSTATE_RECORDING) {
            throw new IOException("AudioRecord did not enter recording state");
        }
        Server.i("audio capture started: codec=" + codecName + " sdk=" + Build.VERSION.SDK_INT);
        // 业务回执：采集器真的起来了（PC 的 AudioState 只认这条，不认传输 ACK）
        client.onAudioCaptureState(revision, true, null);
        if ("raw".equals(codecName)) {
            rawLoop();
        } else {
            encodeLoop();
        }
    }

    /** 停摆看门狗：连续 {@link #STALL_TIMEOUT_NANOS} 没送出帧即判定采集已失效。 */
    private void checkStall() throws IOException {
        long idle = System.nanoTime() - lastFrameNanos;
        if (idle >= STALL_TIMEOUT_NANOS) {
            throw new IOException("audio stalled for " + idle / 1_000_000 + "ms (no frame sent)");
        }
    }

    /** 记录一次真实进展（成功送出音频帧）：重置停摆计时与失败计数。 */
    private void noteFrameSent() {
        lastFrameNanos = System.nanoTime();
        consecutiveReadErrors = 0;
        if (consecutiveFailures > 0) {
            Server.i("audio capture recovered after " + consecutiveFailures + " failure(s)");
            consecutiveFailures = 0;
            // 自愈成功：真实帧已送出，回执给 PC（同一代次，PC 从失败态翻回运行态）
            client.onAudioCaptureState(revision, true, null);
        }
    }

    /** 采集失败日志（首次必打，之后每 {@link #LOG_INTERVAL_NANOS} 一条）。 */
    private void logFailure(Exception e) {
        consecutiveFailures++;
        long now = System.nanoTime();
        if (consecutiveFailures == 1) {
            // 每轮失败只回执一次，避免重建循环刷爆控制流。
            // WHY 兜底类名：异常 message 可能为 null，而空 error 在 PC 侧等于
            // "正常关闭"，会把失败误报成 off。
            String msg = e.getMessage();
            if (msg == null || msg.isEmpty()) {
                msg = e.getClass().getSimpleName();
            }
            client.onAudioCaptureState(revision, false, msg);
        }
        if (consecutiveFailures == 1 || now - lastFailureLogNanos >= LOG_INTERVAL_NANOS) {
            lastFailureLogNanos = now;
            Server.e("audio capture failed (" + consecutiveFailures + "x), restarting: "
                    + e.getMessage(), e);
        }
    }

    /**
     * 单次 read() 失败（负数返回，如 ERROR_DEAD_OBJECT / ERROR_INVALID_OPERATION）：
     * 只记日志并重试，连续失败时轻微退避——瞬时 HAL/路由重配不该终止采集，
     * 持续失败由停摆看门狗触发重建。
     */
    private void warnReadError(int code) {
        if (!running) {
            return; // stop() 唤醒阻塞 read 造成的失败是预期行为，不进日志
        }
        consecutiveReadErrors++;
        long now = System.nanoTime();
        if (consecutiveReadErrors == 1 || now - lastReadWarnNanos >= LOG_INTERVAL_NANOS) {
            lastReadWarnNanos = now;
            Server.w("AudioRecord.read failed: " + code + " ("
                    + consecutiveReadErrors + " in a row, retrying)");
        }
        if (consecutiveReadErrors > 10) {
            try {
                Thread.sleep(10);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    /** 重建前退避（可被 stop() 打断：sleep 期间 running 变 false 即退出）。 */
    private void sleepBeforeRestart() {
        try {
            Thread.sleep(RESTART_BACKOFF_MS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    /** raw：PCM 直通（S16LE 48kHz 立体声），20ms 一帧。 */
    private void rawLoop() throws IOException {
        byte[] frame = new byte[FRAME_BYTES];
        long pts = 0;
        while (running) {
            checkStall();
            int read = recorder.read(frame, 0, frame.length);
            if (read < 0) {
                warnReadError(read);
                continue;
            }
            if (read == 0) {
                continue;
            }
            sendData(pts, frame, read);
            pts += read * 1_000_000L / (SAMPLE_RATE * CHANNELS * 2);
        }
    }

    /**
     * 编码循环（opus / aac / flac）：AudioRecord PCM → MediaCodec → 帧写出。
     *
     * 单线程轮询：dequeueInputBuffer 写 PCM，dequeueOutputBuffer 读编码结果；
     * 编码器输出首个包通常是 config 包（AAC AudioSpecificConfig / FLAC
     * STREAMINFO），走可靠 MediaConfig；随后普通帧走媒体分片。
     */
    private void encodeLoop() throws IOException {
        MediaFormat format = createEncoderFormat();
        codec = MediaCodec.createEncoderByType(getMimeType());
        codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
        codec.start();

        MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
        ByteBuffer pcmBuffer = ByteBuffer.allocateDirect(FRAME_BYTES);
        long pts = 0;
        while (running) {
            checkStall();
            int inIndex = codec.dequeueInputBuffer(10_000);
            if (inIndex >= 0) {
                pcmBuffer.clear();
                ByteBuffer input = codec.getInputBuffer(inIndex);
                input.clear();
                int readSize = Math.min(FRAME_BYTES, input.remaining()) / (CHANNELS * 2) * (CHANNELS * 2);
                int read = recorder.read(pcmBuffer, readSize);
                if (read > 0) {
                    pcmBuffer.position(0);
                    pcmBuffer.limit(read);
                    input.put(pcmBuffer);
                    codec.queueInputBuffer(inIndex, 0, read, pts, 0);
                    pts += read * 1_000_000L / (SAMPLE_RATE * CHANNELS * 2);
                } else {
                    // 空/失败读取：仍须归还输入缓冲，否则编码器输入槽耗尽后彻底停摆
                    if (read < 0) {
                        warnReadError(read);
                    }
                    codec.queueInputBuffer(inIndex, 0, 0, pts, 0);
                }
            }

            int outIndex;
            while (running && (outIndex = codec.dequeueOutputBuffer(info, 0)) >= 0) {
                ByteBuffer encoded = codec.getOutputBuffer(outIndex);
                if (encoded != null) {
                    encoded.position(info.offset);
                    encoded.limit(info.offset + info.size);
                    boolean config = (info.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0;
                    if (config) {
                        sendConfig(encoded, info.size);
                    } else {
                        sendData(info.presentationTimeUs, encoded, info.size);
                    }
                }
                codec.releaseOutputBuffer(outIndex, false);
            }
        }
    }

    /** 编码器 MediaFormat：MIME + 码率 + 48kHz 立体声。 */
    private MediaFormat createEncoderFormat() {
        MediaFormat format = new MediaFormat();
        format.setString(MediaFormat.KEY_MIME, getMimeType());
        format.setInteger(MediaFormat.KEY_BIT_RATE, bitRate > 0 ? bitRate : 128_000);
        format.setInteger(MediaFormat.KEY_CHANNEL_COUNT, CHANNELS);
        format.setInteger(MediaFormat.KEY_SAMPLE_RATE, SAMPLE_RATE);
        return format;
    }

    private String getMimeType() {
        switch (codecName) {
            case "opus": return MediaFormat.MIMETYPE_AUDIO_OPUS;
            case "aac": return MediaFormat.MIMETYPE_AUDIO_AAC;
            case "flac": return MediaFormat.MIMETYPE_AUDIO_FLAC;
            default: return MediaFormat.MIMETYPE_AUDIO_RAW;
        }
    }

    /** 发送普通音频帧（尽力而为、分片；带本采集器代次）。 */
    private void sendData(long pts, byte[] payload, int size) {
        byte[] data = size == payload.length ? payload : Arrays.copyOf(payload, size);
        client.sendMedia(ClientConnection.MEDIA_STREAM_AUDIO, client.nextMediaSeq(), pts, revision, 0,
                data);
        noteFrameSent();
    }

    private void sendData(long pts, ByteBuffer payload, int size) {
        byte[] data = new byte[size];
        payload.get(data);
        sendData(pts, data, size);
    }

    /** 发送 codec config 帧（可靠 control 流；带本采集器代次）。 */
    private void sendConfig(ByteBuffer payload, int size) {
        byte[] data = new byte[size];
        payload.get(data);
        client.sendMediaConfig(ClientConnection.MEDIA_STREAM_AUDIO, data, revision);
    }

    private void releaseAll() {
        AudioRecord current = recorder;
        recorder = null;
        if (current != null) {
            // stop() 与 release() 分开 try：stop() 抛（重复 stop / 未初始化）时
            // 仍必须 release，否则采集器不释放，r_submix input 一直被占，
            // 下一次采集拿不到声音。
            try {
                current.stop();
            } catch (RuntimeException ignored) {
                // 可能已由 stop() 或捕获线程停过
            }
            try {
                current.release();
            } catch (RuntimeException e) {
                Server.w("AudioRecord.release failed", e);
            }
        }
        MediaCodec currentCodec = codec;
        codec = null;
        if (currentCodec != null) {
            try {
                currentCodec.stop();
            } catch (RuntimeException ignored) {
                // 编码器可能已进入错误态
            }
            try {
                currentCodec.release();
            } catch (RuntimeException e) {
                Server.w("MediaCodec.release failed", e);
            }
        }
        LoopbackRecorder currentLoopback = loopback;
        loopback = null;
        if (currentLoopback != null) {
            currentLoopback.release();
        }
    }

    public void stop() {
        running = false;
        // 采集线程可能正卡在重建退避的 sleep 里：打断它，避免 stop 后仍多活 1s
        // （两个 AudioRecord 短暂并存会抢同一路 r_submix input）。
        if (thread != null) {
            thread.interrupt();
        }
        // read() 是阻塞调用，先 stop 唤醒它，再等待线程释放采集器和路由。
        AudioRecord current = recorder;
        if (current != null) {
            try {
                current.stop();
            } catch (IllegalStateException ignored) {
                // 采集器可能仍在初始化或已经由捕获线程释放。
            }
        }
        if (thread != null) {
            try {
                thread.join(2000);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
        }
    }
}
