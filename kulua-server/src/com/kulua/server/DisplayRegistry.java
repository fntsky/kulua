package com.kulua.server;

import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.util.Log;
import android.view.Surface;

import java.util.HashMap;
import java.util.Map;

/**
 * 虚拟显示器管理（多融合窗口）。
 *
 * 每个显示器：一个 VirtualDisplay + 一个 DisplayEncoder（MediaCodec 编码器）。
 * 显示器 id 由本类分配（内部递增），displayId 为系统 VirtualDisplay 的真实 id。
 */
public final class DisplayRegistry {

    private static final String TAG = "kulua-server";

    // 隐藏常量（对照 scrcpy NewDisplayCapture.java 的 flags 组合）
    private static final int FLAG_PUBLIC = android.hardware.display.DisplayManager.VIRTUAL_DISPLAY_FLAG_PUBLIC;
    private static final int FLAG_PRESENTATION = android.hardware.display.DisplayManager.VIRTUAL_DISPLAY_FLAG_PRESENTATION;
    private static final int FLAG_OWN_CONTENT_ONLY = android.hardware.display.DisplayManager.VIRTUAL_DISPLAY_FLAG_OWN_CONTENT_ONLY;
    private static final int FLAG_SUPPORTS_TOUCH = 1 << 6;
    private static final int FLAG_ROTATES_WITH_CONTENT = 1 << 7;
    private static final int FLAG_TRUSTED = 1 << 10;
    private static final int FLAG_OWN_DISPLAY_GROUP = 1 << 11;
    private static final int FLAG_ALWAYS_UNLOCKED = 1 << 12;
    private static final int FLAG_TOUCH_FEEDBACK_DISABLED = 1 << 13;
    private static final int FLAG_OWN_FOCUS = 1 << 14;
    private static final int FLAG_DEVICE_DISPLAY_GROUP = 1 << 15;

    private final Map<Integer, DisplayEntry> displays = new HashMap<>();
    private int nextId = 1;

    /** 显示器条目：编码器 + 系统 VirtualDisplay。 */
    public static final class DisplayEntry {
        public final int id;
        public DisplayEncoder encoder;
        public VirtualDisplay virtualDisplay;

        DisplayEntry(int id) {
            this.id = id;
        }
    }

    /** 分配内部显示器 id（编码器由视频连接创建后 attach）。 */
    public synchronized DisplayEntry create(int width, int height, int dpi) {
        int id = nextId++;
        DisplayEntry entry = new DisplayEntry(id);
        displays.put(id, entry);
        Log.i(TAG, "create display #" + id + " " + width + "x" + height + "@" + dpi);
        return entry;
    }

    /** 视频连接创建编码器后回填。 */
    public synchronized void attachEncoder(int id, DisplayEncoder encoder) {
        DisplayEntry entry = displays.get(id);
        if (entry != null) {
            entry.encoder = encoder;
        }
    }

    /** 编码器启动后回填系统 VirtualDisplay（编码器内部创建）。 */
    public synchronized void attachVirtualDisplay(int id, VirtualDisplay virtualDisplay) {
        DisplayEntry entry = displays.get(id);
        if (entry != null) {
            entry.virtualDisplay = virtualDisplay;
        }
    }

    /** 系统 VirtualDisplay 的真实 display id（输入注入用）。 */
    public synchronized int systemDisplayId(int id) {
        DisplayEntry entry = displays.get(id);
        return entry != null && entry.virtualDisplay != null
                ? entry.virtualDisplay.getDisplay().getDisplayId() : 0;
    }

    /** 按内部 id 获取条目。 */
    public synchronized DisplayEntry get(int id) {
        return displays.get(id);
    }

    /** 销毁显示器（停止编码器 + 释放 VirtualDisplay）。 */
    public synchronized void destroy(int id) {
        DisplayEntry entry = displays.remove(id);
        if (entry == null) {
            return;
        }
        Log.i(TAG, "destroy display #" + id);
        entry.encoder.stop();
        if (entry.virtualDisplay != null) {
            entry.virtualDisplay.release();
        }
    }

    public synchronized void destroyAll() {
        for (Integer id : displays.keySet().toArray(new Integer[0])) {
            destroy(id);
        }
    }

    /** 构建 VirtualDisplay flags（对照 scrcpy NewDisplayCapture）。 */
    static int buildFlags() {
        int flags = FLAG_PUBLIC | FLAG_PRESENTATION | FLAG_OWN_CONTENT_ONLY
                | FLAG_SUPPORTS_TOUCH | FLAG_ROTATES_WITH_CONTENT;
        if (android.os.Build.VERSION.SDK_INT >= 33) {
            flags |= FLAG_TRUSTED | FLAG_OWN_DISPLAY_GROUP | FLAG_ALWAYS_UNLOCKED
                    | FLAG_TOUCH_FEEDBACK_DISABLED;
            if (android.os.Build.VERSION.SDK_INT >= 34) {
                flags |= FLAG_OWN_FOCUS | FLAG_DEVICE_DISPLAY_GROUP;
            }
        }
        return flags;
    }
}
