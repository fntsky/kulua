package com.kulua.server;

import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.util.Log;

import java.io.IOException;
import java.io.InputStream;

/**
 * 连接管理：监听 `localabstract:scrcpy_<scid>`，按首字节握手分派连接。
 *
 * 握手协议：每个连接第一个字节声明类型
 * - 0x01 control：控制通道（剪贴板 / 输入注入 / START_APP / 显示器管理）
 * - 0x02 audio：音频流（阶段 3 实现）
 * - 0x03 video：视频流，连接驱动显示器创建（阶段 2 实现）
 */
public final class ConnectionManager {

    public static final int TYPE_CONTROL = 0x01;
    public static final int TYPE_AUDIO = 0x02;
    public static final int TYPE_VIDEO = 0x03;

    private static final String TAG = "kulua-server";

    private final Options options;
    private final DisplayManager displayManager;

    public ConnectionManager(Options options) {
        this.options = options;
        this.displayManager = new DisplayManager();
    }

    public void loop() throws IOException {
        String socketName = "scrcpy_" + options.scid;
        Log.i(TAG, "listening on " + socketName);
        try (LocalServerSocket serverSocket = new LocalServerSocket(socketName)) {
            while (true) {
                LocalSocket socket = serverSocket.accept();
                handleConnection(socket);
            }
        }
    }

    private void handleConnection(LocalSocket socket) {
        // 每个连接独立线程，互不阻塞
        new Thread(() -> {
            try {
                InputStream input = socket.getInputStream();
                int type = input.read();
                switch (type) {
                    case TYPE_CONTROL:
                        Log.i(TAG, "control connection");
                        new ControlChannel(socket, displayManager).run();
                        break;
                    case TYPE_AUDIO:
                        Log.i(TAG, "audio connection (TODO: 阶段 3)");
                        break;
                    case TYPE_VIDEO:
                        Log.i(TAG, "video connection (TODO: 阶段 2)");
                        break;
                    default:
                        Log.w(TAG, "unknown connection type: " + type);
                        break;
                }
            } catch (IOException e) {
                Log.w(TAG, "connection error", e);
            } finally {
                try {
                    socket.close();
                } catch (IOException ignored) {
                    // ignore
                }
            }
        }).start();
    }
}
