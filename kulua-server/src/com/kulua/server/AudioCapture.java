package com.kulua.server;

import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioRecord;
import android.net.LocalSocket;
import android.util.Log;

import java.io.IOException;
import java.io.OutputStream;
import java.lang.reflect.Method;

/**
 * 音频捕获（系统播放声音，shell 权限下无需 MediaProjection 授权）。
 *
 * 实现对照 scrcpy AudioPlaybackCapture：反射构造 AudioPolicy + AudioMix
 * （ROUTE_FLAG_LOOP_BACK），AudioRecord 读取混合后的 PCM。
 *
 * 输出格式（沿用 scrcpy 音频协议）：
 * - 先写 4B codec id：0x00726177 = "raw"（S16LE 48kHz 立体声）
 * - 再写帧：12B 头（8B pts/flags + 4B size，大端）+ PCM
 */
public final class AudioCapture {

    private static final String TAG = "kulua-server";

    private static final int SAMPLE_RATE = 48000;
    private static final int CHANNELS = 2;
    private static final int FRAME_MS = 20;
    private static final int FRAME_BYTES = SAMPLE_RATE * CHANNELS * 2 * FRAME_MS / 1000;

    private final LocalSocket socket;
    private volatile boolean running;
    private Thread thread;
    private AudioRecord recorder;

    public AudioCapture(LocalSocket socket) {
        this.socket = socket;
    }

    public void start() throws IOException {
        running = true;
        thread = new Thread(this::captureLoop, "audio-capture");
        thread.start();
    }

    private void captureLoop() {
        try {
            OutputStream output = socket.getOutputStream();
            // codec id：0x00726177 = "raw"（前导 0 + ASCII）
            output.write(new byte[]{0x00, 0x72, 0x61, 0x77});
            output.flush();

            recorder = createLoopbackRecorder();
            recorder.startRecording();

            byte[] frame = new byte[FRAME_BYTES];
            long pts = 0;
            while (running) {
                int read = recorder.read(frame, 0, frame.length);
                if (read <= 0) {
                    continue;
                }
                writeAudioFrame(output, pts, frame, read);
                pts += FRAME_MS * 1000L;
            }
        } catch (IOException | RuntimeException e) {
            if (running) {
                Log.e(TAG, "audio capture ended: " + e.getMessage());
            }
        } finally {
            if (recorder != null) {
                try {
                    recorder.stop();
                    recorder.release();
                } catch (Exception ignored) {
                    // ignore
                }
            }
        }
    }

    private static void writeAudioFrame(OutputStream output, long pts,
                                        byte[] payload, int size) throws IOException {
        byte[] header = new byte[12];
        header[0] = (byte) (pts >>> 56);
        header[1] = (byte) (pts >>> 48);
        header[2] = (byte) (pts >>> 40);
        header[3] = (byte) (pts >>> 32);
        header[4] = (byte) (pts >>> 24);
        header[5] = (byte) (pts >>> 16);
        header[6] = (byte) (pts >>> 8);
        header[7] = (byte) pts;
        header[8] = (byte) (size >>> 24);
        header[9] = (byte) (size >>> 16);
        header[10] = (byte) (size >>> 8);
        header[11] = (byte) size;
        output.write(header);
        output.write(payload, 0, size);
    }

    /**
     * 创建 loopback AudioRecord（反射 AudioPolicy，对照 scrcpy AudioPlaybackCapture）。
     * 捕获系统播放的混音（USAGE_MEDIA 等），shell 权限下可用。
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

            // AudioMixingRule.Builder builder = new AudioMixingRule.Builder();
            Object mixRuleBuilder = audioMixingRuleBuilderClass.getConstructor().newInstance();

            // builder.setTargetMixRole(AudioMixingRule.MIX_ROLE_PLAYERS);
            int mixRolePlayers = audioMixingRuleClass.getField("MIX_ROLE_PLAYERS").getInt(null);
            audioMixingRuleBuilderClass.getMethod("setTargetMixRole", int.class)
                    .invoke(mixRuleBuilder, mixRolePlayers);

            // builder.addMixRule(RULE_MATCH_ATTRIBUTE_USAGE, mediaAttributes);
            AudioAttributes mediaAttributes = new AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA).build();
            int ruleMatchUsage = audioMixingRuleClass.getField("RULE_MATCH_ATTRIBUTE_USAGE").getInt(null);
            audioMixingRuleBuilderClass.getMethod("addMixRule", int.class, Object.class)
                    .invoke(mixRuleBuilder, ruleMatchUsage, mediaAttributes);

            Object mixRule = audioMixingRuleBuilderClass.getMethod("build").invoke(mixRuleBuilder);

            // AudioMix.Builder mixBuilder = new AudioMix.Builder(mixRule);
            Object mixBuilder = audioMixBuilderClass.getConstructor(audioMixingRuleClass)
                    .newInstance(mixRule);

            AudioFormat format = new AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(SAMPLE_RATE)
                    .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
                    .build();
            mixBuilder.getClass().getMethod("setFormat", AudioFormat.class).invoke(mixBuilder, format);

            // ROUTE_FLAG_LOOP_BACK：捕获而不在设备扬声器外放
            int loopBack = audioMixClass.getField("ROUTE_FLAG_LOOP_BACK").getInt(null);
            mixBuilder.getClass().getMethod("setRouteFlags", int.class).invoke(mixBuilder, loopBack);

            Object mix = audioMixBuilderClass.getMethod("build").invoke(mixBuilder);

            // AudioPolicy.Builder policyBuilder = new AudioPolicy.Builder(context);
            Object policyBuilder = audioPolicyBuilderClass
                    .getConstructor(android.content.Context.class)
                    .newInstance(Clipboard.getContext());
            // policyBuilder.setMix(mix)（无参版本在 shell 下可用）
            policyBuilder.getClass().getMethod("setMix", audioMixClass).invoke(policyBuilder, mix);
            Object policy = audioPolicyBuilderClass.getMethod("build").invoke(policyBuilder);

            // policy.register()
            boolean registered = (Boolean) audioPolicyClass.getMethod("register").invoke(policy);
            if (!registered) {
                throw new IOException("AudioPolicy.register() failed");
            }

            // 通过 policy.getMix() 拿到 AudioMix 再建 AudioRecord（反射字段）
            // 简化：直接用 AudioRecord 构造 + 反射 set 私有字段 AudioMix
            return createAudioRecordForMix(mix);
        } catch (IOException e) {
            throw e;
        } catch (Exception e) {
            throw new IOException("create loopback recorder failed", e);
        }
    }

    /** 反射把 AudioMix 塞进 AudioRecord（AudioRecord 构造后通过反射字段）。 */
    private static AudioRecord createAudioRecordForMix(Object mix) throws Exception {
        int bufferBytes = FRAME_BYTES * 4;
        AudioRecord record = new AudioRecord.Builder()
                .setAudioFormat(new AudioFormat.Builder()
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setSampleRate(SAMPLE_RATE)
                        .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
                        .build())
                .setBufferSizeInBytes(bufferBytes)
                .build();
        // 反射写入 AudioRecord 的 AudioMix 字段（scrcpy 同款私有 API 用法）
        java.lang.reflect.Field field = AudioRecord.class.getDeclaredField("mAudioMix");
        field.setAccessible(true);
        field.set(record, mix);
        return record;
    }

    public void stop() {
        running = false;
        if (thread != null) {
            try {
                thread.join(2000);
            } catch (InterruptedException ignored) {
                // ignore
            }
        }
    }
}
