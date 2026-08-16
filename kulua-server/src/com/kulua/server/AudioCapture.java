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
     * 创建 loopback AudioRecord（反射 AudioPolicy，对照 scrcpy AudioPlaybackCapture
     * 的完整流程，逐行核对）：
     * - AudioMixingRule(MIX_ROLE_PLAYERS + RULE_MATCH_ATTRIBUTE_USAGE)
     * - AudioMix(ROUTE_FLAG_LOOP_BACK)
     * - AudioPolicy.Builder(FakeContext) + addMix → build
     * - AudioManager.registerAudioPolicyStatic（隐藏静态方法，返回 0 成功）
     * - audioPolicy.createAudioRecordSink(mix)（隐藏方法，返回绑定 mix 的 AudioRecord）
     *
     * 关键差异（与早期实现相比）：必须用 registerAudioPolicyStatic + createAudioRecordSink，
     * 不能 policy.register() + 反射塞 mAudioMix 字段（后者在 Android 13+ 无法工作）。
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

            // builder.addMixRule(RULE_MATCH_ATTRIBUTE_USAGE, mediaAttributes)
            AudioAttributes mediaAttributes = new AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA).build();
            int ruleMatchUsage = audioMixingRuleClass.getField("RULE_MATCH_ATTRIBUTE_USAGE").getInt(null);
            audioMixingRuleBuilderClass.getMethod("addMixRule", int.class, Object.class)
                    .invoke(mixRuleBuilder, ruleMatchUsage, mediaAttributes);

            // builder.voiceCommunicationCaptureAllowed(true)（官方同款）
            audioMixingRuleBuilderClass.getMethod("voiceCommunicationCaptureAllowed", boolean.class)
                    .invoke(mixRuleBuilder, true);

            Object mixRule = audioMixingRuleBuilderClass.getMethod("build").invoke(mixRuleBuilder);

            // AudioMix.Builder mixBuilder = new AudioMix.Builder(mixRule)
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

            // AudioPolicy.Builder policyBuilder = new AudioPolicy.Builder(FakeContext)
            Object policyBuilder = audioPolicyBuilderClass
                    .getConstructor(android.content.Context.class)
                    .newInstance(FakeContext.get());
            // policyBuilder.addMix(mix)
            policyBuilder.getClass().getMethod("addMix", audioMixClass).invoke(policyBuilder, mix);
            Object policy = audioPolicyBuilderClass.getMethod("build").invoke(policyBuilder);

            // AudioManager.registerAudioPolicyStatic(audioPolicy)（隐藏静态方法）
            java.lang.reflect.Method registerStatic = android.media.AudioManager.class
                    .getDeclaredMethod("registerAudioPolicyStatic", audioPolicyClass);
            registerStatic.setAccessible(true);
            int result = (int) registerStatic.invoke(null, policy);
            if (result != 0) {
                throw new IOException("registerAudioPolicyStatic() returned " + result);
            }

            // audioPolicy.createAudioRecordSink(mix)（隐藏方法，返回绑定 mix 的 AudioRecord）
            java.lang.reflect.Method createSink = audioPolicyClass
                    .getMethod("createAudioRecordSink", audioMixClass);
            return (AudioRecord) createSink.invoke(policy, mix);
        } catch (IOException e) {
            throw e;
        } catch (Exception e) {
            throw new IOException("create loopback recorder failed", e);
        }
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
