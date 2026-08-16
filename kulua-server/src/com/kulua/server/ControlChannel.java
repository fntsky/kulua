package com.kulua.server;

import android.net.LocalSocket;
import android.util.Log;
import android.view.InputDevice;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;

import java.io.DataInputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;

/**
 * 控制通道：解析 client → server 控制消息并执行。
 *
 * 消息布局沿用 scrcpy v4.0（对照 app/src/control_msg.c）：
 * - 0  INJECT_KEYCODE: action u8, keycode u32be, repeat u32be, metaState u32be
 * - 1  INJECT_TEXT:    len u32be + UTF-8
 * - 2  INJECT_TOUCH:   displayId u32be, action u8, pointerId u64be, x i32be, y i32be,
 *                      w u16be, h u16be, pressure u16fp, actionButton u32be, buttons u32be
 * - 3  INJECT_SCROLL:  displayId u32be, x i32be, y i32be, w u16be, h u16be,
 *                      hScroll i16fp, vScroll i16fp, buttons u32be
 * - 4  BACK_OR_SCREEN_ON: action u8
 * - 9  SET_CLIPBOARD:  sequence u64be, paste u8, len u32be + UTF-8
 * - 16 START_APP:      displayId u32be, len u8 + package（多显示器扩展）
 * - 21 RESIZE_DISPLAY: displayId u32be, w u16be, h u16be（多显示器扩展）
 * - 100 CREATE_DISPLAY: w u16be, h u16be, dpi u16be（扩展，阶段 2 生效）
 * - 101 DESTROY_DISPLAY: displayId u32be（扩展，阶段 2 生效）
 */
public final class ControlChannel {

    private static final String TAG = "kulua-server";

    private static final int TYPE_INJECT_KEYCODE = 0;
    private static final int TYPE_INJECT_TEXT = 1;
    private static final int TYPE_INJECT_TOUCH_EVENT = 2;
    private static final int TYPE_INJECT_SCROLL_EVENT = 3;
    private static final int TYPE_BACK_OR_SCREEN_ON = 4;
    private static final int TYPE_SET_CLIPBOARD = 9;
    private static final int TYPE_START_APP = 16;
    private static final int TYPE_RESIZE_DISPLAY = 21;
    // 扩展消息（避开 scrcpy 已用值）
    private static final int TYPE_CREATE_DISPLAY = 100;
    private static final int TYPE_DESTROY_DISPLAY = 101;

    private final LocalSocket socket;
    private final DisplayRegistry displayManager;

    public ControlChannel(LocalSocket socket, DisplayRegistry displayManager) {
        this.socket = socket;
        this.displayManager = displayManager;
    }

    public void run() throws IOException {
        DataInputStream input = new DataInputStream(socket.getInputStream());
        // 剪贴板变化推送（server → client）：注册监听，连接断开时注销
        Clipboard.ChangeListener listener = this::pushClipboard;
        Clipboard.addChangeListener(listener);
        try {
            while (true) {
                int type;
                try {
                    type = input.readUnsignedByte();
                } catch (EOFException e) {
                    // 客户端断开，正常结束
                    break;
                }
                if (!handleMessage(type, input)) {
                    break;
                }
            }
        } finally {
            Clipboard.removeChangeListener(listener);
        }
        Log.i(TAG, "control channel closed");
    }

    /**
     * 推送剪贴板变化：scrcpy 客户端事件格式 type 0x00 + len u32be + UTF-8。
     * 仅在有内容且与当前值不同时推送（避免 setClipboard 回环时重复推送）。
     */
    private void pushClipboard(String text) {
        try {
            byte[] data = text.getBytes(StandardCharsets.UTF_8);
            synchronized (socket) {
                OutputStream out = socket.getOutputStream();
                out.write(0x00);
                out.write((data.length >>> 24) & 0xff);
                out.write((data.length >>> 16) & 0xff);
                out.write((data.length >>> 8) & 0xff);
                out.write(data.length & 0xff);
                out.write(data);
                out.flush();
            }
        } catch (IOException e) {
            // 连接可能已断开，忽略（run() 的读循环会处理断开）
            Log.w(TAG, "clipboard push failed", e);
        }
    }

    private boolean handleMessage(int type, DataInputStream input) throws IOException {
        switch (type) {
            case TYPE_INJECT_KEYCODE:
                injectKeycode(input);
                return true;
            case TYPE_INJECT_TEXT:
                injectText(input);
                return true;
            case TYPE_INJECT_TOUCH_EVENT:
                injectTouch(input);
                return true;
            case TYPE_INJECT_SCROLL_EVENT:
                injectScroll(input);
                return true;
            case TYPE_BACK_OR_SCREEN_ON:
                injectBackOrScreenOn(input);
                return true;
            case TYPE_SET_CLIPBOARD:
                setClipboard(input);
                return true;
            case TYPE_START_APP:
                startApp(input);
                return true;
            case TYPE_RESIZE_DISPLAY:
                resizeDisplay(input);
                return true;
            case TYPE_CREATE_DISPLAY:
                createDisplay(input);
                return true;
            case TYPE_DESTROY_DISPLAY:
                destroyDisplay(input);
                return true;
            default:
                Log.w(TAG, "unknown control message type: " + type);
                return false;
        }
    }

    private void injectKeycode(DataInputStream input) throws IOException {
        int action = input.readUnsignedByte();
        int keyCode = input.readInt();
        int repeat = input.readInt();
        int metaState = input.readInt();
        long now = System.currentTimeMillis();
        KeyEvent event = new KeyEvent(now, now, action, keyCode, repeat, metaState,
                KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, InputDevice.SOURCE_KEYBOARD);
        Device.injectKeyEvent(event);
    }

    private void injectText(DataInputStream input) throws IOException {
        int len = input.readInt();
        if (len < 0 || len > 1024 * 1024) {
            Log.w(TAG, "invalid text length: " + len);
            return;
        }
        byte[] data = new byte[len];
        input.readFully(data);
        String text = new String(data, StandardCharsets.UTF_8);
        Device.injectText(text);
    }

    private void injectTouch(DataInputStream input) throws IOException {
        int displayId = input.readInt();
        int action = input.readUnsignedByte();
        long pointerId = input.readLong();
        int x = input.readInt();
        int y = input.readInt();
        int screenWidth = input.readUnsignedShort();
        int screenHeight = input.readUnsignedShort();
        int pressure = input.readUnsignedShort();
        int actionButton = input.readInt();
        int buttons = input.readInt();
        // u16 定点压力（0..0xFFFF）→ float 0..1
        float pressureFloat = pressure / 65535.0f;
        int systemDisplayId = displayManager.systemDisplayId(displayId);
        Device.injectTouch(systemDisplayId, action, pointerId, x, y, screenWidth, screenHeight,
                pressureFloat, actionButton, buttons);
    }

    private void injectScroll(DataInputStream input) throws IOException {
        int displayId = input.readInt();
        int x = input.readInt();
        int y = input.readInt();
        int screenWidth = input.readUnsignedShort();
        int screenHeight = input.readUnsignedShort();
        int hScrollRaw = input.readShort();
        int vScrollRaw = input.readShort();
        int buttons = input.readInt();
        float hScroll = hScrollRaw / 32768.0f * 16.0f;
        float vScroll = vScrollRaw / 32768.0f * 16.0f;
        int systemDisplayId = displayManager.systemDisplayId(displayId);
        Device.injectScroll(systemDisplayId, x, y, screenWidth, screenHeight, hScroll, vScroll, buttons);
    }

    private void injectBackOrScreenOn(DataInputStream input) throws IOException {
        int action = input.readUnsignedByte();
        Device.injectKeyEvent(new KeyEvent(System.currentTimeMillis(), System.currentTimeMillis(),
                action, KeyEvent.KEYCODE_BACK, 0, 0, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0,
                InputDevice.SOURCE_KEYBOARD));
    }

    private void setClipboard(DataInputStream input) throws IOException {
        long sequence = input.readLong();
        boolean paste = input.readUnsignedByte() != 0;
        int len = input.readInt();
        if (len < 0 || len > 16 * 1024 * 1024) {
            Log.w(TAG, "invalid clipboard length: " + len);
            return;
        }
        byte[] data = new byte[len];
        input.readFully(data);
        String text = new String(data, StandardCharsets.UTF_8);
        boolean ok = Clipboard.set(text);
        Log.i(TAG, "set clipboard (" + sequence + ") paste=" + paste + " ok=" + ok);
        Clipboard.broadcastChange(text);
        if (paste) {
            Device.injectText(text);
        }
    }

    private void startApp(DataInputStream input) throws IOException {
        int displayId = input.readInt();
        int len = input.readUnsignedByte();
        byte[] data = new byte[len];
        input.readFully(data);
        String packageName = new String(data, StandardCharsets.UTF_8);
        int systemDisplayId = displayManager.systemDisplayId(displayId);
        Device.startApp(packageName, systemDisplayId);
    }

    private void resizeDisplay(DataInputStream input) throws IOException {
        int displayId = input.readInt();
        int width = input.readUnsignedShort();
        int height = input.readUnsignedShort();
        DisplayRegistry.DisplayEntry entry = displayManager.get(displayId);
        if (entry != null && entry.encoder != null) {
            Log.i(TAG, "resize display #" + displayId + " to " + width + "x" + height);
            entry.encoder.resize(width, height);
        } else {
            Log.w(TAG, "resize unknown display #" + displayId);
        }
    }

    private void createDisplay(DataInputStream input) throws IOException {
        int width = input.readUnsignedShort();
        int height = input.readUnsignedShort();
        int dpi = input.readUnsignedShort();
        // video 连接自带创建参数（6B），控制通道的 CREATE_DISPLAY 仅登记尺寸
        Log.i(TAG, "create display request " + width + "x" + height + "@" + dpi
                + " (由 video 连接实际创建)");
    }

    private void destroyDisplay(DataInputStream input) throws IOException {
        int displayId = input.readInt();
        Log.i(TAG, "destroy display #" + displayId + " (TODO: 阶段 2)");
    }

    private void sendControlEvent(int type, byte[] payload) {
        try {
            OutputStream output = socket.getOutputStream();
            output.write(type);
            output.write(payload);
            output.flush();
        } catch (IOException e) {
            Log.w(TAG, "send control event failed", e);
        }
    }
}
