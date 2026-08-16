package com.kulua.server;

import android.annotation.TargetApi;
import android.content.AttributionSource;
import android.content.ContentResolver;
import android.content.Context;
import android.content.ContextWrapper;
import android.content.IContentProvider;
import android.os.Binder;
import android.os.IBinder;
import android.os.Process;

import java.lang.reflect.Method;

/**
 * 伪 Context：把系统 Context 的包名伪装成 shell 包名。
 *
 * 为什么需要：
 * 1. `DisplayManager.createVirtualDisplay` 校验 packageName 与 calling uid 匹配，
 *    系统 Context 的包名（"android"）不匹配 shell uid → SecurityException。
 *    官方 scrcpy 用 FakeContext 解决（包名设为 com.android.shell）。
 * 2. 剪贴板读取（Settings provider）同样校验包名与 uid，且 ContentResolver
 *    必须用 `getContentProviderExternal`（binder 直取 provider）绕开
 *    ActivityThread.acquireProvider 的 uid 校验。
 */
public final class FakeContext extends ContextWrapper {

    public static final String PACKAGE_NAME = "com.android.shell";

    private static final FakeContext INSTANCE = new FakeContext();

    public static FakeContext get() {
        return INSTANCE;
    }

    private final ContentResolver contentResolver = new ContentResolver(this) {
        @SuppressWarnings("unused")
        // @Override (but super-class method not visible)
        protected IContentProvider acquireProvider(Context c, String name) {
            return getContentProviderExternal(name);
        }

        @SuppressWarnings("unused")
        // @Override (but super-class method not visible)
        public boolean releaseProvider(IContentProvider icp) {
            return false;
        }

        @SuppressWarnings("unused")
        // @Override (but super-class method not visible)
        protected IContentProvider acquireUnstableProvider(Context c, String name) {
            return null;
        }

        @SuppressWarnings("unused")
        // @Override (but super-class method not visible)
        public boolean releaseUnstableProvider(IContentProvider icp) {
            return false;
        }

        @SuppressWarnings("unused")
        // @Override (but super-class method not visible)
        public void unstableProviderDied(IContentProvider icp) {
            // ignore
        }
    };

    private FakeContext() {
        super(Clipboard.getContext());
    }

    @Override
    public ContentResolver getContentResolver() {
        return contentResolver;
    }

    @Override
    public String getPackageName() {
        return PACKAGE_NAME;
    }

    @Override
    public String getOpPackageName() {
        return PACKAGE_NAME;
    }

    @TargetApi(31)
    @Override
    public AttributionSource getAttributionSource() {
        AttributionSource.Builder builder = new AttributionSource.Builder(Process.SHELL_UID);
        builder.setPackageName(PACKAGE_NAME);
        return builder.build();
    }

    /** ActivityManager.getContentProviderExternal：binder 直取 provider（shell 权限下可用）。 */
    private static IContentProvider getContentProviderExternal(String name) {
        try {
            Class<?> activityManagerClass = Class.forName("android.app.ActivityManager");
            Method getService = activityManagerClass.getDeclaredMethod("getService");
            getService.setAccessible(true);
            Object am = getService.invoke(null);
            // 新版签名：getContentProviderExternal(String, int, IBinder, String)
            try {
                Method m = am.getClass().getMethod("getContentProviderExternal",
                        String.class, int.class, IBinder.class, String.class);
                return (IContentProvider) m.invoke(am, name, Process.SHELL_UID,
                        new Binder(), PACKAGE_NAME);
            } catch (NoSuchMethodException e) {
                // 旧版签名：getContentProviderExternal(String, int, IBinder)
                Method m = am.getClass().getMethod("getContentProviderExternal",
                        String.class, int.class, IBinder.class);
                return (IContentProvider) m.invoke(am, name, Process.SHELL_UID, new Binder());
            }
        } catch (Exception e) {
            android.util.Log.e("kulua-server", "getContentProviderExternal failed: " + name, e);
            return null;
        }
    }
}
