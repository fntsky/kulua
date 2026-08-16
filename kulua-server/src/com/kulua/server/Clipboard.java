package com.kulua.server;

import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.util.Log;

import java.lang.reflect.Method;

/**
 * 剪贴板读写（app_process 无 Context，通过 ActivityThread.getSystemContext() 获取）。
 */
public final class Clipboard {

    private static final String TAG = "kulua-server";

    private static Context context;

    private Clipboard() {
        // not instantiable
    }

    /** 获取系统 Context（Device 等共享）。 */
    public static synchronized Context getContext() {
        if (context == null) {
            context = createSystemContext();
        }
        return context;
    }

    private static Context createSystemContext() {
        try {
            Class<?> activityThreadClass = Class.forName("android.app.ActivityThread");
            Method currentActivityThread = activityThreadClass.getMethod("currentActivityThread");
            Object activityThread = currentActivityThread.invoke(null);
            Method getSystemContext = activityThreadClass.getMethod("getSystemContext");
            return (Context) getSystemContext.invoke(activityThread);
        } catch (Exception e) {
            throw new AssertionError("cannot create system context", e);
        }
    }

    public static boolean set(String text) {
        try {
            ClipboardManager manager =
                    (ClipboardManager) getContext().getSystemService(Context.CLIPBOARD_SERVICE);
            manager.setPrimaryClip(ClipData.newPlainText(null, text));
            return true;
        } catch (Exception e) {
            Log.e(TAG, "set clipboard failed", e);
            return false;
        }
    }

    /** 剪贴板变化时的订阅回调（ConnectionManager 注册）。 */
    public interface ChangeListener {
        void onClipboardChanged(String text);
    }

    private static final java.util.List<ChangeListener> listeners = new java.util.ArrayList<>();

    /** 注册剪贴板变化监听（control 连接推送用）。 */
    public static synchronized void addChangeListener(ChangeListener listener) {
        listeners.add(listener);
    }

    /** 注销剪贴板变化监听（连接断开时）。 */
    public static synchronized void removeChangeListener(ChangeListener listener) {
        listeners.remove(listener);
    }

    /** 广播剪贴板变化（PC→手机写入后回环推送 + 手机本地变化）。 */
    public static synchronized void broadcastChange(String text) {
        for (ChangeListener listener : listeners) {
            try {
                listener.onClipboardChanged(text);
            } catch (Exception ignored) {
                // ignore
            }
        }
    }

    /** 启动系统剪贴板监听线程（手机→PC 方向推送）。 */
    public static void startMonitor() {
        Thread thread = new Thread(() -> {
            String last = null;
            while (true) {
                try {
                    Thread.sleep(300);
                    String current = get();
                    if (!current.isEmpty() && !current.equals(last)) {
                        last = current;
                        broadcastChange(current);
                    }
                } catch (InterruptedException e) {
                    return;
                } catch (Exception ignored) {
                    // ignore
                }
            }
        }, "clipboard-monitor");
        thread.setDaemon(true);
        thread.start();
    }

    public static String get() {
        try {
            ClipboardManager manager =
                    (ClipboardManager) getContext().getSystemService(Context.CLIPBOARD_SERVICE);
            ClipData clip = manager.getPrimaryClip();
            if (clip != null && clip.getItemCount() > 0) {
                CharSequence text = clip.getItemAt(0).getText();
                return text != null ? text.toString() : "";
            }
            return "";
        } catch (Exception e) {
            Log.e(TAG, "get clipboard failed", e);
            return "";
        }
    }
}
