package com.kulua.server;

import java.util.HashMap;
import java.util.Map;

/** 启动参数解析（与 scrcpy server 风格一致：`key=value` 空格分隔）。 */
public final class Options {

    /** scid（16 进制字符串），会话隔离标识 + 自定义协议字段 */
    public String scid = "4b4c0000";

    /** UDP 直连端口（phone 绑定的 Wi-Fi 网络端口） */
    public int port = 27183;

    /** 一次性模式：枚举可启动应用后退出（apps.rs 解析输出） */
    public boolean listApps;

    /** 音频编码器：raw / opus / aac / flac（缺省 raw，与早期版本行为一致） */
    public String audioCodec = "raw";

    /** 音频码率（bps），0 = 编码器默认 */
    public int audioBitRate;

    private Options() {
        // use parse()
    }

    public static Options parse(String... args) {
        Options options = new Options();
        Map<String, String> map = new HashMap<>();
        for (String arg : args) {
            int eq = arg.indexOf('=');
            if (eq > 0) {
                map.put(arg.substring(0, eq), arg.substring(eq + 1));
            }
        }
        String scid = map.get("scid");
        if (scid != null) {
            options.scid = scid;
        }
        String portStr = map.get("port");
        if (portStr != null) {
            try {
                options.port = Integer.parseInt(portStr);
            } catch (NumberFormatException ignored) {
                // 非法端口忽略，用默认
            }
        }
        options.listApps = "true".equals(map.get("list_apps"));
        String audioCodec = map.get("audio_codec");
        if (audioCodec != null) {
            options.audioCodec = audioCodec;
        }
        String audioBitRate = map.get("audio_bit_rate");
        if (audioBitRate != null) {
            try {
                options.audioBitRate = Integer.parseInt(audioBitRate);
            } catch (NumberFormatException ignored) {
                // 非法码率忽略，用默认
            }
        }
        return options;
    }
}
