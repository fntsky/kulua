package com.kulua.server;

import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;

import java.lang.reflect.Method;

/**
 * 剪贴板读写（app_process 无 Context，通过 ActivityThread.getSystemContext() 获取）。
 */
public final class Clipboard {


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
            // app_process 直跑 main 不会初始化 ActivityThread（sCurrentActivityThread 为 null，
            // currentActivityThread() 返回 null → 后续 getSystemContext NPE）。官方 scrcpy
            // 用 Workarounds 手动构造实例并回填静态字段，这里照做：
            Object activityThread;
            try {
                Method currentActivityThread = activityThreadClass.getMethod("currentActivityThread");
                activityThread = currentActivityThread.invoke(null);
            } catch (Exception ignored) {
                activityThread = null;
            }
            if (activityThread == null) {
                java.lang.reflect.Constructor<?> ctor =
                        activityThreadClass.getDeclaredConstructor();
                ctor.setAccessible(true);
                activityThread = ctor.newInstance();
                // ActivityThread.sCurrentActivityThread = activityThread;
                java.lang.reflect.Field sCurrentActivityThread =
                        activityThreadClass.getDeclaredField("sCurrentActivityThread");
                sCurrentActivityThread.setAccessible(true);
                sCurrentActivityThread.set(null, activityThread);
                // activityThread.mSystemThread = true（否则 getSystemContext 内部校验失败）
                java.lang.reflect.Field mSystemThread =
                        activityThreadClass.getDeclaredField("mSystemThread");
                mSystemThread.setAccessible(true);
                mSystemThread.setBoolean(activityThread, true);
            }
            Method getSystemContext = activityThreadClass.getMethod("getSystemContext");
            return (Context) getSystemContext.invoke(activityThread);
        } catch (Exception e) {
            throw new AssertionError("cannot create system context", e);
        }
    }

    public static boolean set(String text) {
        try {
            ClipboardManager manager = getClipboardManager();
            manager.setPrimaryClip(ClipData.newPlainText(null, text));
            return true;
        } catch (Exception e) {
            Server.e("set clipboard failed", e);
            return false;
        }
    }

    /**
     * 获取绑定 FakeContext 的 ClipboardManager。
     *
     * 不能用 getSystemService(CLIPBOARD_SERVICE)：ContextWrapper 委托给 base
     * （系统 ContextImpl），返回的 ClipboardManager 内部 mContext 仍是系统
     * context（getOpPackageName() = "android"）。vivo 等 ROM 的 getPrimaryClip
     * 会额外查询 Settings（isUserGettingPrimaryClip），该查询经 ContentResolver
     * 校验包名与 uid → SecurityException。
     * 反射构造 ClipboardManager(FakeContext, Looper)：mContext = FakeContext
     * （getOpPackageName() = "com.android.shell"，与 shell uid 匹配），且
     * FakeContext 覆盖的 getContentResolver()（getContentProviderExternal）
     * 在 vivo 的 Settings 查询路径上生效。
     */
    private static ClipboardManager getClipboardManager() throws Exception {
        // 构造器签名因 ROM 而异：AOSP 是 (Context, Looper)，vivo 改成了
        // (Context, Handler)（实测枚举确认）。先试 AOSP 签名，失败再试 vivo。
        android.content.Context ctx = FakeContext.get();
        // app_process 直跑 main：Looper.prepare() 只设置了线程局部 looper，
        // 没有回填 sMainLooper（Looper.getMainLooper() 返回 null）。Handler
        // 构造需要非 null looper，因此用当前线程的 looper（主线程有，
        // monitor 等后台线程没有）→ 无 looper 时 new Handler() 会抛异常。
        // 解决：后台线程里也先 Looper.prepare()（一次性）。
        if (android.os.Looper.myLooper() == null) {
            android.os.Looper.prepare();
        }
        android.os.Handler handler = new android.os.Handler(android.os.Looper.myLooper());
        Exception last = null;
        for (Class<?>[] sig : new Class<?>[][]{
                {android.content.Context.class, android.os.Looper.class},
                {android.content.Context.class, android.os.Handler.class},
        }) {
            try {
                java.lang.reflect.Constructor<ClipboardManager> ctor =
                        ClipboardManager.class.getDeclaredConstructor(sig);
                ctor.setAccessible(true);
                Object[] args = sig[1] == android.os.Looper.class
                        ? new Object[]{ctx, android.os.Looper.myLooper()}
                        : new Object[]{ctx, handler};
                return ctor.newInstance(args);
            } catch (NoSuchMethodException e) {
                last = e;
            }
        }
        throw new AssertionError("no ClipboardManager ctor", last);
    }

    /**
     * 经 IClipboard binder 直接读剪贴板（绕过 ClipboardManager + Settings 查询）。
     *
     * `service call clipboard` 同款调用链：ServiceManager.getService("clipboard") →
     * IClipboard$Stub.asInterface → getPrimaryClip(callingPackage, ...)。
     * callingPackage 传 com.android.shell（shell uid 的包名），shell 有剪贴板读取权限。
     * 不同 ROM 的 getPrimaryClip 签名不同（API 33+ 多 deviceId 参数），按参数类型匹配。
     */
    private static ClipData getPrimaryClipDirect() {
        try {
            Class<?> serviceManagerClass = Class.forName("android.os.ServiceManager");
            java.lang.reflect.Method getService = serviceManagerClass.getMethod("getService", String.class);
            android.os.IBinder binder = (android.os.IBinder) getService.invoke(null, "clipboard");
            if (binder == null) {
                return null;
            }
            Class<?> iClipboardClass = Class.forName("android.content.IClipboard");
            Class<?> stubClass = Class.forName("android.content.IClipboard$Stub");
            java.lang.reflect.Method asInterface = stubClass.getMethod("asInterface", android.os.IBinder.class);
            Object clipboard = asInterface.invoke(null, binder);

            // 按参数类型匹配 getPrimaryClip 方法（签名因 ROM/API 而异）
            for (java.lang.reflect.Method m : iClipboardClass.getMethods()) {
                if (!m.getName().equals("getPrimaryClip")) {
                    continue;
                }
                Class<?>[] params = m.getParameterTypes();
                if (params.length < 2) {
                    continue;
                }
                // 第一个参数必须是 callingPackage（String）
                if (params[0] != String.class) {
                    continue;
                }
                Object[] args = new Object[params.length];
                args[0] = FakeContext.PACKAGE_NAME; // "com.android.shell"
                // 剩余参数填默认值：String → null，int → 0（userId），
                // boolean → false（vivo 等 ROM 的 getPrimaryClip 额外带 hasListener 等标志）
                for (int i = 1; i < params.length; i++) {
                    if (params[i] == int.class) {
                        args[i] = 0;
                    } else if (params[i] == long.class) {
                        args[i] = 0L;
                    } else if (params[i] == boolean.class) {
                        args[i] = false;
                    } else {
                        args[i] = null;
                    }
                }
                Object result = m.invoke(clipboard, args);
                return result instanceof ClipData ? (ClipData) result : null;
            }
            return null;
        } catch (Exception e) {
            Server.e("getPrimaryClipDirect failed", e);
            return null;
        }
    }

    /** 剪贴板变化时的订阅回调（ClientConnection 注册）。 */
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
            // 不走 ClipboardManager.getPrimaryClip()：vivo 等 ROM 会在内部额外查询
            // Settings provider（isUserGettingPrimaryClip），该查询依赖 ContentResolver
            // acquireProvider（uid 校验），shell 环境下容易失败。
            // 直接调 IClipboard binder（`service call clipboard` 同款）：callingPackage
            // 传 "com.android.shell"（shell uid 的包名），完全绕开 Settings 查询。
            ClipData clip = getPrimaryClipDirect();
            if (clip != null && clip.getItemCount() > 0) {
                CharSequence text = clip.getItemAt(0).getText();
                return text != null ? text.toString() : "";
            }
            return "";
        } catch (Exception e) {
            Server.e("get clipboard failed", e);
            return "";
        }
    }
}
