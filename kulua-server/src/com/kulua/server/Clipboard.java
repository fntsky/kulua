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
