package com.kulua.server;

import android.content.Context;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;

import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.PrintStream;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.List;

/**
 * 应用枚举一次性模式（`list_apps=true`）：输出格式与官方 scrcpy 一致，
 * 供 Rust 侧 apps.rs 解析器复用。
 *
 * 官方格式（LogUtils.buildAppListMessage 对照）：
 * - 行前缀 ` * `（系统应用）/ ` - `（普通应用）
 * - 名称列补位到 30 列（Java String.length()，UTF-16 码元），后跟 ` <包名>`
 * - 名称 ≥30 字符时包名换行：`\n   ` + 30 空格 + ` <包名>`
 */
public final class AppLister {

    private static final int COLUMN = 30;

    private AppLister() {
        // not instantiable
    }

    /** 枚举可启动应用并打印（stdout），进程随即退出。 */
    public static void runAndExit() {
        PrintStream out = new PrintStream(new FileOutputStream(FileDescriptor.out));
        try {
            Context context = Clipboard.getContext();
            PackageManager pm = context.getPackageManager();
            List<AppInfo> apps = new ArrayList<>();
            for (ApplicationInfo appInfo : pm.getInstalledApplications(PackageManager.GET_META_DATA)) {
                if (!appInfo.enabled) {
                    continue;
                }
                Intent launchIntent = pm.getLaunchIntentForPackage(appInfo.packageName);
                if (launchIntent == null) {
                    launchIntent = pm.getLeanbackLaunchIntentForPackage(appInfo.packageName);
                }
                if (launchIntent == null) {
                    continue;
                }
                String name = pm.getApplicationLabel(appInfo).toString();
                boolean system = (appInfo.flags & ApplicationInfo.FLAG_SYSTEM) != 0;
                apps.add(new AppInfo(appInfo.packageName, name, system));
            }
            // 排序：系统在前 → 名称 → 包名（与官方一致）
            Collections.sort(apps, new Comparator<AppInfo>() {
                @Override
                public int compare(AppInfo a, AppInfo b) {
                    int cmp = Boolean.compare(b.system, a.system);
                    if (cmp != 0) {
                        return cmp;
                    }
                    cmp = a.name.compareTo(b.name);
                    if (cmp != 0) {
                        return cmp;
                    }
                    return a.packageName.compareTo(b.packageName);
                }
            });

            StringBuilder builder = new StringBuilder("List of apps:");
            for (AppInfo app : apps) {
                builder.append("\n ");
                builder.append(app.system ? "* " : "- ");
                builder.append(app.name);
                int padding = COLUMN - app.name.length();
                if (padding > 0) {
                    appendSpaces(builder, padding);
                } else {
                    builder.append("\n   ");
                    appendSpaces(builder, COLUMN);
                }
                builder.append(" ").append(app.packageName);
            }
            out.println(builder);
        } catch (Exception e) {
            out.println("List of apps:");
            out.println(" * 无法枚举应用: " + e);
        } finally {
            out.flush();
        }
    }

    private static void appendSpaces(StringBuilder builder, int count) {
        for (int i = 0; i < count; i++) {
            builder.append(' ');
        }
    }

    private static final class AppInfo {
        final String packageName;
        final String name;
        final boolean system;

        AppInfo(String packageName, String name, boolean system) {
            this.packageName = packageName;
            this.name = name;
            this.system = system;
        }
    }
}
