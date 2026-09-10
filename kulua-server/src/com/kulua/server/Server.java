// 自研 Kulua server：设备端 app_process 服务（替代官方 scrcpy-server）。
//
// 架构（与官方 scrcpy 的关键区别）：
// - 一个 server 进程管理**多个虚拟显示器**（多融合窗口）
// - socket 模型：server 监听 `localabstract:scrcpy_<scid>`，
//   每个连接先发 1 字节类型握手：0x01=control / 0x02=audio / 0x03=video
// - video 连接驱动显示器创建：客户端连上后发 CREATE_DISPLAY{width,height,dpi}，
//   server 为该连接创建 VirtualDisplay + MediaCodec 编码器，视频流沿 scrcpy
//   帧格式（12B 帧头 + payload）送回
// - 控制消息布局沿用 scrcpy v4.0（输入注入/剪贴板/START_APP/RESIZE_DISPLAY），
//   扩展自定义消息：100=CREATE_DISPLAY 101=DESTROY_DISPLAY

package com.kulua.server;

import java.io.IOException;

public final class Server {

    /** logcat tag，固定不变（`logcat -s kulua-server` 过滤用；版本在消息尾部）。 */
    public static final String TAG = "kulua-server";

    /**
     * 服务端版本号：拼进每条日志消息尾部（[v0.8.0]），用于确认手机上跑的是
     * 哪个构建。协议/行为有变更时手动递增。
     */
    public static final String VERSION = "0.8.3";

    private static final String V_SUFFIX = " [v" + VERSION + "]";

    /** Log.i 包装：tag 固定 kulua-server，消息尾追加版本。 */
    public static void i(String msg) {
        android.util.Log.i(TAG, msg + V_SUFFIX);
    }

    /** Log.w 包装：同 {@link #i(String)}。 */
    public static void w(String msg) {
        android.util.Log.w(TAG, msg + V_SUFFIX);
    }

    /** Log.w 包装（带异常）：同 {@link #i(String)}。 */
    public static void w(String msg, Throwable t) {
        android.util.Log.w(TAG, msg + V_SUFFIX, t);
    }

    /** Log.d 包装：同 {@link #i(String)}。 */
    public static void d(String msg) {
        android.util.Log.d(TAG, msg + V_SUFFIX);
    }

    /** Log.e 包装（无异常）：同 {@link #i(String)}。 */
    public static void e(String msg) {
        android.util.Log.e(TAG, msg + V_SUFFIX);
    }

    /** Log.e 包装（带异常）：同 {@link #i(String)}。 */
    public static void e(String msg, Throwable t) {
        android.util.Log.e(TAG, msg + V_SUFFIX, t);
    }

    public static final String SERVER_PATH;

    static {
        String[] classPaths = System.getProperty("java.class.path").split(java.io.File.pathSeparator);
        SERVER_PATH = classPaths[0];
    }

    private Server() {
        // not instantiable
    }

    public static void main(String... args) {
        int status = 0;
        try {
            internalMain(args);
        } catch (Throwable t) {
            // stderr 随 daemon 的 `adb shell` stderr 转发到 PC 控制台（见 scrcpy.rs），
            // 没挂 logcat 时也能看到致命错误
            t.printStackTrace();
            android.util.Log.e(TAG, "server error", t);
            status = 1;
        } finally {
            System.exit(status);
        }
    }

    private static void internalMain(String... args) throws IOException {
        // app_process 直跑 main 没有 Looper，而 ActivityThread 构造需要它
        // （构造 Handler）。官方 scrcpy 同样在 main 里先 prepare（quitAllowed=true，
        // 见 Server.prepareMainLooper）。
        android.os.Looper.prepare();

        Options options = Options.parse(args);
        android.util.Log.i(TAG, "kulua-server v" + VERSION + " scid=" + options.scid);

        // 一次性模式：枚举应用后退出（不监听 socket）
        if (options.listApps) {
            AppLister.runAndExit();
            return;
        }

        // 预热系统 Context：必须由主线程（已 Looper.prepare）初始化 ActivityThread，
        // 若留给 clipboard-monitor 等后台线程首次触发，会因无 Looper 而崩溃
        // （ActivityThread 构造需要 Handler）
        Clipboard.getContext();
        // 回填 ActivityThread 隐藏字段（mBoundApplication 包名 = com.android.shell 等），
        // 否则 createVirtualDisplay 校验 uid 失败（"packageName must match the calling uid"）
        Workarounds.apply();
        // 轮询系统剪贴板，变化时推送给所有 control 连接（手机 → PC 方向）
        Clipboard.startMonitor();
        // UDP 直连服务器：phone 在 Wi-Fi 网络端口监听，接收所有客户端（daemon/fusion-viewer）
        UdpServer udpServer = new UdpServer(options);
        udpServer.loop();
    }
}
