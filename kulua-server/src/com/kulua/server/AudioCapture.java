package com.kulua.server;

import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioRecord;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.util.Log;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.Arrays;

import kulua.direct.Frame;

/**
 * 音频捕获（系统播放声音，shell 权限下无需 MediaProjection 授权）。
 *
 * 实现对照 scrcpy AudioPlaybackCapture：反射构造 AudioPolicy + AudioMix
 * （ROUTE_FLAG_LOOP_BACK），AudioRecord 读取混合后的 PCM。
 *
 * 输出（直接 UDP proto）：
 * - 编码：opus / aac / flac：AudioRecord PCM → MediaCodec 编码
 *   - config 包（AAC AudioSpecificConfig / FLAC STREAMINFO）→ 可靠 control 流
 *     （MediaConfig），确保解码器可在首帧前初始化
 *   - 普通帧 → 尽力而为媒体分片（Frame{stream=AUDIO, media_pts, media_flags, data}）
 * - raw：PCM 直通（S16LE 48kHz 立体声，20ms 一帧），无 config
 */
public final class AudioCapture {

    private static final String TAG = "kulua-server";

    private static final int SAMPLE_RATE = 48000;
    private static final int CHANNELS = 2;
    private static final int FRAME_MS = 20;
    private static final int FRAME_BYTES = SAMPLE_RATE * CHANNELS * 2 * FRAME_MS / 1000;

    private final String codecName;
    private final int bitRate;
    private final ClientConnection client;
    private volatile boolean running;
    private Thread thread;
    private AudioRecord recorder;
    private MediaCodec codec;

    public AudioCapture(String codecName, int bitRate, ClientConnection client) {
        this.codecName = codecName;
        this.bitRate = bitRate;
        this.client = client;
    }

    public void start() {
        running = true;
        thread = new Thread(this::captureLoop, "audio-capture");
        thread.start();
    }

    private void captureLoop() {
        try {
            recorder = createLoopbackRecorder();
            recorder.startRecording();
            if ("raw".equals(codecName)) {
                rawLoop();
            } else {
                encodeLoop();
            }
        } catch (IOException | RuntimeException e) {
            if (running) {
                Log.e(TAG, "audio capture ended: " + e.getMessage());
            }
        } finally {
            releaseAll();
            running = false;
        }
    }

    /** raw：PCM 直通（S16LE 48kHz 立体声），20ms 一帧。 */
    private void rawLoop() {
        byte[] frame = new byte[FRAME_BYTES];
        long pts = 0;
        while (running) {
            int read = recorder.read(frame, 0, frame.length);
            if (read <= 0) {
                continue;
            }
            sendData(pts, frame, read);
            pts += FRAME_MS * 1000L;
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
        try {
            MediaFormat format = createEncoderFormat();
            codec = MediaCodec.createEncoderByType(getMimeType());
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
            codec.start();

            MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
            ByteBuffer pcmBuffer = ByteBuffer.allocateDirect(FRAME_BYTES);
            long pts = 0;
            while (running) {
                int inIndex = codec.dequeueInputBuffer(10_000);
                if (inIndex >= 0) {
                    pcmBuffer.clear();
                    int read = recorder.read(pcmBuffer, FRAME_BYTES);
                    if (read > 0) {
                        pcmBuffer.rewind();
                        ByteBuffer input = codec.getInputBuffer(inIndex);
                        input.clear();
                        input.put(pcmBuffer);
                        codec.queueInputBuffer(inIndex, 0, read, pts, 0);
                        pts += FRAME_MS * 1000L;
                    } else {
                        codec.queueInputBuffer(inIndex, 0, 0, pts, 0);
                    }
                }

                int outIndex;
                while (running && (outIndex = codec.dequeueOutputBuffer(info, 0)) >= 0) {
                    ByteBuffer encoded = codec.getOutputBuffer(outIndex);
                    if (encoded != null) {
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
        } finally {
            if (codec != null) {
                codec.stop();
                codec.release();
                codec = null;
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

    /** 发送普通音频帧（尽力而为、分片）。 */
    private void sendData(long pts, byte[] payload, int size) {
        byte[] data = size == payload.length ? payload : Arrays.copyOf(payload, size);
        client.sendMedia(Frame.Stream.AUDIO.getNumber(), client.nextMediaSeq(), pts, 0, data);
    }

    private void sendData(long pts, ByteBuffer payload, int size) {
        payload.rewind();
        byte[] data = new byte[size];
        payload.get(data);
        client.sendMedia(Frame.Stream.AUDIO.getNumber(), client.nextMediaSeq(), pts, 0, data);
    }

    /** 发送 codec config 帧（可靠 control 流）。 */
    private void sendConfig(ByteBuffer payload, int size) {
        payload.rewind();
        byte[] data = new byte[size];
        payload.get(data);
        client.sendMediaConfig(Frame.Stream.AUDIO.getNumber(), data);
    }

    /**
     * 创建 loopback AudioRecord（反射 AudioPolicy，对照 scrcpy AudioPlaybackCapture
     * 的完整流程）：AudioMixingRule + AudioMix(ROUTE_FLAG_LOOP_BACK) +
     * AudioPolicy.Builder(FakeContext) + registerAudioPolicyStatic +
     * createAudioRecordSink。
     */
    @SuppressWarnings("unchecked")
    private static AudioRecord createLoopbackRecorder() throws IOException {
        try {
            Class<?> audioMixingRuleClass = Class.forName("android.media.audiopolicy.AudioMixingRule");
            Class<?> audioMixingRuleBuilderClass = Class.forName("android.media.audiopolicy.AudioMixingRule$Builder");
            Class<?> audioMixClass = Class.forName("android.media.audiopolicy.AudioMix");
            Class<?> audioMixBuilderClass = Class.forName("android.media.audiopolicy.AudioMix$Builder");
            Class<?> audioPolicyClass = Class.forName("android.media.audiopolicy.AudioPolicy");
            Class<?> audioPolicyBuilderClass = Class.forName("android.media.audiopolicy.AudioPolicy$Builder");

            Object mixRuleBuilder = audioMixingRuleBuilderClass.getConstructor().newInstance();
            int mixRolePlayers = audioMixingRuleClass.getField("MIX_ROLE_PLAYERS").getInt(null);
            audioMixingRuleBuilderClass.getMethod("setTargetMixRole", int.class)
                    .invoke(mixRuleBuilder, mixRolePlayers);

            AudioAttributes mediaAttributes = new AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA).build();
            int ruleMatchUsage = audioMixingRuleClass.getField("RULE_MATCH_ATTRIBUTE_USAGE").getInt(null);
            audioMixingRuleBuilderClass.getMethod("addMixRule", int.class, Object.class)
                    .invoke(mixRuleBuilder, ruleMatchUsage, mediaAttributes);
            audioMixingRuleBuilderClass.getMethod("voiceCommunicationCaptureAllowed", boolean.class)
                    .invoke(mixRuleBuilder, true);
            Object mixRule = audioMixingRuleBuilderClass.getMethod("build").invoke(mixRuleBuilder);

            Object mixBuilder = audioMixBuilderClass.getConstructor(audioMixingRuleClass)
                    .newInstance(mixRule);
            AudioFormat format = new AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(SAMPLE_RATE)
                    .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
                    .build();
            mixBuilder.getClass().getMethod("setFormat", AudioFormat.class).invoke(mixBuilder, format);
            int loopBack = audioMixClass.getField("ROUTE_FLAG_LOOP_BACK").getInt(null);
            mixBuilder.getClass().getMethod("setRouteFlags", int.class).invoke(mixBuilder, loopBack);
            Object mix = audioMixBuilderClass.getMethod("build").invoke(mixBuilder);

            Object policyBuilder = audioPolicyBuilderClass
                    .getConstructor(android.content.Context.class)
                    .newInstance(FakeContext.get());
            policyBuilder.getClass().getMethod("addMix", audioMixClass).invoke(policyBuilder, mix);
            Object policy = audioPolicyBuilderClass.getMethod("build").invoke(policyBuilder);

            java.lang.reflect.Method registerStatic = android.media.AudioManager.class
                    .getDeclaredMethod("registerAudioPolicyStatic", audioPolicyClass);
            registerStatic.setAccessible(true);
            int result = (int) registerStatic.invoke(null, policy);
            if (result != 0) {
                throw new IOException("registerAudioPolicyStatic() returned " + result);
            }

            java.lang.reflect.Method createSink = audioPolicyClass
                    .getMethod("createAudioRecordSink", audioMixClass);
            return (AudioRecord) createSink.invoke(policy, mix);
        } catch (IOException e) {
            throw e;
        } catch (Exception e) {
            throw new IOException("create loopback recorder failed", e);
        }
    }

    private void releaseAll() {
        if (recorder != null) {
            try {
                recorder.stop();
                recorder.release();
            } catch (Exception ignored) {
                // ignore
            }
            recorder = null;
        }
        if (codec != null) {
            try {
                codec.stop();
                codec.release();
            } catch (Exception ignored) {
                // ignore
            }
            codec = null;
        }
    }

    public void stop() {
        running = false;
        if (thread != null) {
            try {
                thread.join(2000);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
        }
    }
}
