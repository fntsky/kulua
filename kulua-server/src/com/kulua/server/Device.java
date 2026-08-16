package com.kulua.server;

import android.content.Context;
import android.hardware.input.InputManager;
import android.util.Log;
import android.view.InputDevice;
import android.view.InputEvent;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;

/**
 * 设备操作：输入注入与应用启动（shell 权限，app_process 下可用）。
 *
 * 注：触摸注入使用 displayId=0（实体屏）；多显示器场景的 display 映射在
 * 阶段 2 完善（位置事件按 position.screenSize 匹配显示器）。
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

    public static boolean injectKeyEvent(KeyEvent event) {
        return injectEvent(event, 0);
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

    /** 文本注入：按字符映射为 key event（KeyCharacterMap.getKeyEvents，公开 API）。 */
    public static boolean injectText(String text) {
        boolean ok = true;
        for (int i = 0; i < text.length(); i++) {
            char c = text.charAt(i);
            // getEvents 是实例方法；getKeyEvents(char) 是隐藏 API
            @SuppressWarnings("deprecation")
            KeyEvent[] events = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD)
                    .getEvents(new char[]{c});
            if (events == null || events.length == 0) {
                Log.w(TAG, "cannot map char: " + c);
                continue;
            }
            for (KeyEvent event : events) {
                ok &= injectEvent(event, 0);
            }
        }
        return ok;
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

    /** 启动应用：shell 权限下用 `am start` 命令（ActivityManager.startActivity 静态为隐藏 API）。 */
    public static void startApp(String packageName) {
        try {
            Process process = new ProcessBuilder("am", "start",
                    "-a", "android.intent.action.MAIN",
                    "-c", "android.intent.category.LAUNCHER",
                    "-p", packageName)
                    .redirectErrorStream(true)
                    .start();
            process.waitFor();
            Log.i(TAG, "start app " + packageName + " exit=" + process.exitValue());
        } catch (Exception e) {
            Log.e(TAG, "start app failed: " + packageName, e);
        }
    }
}
