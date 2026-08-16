package com.kulua.server;

import java.util.HashMap;
import java.util.Map;

/** 启动参数解析（与 scrcpy server 风格一致：`key=value` 空格分隔）。 */
public final class Options {

    /** scid（16 进制字符串），socket 名为 scrcpy_<scid> */
    public String scid = "4b4c0000";

    /** 一次性模式：枚举可启动应用后退出（apps.rs 解析输出） */
    public boolean listApps;

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
        options.listApps = "true".equals(map.get("list_apps"));
        return options;
    }
}
