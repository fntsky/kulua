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
            android.util.Log.e("kulua", "server error", t);
            status = 1;
        } finally {
            System.exit(status);
        }
    }

    private static void internalMain(String... args) throws IOException {
        Options options = Options.parse(args);
        android.util.Log.i("kulua", "kulua-server " + options.scid);

        ConnectionManager manager = new ConnectionManager(options);
        manager.loop();
    }
}
