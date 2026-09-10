package com.kulua.server;

import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioRecord;
import android.media.MediaRecorder;
import android.os.Build;

import java.io.IOException;

/**
 * AudioRecord 构造（从 AudioCapture 拆出，纯搬家）：优先 REMOTE_SUBMIX 直接捕获
 * 系统输出，Android 12 厂商 ROM 未必提供该接口，失败则反射注册 loopback
 * AudioPolicy（对照 scrcpy AudioPlaybackCapture 的完整流程）并以它的 sink 作为
 * 采集器。
 *
 * loopback policy 的持有/注销与采集器一一对应：注册成功后挂在本实例上，
 * {@link #release()} 注销。不能做成静态持有——多个客户端/重建重叠时会把
 * 别人的 policy 顶掉，旧路由会继续截走设备声音。
 */
final class LoopbackRecorder {

    /** 已注册的 AudioPolicy（反射对象，仅本类使用）；null = 未注册/已注销。 */
    private Object policy;

    /**
     * 创建采集器。
     *
     * @param sampleRate 采样率（与编码/直通格式一致）
     * @param frameBytes 一帧 PCM 字节数（缓冲下界）
     */
    AudioRecord create(int sampleRate, int frameBytes) throws IOException {
        try {
            AudioRecord.Builder builder = new AudioRecord.Builder();
            if (Build.VERSION.SDK_INT >= 31) {
                builder.setContext(FakeContext.get());
            }
            int minBuffer = AudioRecord.getMinBufferSize(sampleRate,
                    AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT);
            // REMOTE_SUBMIX 捕获系统输出并转移本机播放，不受单个应用的 usage 限制。
            AudioRecord result = builder.setAudioSource(MediaRecorder.AudioSource.REMOTE_SUBMIX)
                    .setAudioFormat(new AudioFormat.Builder()
                            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                            .setSampleRate(sampleRate)
                            .setChannelMask(AudioFormat.CHANNEL_IN_STEREO).build())
                    .setBufferSizeInBytes(Math.max(frameBytes * 2, minBuffer * 8))
                    .build();
            Server.i("audio source: remote_submix");
            return result;
        } catch (RuntimeException e) {
            if (Build.VERSION.SDK_INT < 33) {
                throw new IOException("create remote submix recorder failed", e);
            }
            Server.w("remote submix unavailable; trying playback capture", e);
            return createLoopback(sampleRate);
        }
    }

    /**
     * 创建 loopback AudioRecord（反射 AudioPolicy，对照 scrcpy AudioPlaybackCapture
     * 的完整流程）：AudioMixingRule + AudioMix(ROUTE_FLAG_LOOP_BACK) +
     * AudioPolicy.Builder(FakeContext) + registerAudioPolicyStatic +
     * createAudioRecordSink。
     */
    @SuppressWarnings("unchecked")
    private AudioRecord createLoopback(int sampleRate) throws IOException {
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
                    .setSampleRate(sampleRate)
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
            // 保持 policy 存活并在停止时注销，否则旧路由可能继续截走设备声音。
            this.policy = policy;

            java.lang.reflect.Method createSink = audioPolicyClass
                    .getMethod("createAudioRecordSink", audioMixClass);
            return (AudioRecord) createSink.invoke(policy, mix);
        } catch (IOException e) {
            throw e;
        } catch (Exception e) {
            throw new IOException("create loopback recorder failed", e);
        }
    }

    /** 注销本实例注册的 loopback policy（无 policy 时空操作）。 */
    void release() {
        Object current = policy;
        policy = null;
        if (current == null) {
            return;
        }
        try {
            android.media.AudioManager.class.getDeclaredMethod("unregisterAudioPolicyAsyncStatic",
                    Class.forName("android.media.audiopolicy.AudioPolicy")).invoke(null, current);
        } catch (Exception e) {
            Server.w("unregister audio policy failed", e);
        }
    }
}
