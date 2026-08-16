package com.kulua.server;

import android.annotation.TargetApi;
import android.content.AttributionSource;
import android.content.Context;
import android.content.ContextWrapper;
import android.os.Process;

/**
 * 伪 Context：把系统 Context 的包名伪装成 shell 包名。
 *
 * 为什么需要：app_process 以 shell uid 运行，但系统 Context 的包名是
 * "android"（或类似），`DisplayManager.createVirtualDisplay` 会校验
 * packageName 与 calling uid 匹配，不匹配抛 SecurityException
 * （"packageName must match the calling uid"）。官方 scrcpy 用 FakeContext
 * 解决（包名设为 com.android.shell，正好是 shell uid 的包名）。
 */
public final class FakeContext extends ContextWrapper {

    public static final String PACKAGE_NAME = "com.android.shell";

    private static final FakeContext INSTANCE = new FakeContext();

    public static FakeContext get() {
        return INSTANCE;
    }

    private FakeContext() {
        super(Clipboard.getContext());
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
}
