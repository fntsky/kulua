package com.kulua.server;

import android.content.Context;
import android.hardware.input.InputManager;
import android.view.InputDevice;
import android.view.InputEvent;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;

import java.io.IOException;

/**
 * 设备操作：输入注入与应用启动（shell 权限，app_process 下可用）。
 *
 * 所有注入均带 displayId：0=实体屏，>0=虚拟显示器（setDisplayId 隐藏 API）。
 */
public final class Device {


    private Device() {
        // not instantiable
    }

    private static InputManager getInputManager() {
        return (InputManager) Clipboard.getContext().getSystemService(Context.INPUT_SERVICE);
    }

    /** 隐藏 API：InputManager.injectInputEvent(InputEvent, int)，反射调用。 */
    private static boolean injectInputEvent(InputEvent event, int mode) {
        try {
            java.lang.reflect.Method method = InputManager.class.getMethod(
                    "injectInputEvent", InputEvent.class, int.class);
            return (Boolean) method.invoke(getInputManager(), event, mode);
        } catch (Exception e) {
            Server.e("injectInputEvent reflection failed", e);
            return false;
        }
    }

    public static boolean injectKeyEvent(KeyEvent event, int displayId) {
        return injectEvent(event, displayId);
    }

    public static boolean injectTouch(int displayId, int action, long pointerId, int x, int y,
                                      int screenWidth, int screenHeight, float pressure,
                                      int actionButton, int buttons) {
        long now = System.currentTimeMillis();
        MotionEvent.PointerProperties[] properties = {
                new MotionEvent.PointerProperties(),
        };
        properties[0].id = 0;
        properties[0].toolType = MotionEvent.TOOL_TYPE_FINGER;
        MotionEvent.PointerCoords[] coords = {new MotionEvent.PointerCoords()};
        coords[0].x = x;
        coords[0].y = y;
        coords[0].pressure = pressure;
        coords[0].size = 1.0f;
        MotionEvent event = MotionEvent.obtain(now, now, action, 1,
                properties, coords, 0, actionButton | buttons,
                1.0f, 1.0f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0);
        return injectEvent(event, displayId);
    }

    public static boolean injectScroll(int displayId, int x, int y, int screenWidth,
                                       int screenHeight, float hScroll, float vScroll, int buttons) {
        long now = System.currentTimeMillis();
        MotionEvent.PointerProperties[] properties = {
                new MotionEvent.PointerProperties(),
        };
        properties[0].id = 0;
        properties[0].toolType = MotionEvent.TOOL_TYPE_MOUSE;
        MotionEvent.PointerCoords[] coords = {new MotionEvent.PointerCoords()};
        coords[0].x = x;
        coords[0].y = y;
        coords[0].setAxisValue(MotionEvent.AXIS_HSCROLL, hScroll);
        coords[0].setAxisValue(MotionEvent.AXIS_VSCROLL, vScroll);
        MotionEvent event = MotionEvent.obtain(now, now, MotionEvent.ACTION_SCROLL, 1,
                properties, coords, 0, buttons,
                1.0f, 1.0f, 0, 0, InputDevice.SOURCE_MOUSE, 0);
        return injectEvent(event, displayId);
    }

    /**
     * 文本注入到指定显示器。
     *
     * - 全 ASCII：按字符映射为 key event（KeyCharacterMap.getKeyEvents）逐字符注入
     * - 含非 ASCII（中文等）：KeyCharacterMap 无法映射 → 写入系统剪贴板后注入
     *   KEYCODE_PASTE 粘贴（scrcpy 客户端同款方案；需输入框有焦点）
     */
    public static boolean injectText(String text, int displayId) {
        if (isAscii(text)) {
            boolean ok = true;
            for (int i = 0; i < text.length(); i++) {
                char c = text.charAt(i);
                @SuppressWarnings("deprecation")
                KeyEvent[] events = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD)
                        .getEvents(new char[]{c});
                if (events == null || events.length == 0) {
                    Server.w("cannot map char: " + c);
                    continue;
                }
                for (KeyEvent event : events) {
                    ok &= injectEvent(event, displayId);
                }
            }
            return ok;
        }

        // 非 ASCII：剪贴板 + 粘贴
        if (!Clipboard.set(text)) {
            Server.w("clipboard set failed for paste");
            return false;
        }
        long now = System.currentTimeMillis();
        KeyEvent paste = new KeyEvent(now, now, KeyEvent.ACTION_DOWN,
                KeyEvent.KEYCODE_PASTE, 0, 0, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0,
                InputDevice.SOURCE_KEYBOARD);
        boolean down = injectEvent(paste, displayId);
        KeyEvent pasteUp = new KeyEvent(now, now, KeyEvent.ACTION_UP,
                KeyEvent.KEYCODE_PASTE, 0, 0, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0,
                InputDevice.SOURCE_KEYBOARD);
        boolean up = injectEvent(pasteUp, displayId);
        Server.i("paste non-ascii text (" + text.length() + " chars) down=" + down
                + " up=" + up);
        return down && up;
    }

    private static boolean isAscii(String text) {
        for (int i = 0; i < text.length(); i++) {
            if (text.charAt(i) > 0x7F) {
                return false;
            }
        }
        return true;
    }

    private static boolean injectEvent(InputEvent event, int displayId) {
        // 隐藏 API InputEvent.setDisplayId(int)：注入到指定虚拟显示器
        if (displayId != 0) {
            try {
                java.lang.reflect.Method method = InputEvent.class.getMethod("setDisplayId", int.class);
                method.invoke(event, displayId);
            } catch (Exception e) {
                Server.e("setDisplayId failed", e);
                return false;
            }
        }
        // INJECT_INPUT_EVENT_MODE_ASYNC 是隐藏常量，值为 0
        boolean ok = injectInputEvent(event, 0);
        if (!ok) {
            Server.w("injectInputEvent failed: " + event);
        }
        return ok;
    }

    /** 启动应用到指定显示器（shell 权限下用 `am start`）。 */
    public static void startApp(String packageName, int displayId) {
        new Thread(() -> {
            try {
                // 1) 先解析该包的 LAUNCHER 组件。WHY：个别应用（如高德）的 launcher
                //    用 activity-alias / 非常规 manifest，`am start -p pkg -a MAIN
                //    -c LAUNCHER` 会 "unable to resolve Intent"（实测），应用根本没启动。
                //    用隐藏命令 resolve-activity 拿到真实组件后用 `-n` 启动，最可靠。
                String component = resolveLauncher(packageName);
                System.err.println("[kulua/" + Server.VERSION + "] startApp " + packageName
                        + " display=" + displayId + " component=" + component);

                java.util.List<String> command = new java.util.ArrayList<>();
                command.add("am");
                command.add("start");
                if (component != null) {
                    command.add("-n");
                    command.add(component);
                } else {
                    // 解析不到 launcher → 回退旧方式（错误信息随 out 返回，便于诊断）
                    command.add("-a");
                    command.add("android.intent.action.MAIN");
                    command.add("-c");
                    command.add("android.intent.category.LAUNCHER");
                    command.add("-p");
                    command.add(packageName);
                }
                if (displayId != 0) {
                    command.add("--display");
                    command.add(String.valueOf(displayId));
                }
                // 诊断：System.err 会随 daemon 的 `adb shell` stderr 转发显示出来
                // （Log.i 只进 logcat，daemon 看不到）。
                System.err.println("[kulua/" + Server.VERSION + "] startApp cmd=" + String.join(" ", command));

                Process process = new ProcessBuilder(command).redirectErrorStream(true).start();
                // 异步等待：`am start` 每次要冷启动一个 app_process 虚拟机（1~3s），
                // 同步 waitFor 会阻塞 control 连接的处理线程。输出要 drain，防管道写满。
                StringBuilder out = new StringBuilder(4096);
                byte[] buf = new byte[1024];
                while (process.getInputStream().read(buf) != -1) {
                    if (out.length() < 8192) {
                        out.append(new String(buf, java.nio.charset.StandardCharsets.UTF_8));
                    }
                }
                int exit = process.waitFor();
                String msg = out.toString().trim();
                System.err.println("[kulua/" + Server.VERSION + "] startApp done " + packageName
                        + " display=" + displayId + " exit=" + exit
                        + (msg.isEmpty() ? "" : " out=" + msg));
                Server.i("start app " + packageName + " on display " + displayId
                        + " exit=" + exit);
            } catch (Exception e) {
                System.err.println("[kulua/" + Server.VERSION + "] startApp failed " + packageName + ": " + e);
                Server.e("start app failed: " + packageName, e);
            }
        }, "am-start-" + packageName).start();
    }

    /**
     * 解析包名的 LAUNCHER 组件（`cmd package resolve-activity --brief`）。
     *
     * 返回形如 `com.autonavi.minimap/com.autonavi.minimap.main.MainActivity`；
     * 无 launcher / 命令失败返回 null（调用方回退 `-p` 方式）。
     * Android 8+ 支持该隐藏命令。
     */
    private static String resolveLauncher(String packageName) {
        try {
            Process p = new ProcessBuilder("cmd", "package", "resolve-activity", "--brief",
                    "-a", "android.intent.action.MAIN",
                    "-c", "android.intent.category.LAUNCHER",
                    packageName).redirectErrorStream(true).start();
            StringBuilder out = new StringBuilder(4096);
            byte[] buf = new byte[1024];
            while (p.getInputStream().read(buf) != -1) {
                if (out.length() < 4096) {
                    out.append(new String(buf, java.nio.charset.StandardCharsets.UTF_8));
                }
            }
            p.waitFor();
            for (String line : out.toString().split("\n")) {
                String t = line.trim();
                if (t.contains("/")) {
                    return t;
                }
            }
            return null;
        } catch (Exception e) {
            Server.w("resolveLauncher failed: " + packageName, e);
            return null;
        }
    }
}
