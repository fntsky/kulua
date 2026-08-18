package com.kulua.server;

import android.content.Context;
import android.hardware.input.InputManager;
import android.util.Log;
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

    private static final String TAG = "kulua-server";

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
            Log.e(TAG, "injectInputEvent reflection failed", e);
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
                    Log.w(TAG, "cannot map char: " + c);
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
            Log.w(TAG, "clipboard set failed for paste");
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
        Log.i(TAG, "paste non-ascii text (" + text.length() + " chars) down=" + down
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
                Log.e(TAG, "setDisplayId failed", e);
                return false;
            }
        }
        // INJECT_INPUT_EVENT_MODE_ASYNC 是隐藏常量，值为 0
        boolean ok = injectInputEvent(event, 0);
        if (!ok) {
            Log.w(TAG, "injectInputEvent failed: " + event);
        }
        return ok;
    }

    /** 启动应用到指定显示器（shell 权限下用 `am start --display <id>`）。 */
    public static void startApp(String packageName, int displayId) {
        try {
            java.util.List<String> command = new java.util.ArrayList<>();
            command.add("am");
            command.add("start");
            command.add("-a");
            command.add("android.intent.action.MAIN");
            command.add("-c");
            command.add("android.intent.category.LAUNCHER");
            command.add("-p");
            command.add(packageName);
            if (displayId != 0) {
                command.add("--display");
                command.add(String.valueOf(displayId));
            }
            Process process = new ProcessBuilder(command).redirectErrorStream(true).start();
            // 为什么异步等待：`am start` 每次都要冷启动一个 app_process 虚拟机
            // （实测 1~3s），同步 waitFor 会阻塞本 control 连接的消息处理线程，
            // 期间输入注入等消息全部排队（应用已开始启动但控制通道被卡住）。
            // 改为后台线程等待退出并记录结果；输出也要 drain，避免 am 写满
            // 管道缓冲而自己阻塞。
            new Thread(() -> {
                try {
                    byte[] buf = new byte[1024];
                    // 读完剩余输出后 am 自然退出
                    while (process.getInputStream().read(buf) != -1) {
                        // discard
                    }
                    int exit = process.waitFor();
                    Log.i(TAG, "start app " + packageName + " on display " + displayId
                            + " exit=" + exit);
                } catch (IOException e) {
                    Log.w(TAG, "start app stream: " + packageName, e);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }, "am-start-" + packageName).start();
        } catch (Exception e) {
            Log.e(TAG, "start app failed: " + packageName, e);
        }
    }
}
