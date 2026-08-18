package com.kulua.server;

import android.util.Log;
import android.view.InputDevice;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;

import kulua.direct.CtrlMsg;
import kulua.direct.SetClipboard;

/**
 * 控制通道：解析 client → server 的 CtrlMsg（protobuf）并执行。
 *
 * 消息语义沿用旧 ControlChannel（对照 scrcpy v4.0 + 多显示器扩展）：
 * 输入注入 / START_APP / RESIZE / CREATE|DESTROY_DISPLAY 均带 displayId。
 * 剪贴板推送（phone → PC）由 ClientConnection 的 Clipboard 监听完成。
 */
public final class ControlChannel {
    private static final String TAG = "kulua-server";

    private ControlChannel() {
        // not instantiable
    }

    /** 处理一条来自 PC 的 control 指令。 */
    public static void handle(ClientConnection client, CtrlMsg msg) {
        if (!msg.hasInjectKeycode() && !msg.hasInjectText() && !msg.hasInjectTouch()
                && !msg.hasInjectScroll() && !msg.hasBackOrScreenOn() && !msg.hasSetClipboard()
                && !msg.hasStartApp() && !msg.hasResizeDisplay() && !msg.hasCreateDisplay()
                && !msg.hasDestroyDisplay() && !msg.hasSetAudioCodec()) {
            Log.w(TAG, "empty/unknown ctrl msg from " + client.addr());
            return;
        }
        if (msg.hasInjectKeycode()) {
            kulua.direct.InjectKeycode m = msg.getInjectKeycode();
            long now = System.currentTimeMillis();
            KeyEvent event = new KeyEvent(now, now, (int) m.getAction(), (int) m.getKeycode(),
                    (int) m.getRepeat(), (int) m.getMetaState(), KeyCharacterMap.VIRTUAL_KEYBOARD,
                    0, 0, InputDevice.SOURCE_KEYBOARD);
            Device.injectKeyEvent(event, (int) m.getDisplayId());
        } else if (msg.hasInjectText()) {
            kulua.direct.InjectText m = msg.getInjectText();
            if (m.getText().length() > 1024 * 1024) {
                Log.w(TAG, "inject text too long");
            } else {
                Device.injectText(m.getText(), (int) m.getDisplayId());
            }
        } else if (msg.hasInjectTouch()) {
            kulua.direct.InjectTouch m = msg.getInjectTouch();
            DisplayRegistry displays = client.displays();
            int systemDisplayId = displays.systemDisplayId(m.getDisplayId());
            Device.injectTouch(systemDisplayId, (int) m.getAction(), m.getPointerId(),
                    m.getX(), m.getY(), m.getScreenWidth(), m.getScreenHeight(),
                    m.getPressure(), (int) m.getActionButton(), (int) m.getButtons());
        } else if (msg.hasInjectScroll()) {
            kulua.direct.InjectScroll m = msg.getInjectScroll();
            DisplayRegistry displays = client.displays();
            int systemDisplayId = displays.systemDisplayId(m.getDisplayId());
            Device.injectScroll(systemDisplayId, m.getX(), m.getY(), m.getScreenWidth(),
                    m.getScreenHeight(), m.getHscroll(), m.getVscroll(), (int) m.getButtons());
        } else if (msg.hasBackOrScreenOn()) {
            kulua.direct.BackOrScreenOn m = msg.getBackOrScreenOn();
            long now = System.currentTimeMillis();
            Device.injectKeyEvent(new KeyEvent(now, now, (int) m.getAction(),
                    KeyEvent.KEYCODE_BACK, 0, 0, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0,
                    InputDevice.SOURCE_KEYBOARD), (int) m.getDisplayId());
        } else if (msg.hasSetClipboard()) {
            SetClipboard m = msg.getSetClipboard();
            if (m.getText().length() > 16 * 1024 * 1024) {
                Log.w(TAG, "clipboard text too long");
                return;
            }
            boolean ok = Clipboard.set(m.getText());
            Log.i(TAG, "set clipboard (seq=" + m.getSequence() + ") paste=" + m.getPaste()
                    + " ok=" + ok);
            Clipboard.broadcastChange(m.getText());
            if (m.getPaste()) {
                // 粘贴到实体屏（同旧实现；viewer 中文输入走 INJECT_TEXT 带 displayId）
                Device.injectText(m.getText(), 0);
            }
        } else if (msg.hasStartApp()) {
            kulua.direct.StartApp m = msg.getStartApp();
            DisplayRegistry displays = client.displays();
            int systemDisplayId = displays.systemDisplayId(m.getDisplayId());
            Device.startApp(m.getPackage(), systemDisplayId);
        } else if (msg.hasResizeDisplay()) {
            kulua.direct.ResizeDisplay m = msg.getResizeDisplay();
            DisplayRegistry.DisplayEntry entry = client.displays().get(m.getDisplayId());
            if (entry != null && entry.encoder != null) {
                Log.i(TAG, "resize display #" + m.getDisplayId() + " to "
                        + m.getWidth() + "x" + m.getHeight());
                entry.encoder.resize(m.getWidth(), m.getHeight());
            } else {
                Log.w(TAG, "resize unknown display #" + m.getDisplayId());
            }
        } else if (msg.hasCreateDisplay()) {
            kulua.direct.CreateDisplay m = msg.getCreateDisplay();
            client.createDisplay(m.getWidth(), m.getHeight(), m.getDpi());
        } else if (msg.hasDestroyDisplay()) {
            kulua.direct.DestroyDisplay m = msg.getDestroyDisplay();
            Log.i(TAG, "destroy display #" + m.getDisplayId());
            client.displays().destroy(m.getDisplayId());
        } else if (msg.hasSetAudioCodec()) {
            int codecIndex = (int) msg.getSetAudioCodec().getCodec();
            String name = codecNameForIndex(codecIndex);
            Log.i(TAG, "audio codec hot-switch → " + name + " (" + client.addr() + ")");
            client.startAudio(name, true);
        }
    }

    /** codec 索引（0=raw 1=opus 2=aac 3=flac，与 PC 端一致）→ 名称。 */
    static String codecNameForIndex(int index) {
        switch (index) {
            case 1: return "opus";
            case 2: return "aac";
            case 3: return "flac";
            default: return "raw";
        }
    }
}
