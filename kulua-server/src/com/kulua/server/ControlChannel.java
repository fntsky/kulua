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
 * - 2  INJECT_TOUCH:   action u8, pointerId u64be, x i32be, y i32be,
 *                      w u16be, h u16be, pressure u16fp, actionButton u32be, buttons u32be
 * - 3  INJECT_SCROLL:  x i32be, y i32be, w u16be, h u16be, hScroll i16fp, vScroll i16fp, buttons u32be
 * - 4  BACK_OR_SCREEN_ON: action u8
 * - 9  SET_CLIPBOARD:  sequence u64be, paste u8, len u32be + UTF-8
 * - 16 START_APP:      len u8 + package
 * - 21 RESIZE_DISPLAY: w u16be, h u16be
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
    private final DisplayManager displayManager;

    public ControlChannel(LocalSocket socket, DisplayManager displayManager) {
        this.socket = socket;
        this.displayManager = displayManager;
    }

    public void run() throws IOException {
        DataInputStream input = new DataInputStream(socket.getInputStream());
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
        Log.i(TAG, "control channel closed");
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
        Device.injectTouch(action, pointerId, x, y, screenWidth, screenHeight,
                pressureFloat, actionButton, buttons);
    }

    private void injectScroll(DataInputStream input) throws IOException {
        int x = input.readInt();
        int y = input.readInt();
        int screenWidth = input.readUnsignedShort();
        int screenHeight = input.readUnsignedShort();
        int hScrollRaw = input.readShort();
        int vScrollRaw = input.readShort();
        int buttons = input.readInt();
        float hScroll = hScrollRaw / 32768.0f * 16.0f;
        float vScroll = vScrollRaw / 32768.0f * 16.0f;
        Device.injectScroll(x, y, screenWidth, screenHeight, hScroll, vScroll, buttons);
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
        if (paste) {
            Device.injectText(text);
        }
    }

    private void startApp(DataInputStream input) throws IOException {
        int len = input.readUnsignedByte();
        byte[] data = new byte[len];
        input.readFully(data);
        String packageName = new String(data, StandardCharsets.UTF_8);
        Device.startApp(packageName);
    }

    private void resizeDisplay(DataInputStream input) throws IOException {
        int width = input.readUnsignedShort();
        int height = input.readUnsignedShort();
        // 阶段 2：找到对应显示器并 resize；当前无显示器可调整，仅记录
        Log.i(TAG, "resize display to " + width + "x" + height + " (TODO: 阶段 2)");
    }

    private void createDisplay(DataInputStream input) throws IOException {
        int width = input.readUnsignedShort();
        int height = input.readUnsignedShort();
        int dpi = input.readUnsignedShort();
        int displayId = displayManager.allocateDisplayId();
        Log.i(TAG, "create display #" + displayId + " " + width + "x" + height + "@" + dpi
                + " (TODO: 阶段 2 连接虚拟显示器)");
        // 回复 DISPLAY_CREATED（阶段 2 携带真实 VirtualDisplay id）
        sendControlEvent(TYPE_CREATE_DISPLAY, new byte[]{
                (byte) (displayId >>> 24), (byte) (displayId >>> 16),
                (byte) (displayId >>> 8), (byte) displayId,
        });
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
