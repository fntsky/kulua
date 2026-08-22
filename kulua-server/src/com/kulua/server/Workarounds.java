package com.kulua.server;

import android.annotation.SuppressLint;
import android.app.Application;
import android.app.Instrumentation;
import android.content.pm.ApplicationInfo;
import android.os.Build;

import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * ActivityThread 隐藏字段回填（对照官方 scrcpy Workarounds.java）。
 *
 * 为什么需要：`DisplayManager.createVirtualDisplay` 内部经
 * `DisplayManagerGlobal` 校验 `ActivityThread.currentPackageName()`
 * 与调用 uid 匹配。app_process 环境下 ActivityThread 是手动构造的，
 * mBoundApplication 为 null → currentPackageName() 返回 null → 抛
 * "packageName must match the calling uid"。官方做法是把
 * mBoundApplication.appInfo.packageName 填成 com.android.shell
 * （shell uid 的包名），并补 mInitialApplication / mConfigurationController。
 */
@SuppressLint("PrivateApi,BlockedPrivateApi")
public final class Workarounds {


    private static final Class<?> ACTIVITY_THREAD_CLASS;
    private static final Object ACTIVITY_THREAD;

    static {
        try {
            ACTIVITY_THREAD_CLASS = Class.forName("android.app.ActivityThread");
            Method currentActivityThread = ACTIVITY_THREAD_CLASS.getMethod("currentActivityThread");
            ACTIVITY_THREAD = currentActivityThread.invoke(null);
            if (ACTIVITY_THREAD == null) {
                throw new AssertionError("ActivityThread not initialized");
            }
        } catch (Exception e) {
            throw new AssertionError(e);
        }
    }

    private Workarounds() {
        // not instantiable
    }

    /** 回填全部隐藏字段（必须在 getSystemContext / createVirtualDisplay 之前调用）。 */
    public static void apply() {
        if (Build.VERSION.SDK_INT >= 31) {
            // DisplayManagerGlobal.getDisplayInfoLocked() 会调用
            // ActivityThread.currentActivityThread().getConfiguration()，
            // mConfigurationController 为 null 会 NPE（Android 12+ 引入）
            fillConfigurationController();
        }
        fillAppInfo();
        fillAppContext();
    }

    /** mBoundApplication.appInfo.packageName = "com.android.shell"（uid 匹配校验用）。 */
    private static void fillAppInfo() {
        try {
            Class<?> appBindDataClass = Class.forName("android.app.ActivityThread$AppBindData");
            Constructor<?> ctor = appBindDataClass.getDeclaredConstructor();
            ctor.setAccessible(true);
            Object appBindData = ctor.newInstance();

            ApplicationInfo applicationInfo = new ApplicationInfo();
            applicationInfo.packageName = FakeContext.PACKAGE_NAME;

            Field appInfoField = appBindDataClass.getDeclaredField("appInfo");
            appInfoField.setAccessible(true);
            appInfoField.set(appBindData, applicationInfo);

            Field mBoundApplicationField =
                    ACTIVITY_THREAD_CLASS.getDeclaredField("mBoundApplication");
            mBoundApplicationField.setAccessible(true);
            mBoundApplicationField.set(ACTIVITY_THREAD, appBindData);
        } catch (Throwable t) {
            // workaround，失败不致命（日志记录便于排查）
            Server.d("fillAppInfo failed: " + t.getMessage());
        }
    }

    /** mInitialApplication = Instrumentation.newApplication(...)（部分系统服务要求）。 */
    private static void fillAppContext() {
        try {
            Application app = Instrumentation.newApplication(Application.class, FakeContext.get());
            Field mInitialApplicationField =
                    ACTIVITY_THREAD_CLASS.getDeclaredField("mInitialApplication");
            mInitialApplicationField.setAccessible(true);
            mInitialApplicationField.set(ACTIVITY_THREAD, app);
        } catch (Throwable t) {
            Server.d("fillAppContext failed: " + t.getMessage());
        }
    }

    /** mConfigurationController（Android 12+ DisplayManagerGlobal 查询配置需要）。 */
    private static void fillConfigurationController() {
        try {
            Class<?> configurationControllerClass = Class.forName("android.app.ConfigurationController");
            Class<?> activityThreadInternalClass = Class.forName("android.app.ActivityThreadInternal");
            Constructor<?> ctor =
                    configurationControllerClass.getDeclaredConstructor(activityThreadInternalClass);
            ctor.setAccessible(true);
            Object configurationController = ctor.newInstance(ACTIVITY_THREAD);

            Field field = ACTIVITY_THREAD_CLASS.getDeclaredField("mConfigurationController");
            field.setAccessible(true);
            field.set(ACTIVITY_THREAD, configurationController);
        } catch (Throwable t) {
            Server.d("fillConfigurationController failed: " + t.getMessage());
        }
    }
}
