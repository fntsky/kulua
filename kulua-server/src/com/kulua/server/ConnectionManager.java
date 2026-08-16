package com.kulua.server;

import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.util.Log;

import java.io.DataInputStream;
import java.io.IOException;
import java.io.InputStream;

/**
 * 连接管理：监听 `localabstract:scrcpy_<scid>`，按首字节握手分派连接。
 *
 * 握手协议：每个连接第一个字节声明类型
 * - 0x01 control：控制通道（剪贴板 / 输入注入 / START_APP / 显示器管理）
 * - 0x02 audio：音频流（阶段 3 实现）
 * - 0x03 video：视频流，连接驱动显示器创建：
 *   连接后先读 6 字节创建请求（width u16be, height u16be, dpi u16be），
 *   然后 server 创建 VirtualDisplay + 编码器并向该 socket 推流
 */
public final class ConnectionManager {

    public static final int TYPE_CONTROL = 0x01;
    public static final int TYPE_AUDIO = 0x02;
    public static final int TYPE_VIDEO = 0x03;

    private static final String TAG = "kulua-server";

    private final Options options;
    private final DisplayRegistry displayManager;

    public ConnectionManager(Options options) {
        this.options = options;
        this.displayManager = new DisplayRegistry();
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
                        Log.i(TAG, "audio connection");
                        handleAudioConnection(socket);
                        break;
                    case TYPE_VIDEO:
                        Log.i(TAG, "video connection");
                        handleVideoConnection(socket);
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

    /** 音频连接：捕获系统播放声音并推流，直到连接断开。 */
    private void handleAudioConnection(LocalSocket socket) throws IOException {
        AudioCapture capture = new AudioCapture(socket);
        try {
            capture.start();
            // 等待连接断开（客户端关闭时 read 返回 -1）
            while (socket.getInputStream().read() >= 0) {
                // drain
            }
        } finally {
            Log.i(TAG, "audio connection closed");
            capture.stop();
        }
    }

    /**
     * 视频连接：读 6B 创建请求 → 创建虚拟显示器 + 编码器 → 推流直到断开。
     * 连接断开时销毁对应显示器。
     */
    private void handleVideoConnection(LocalSocket socket) throws IOException {
        DataInputStream input = new DataInputStream(socket.getInputStream());
        int width = input.readUnsignedShort();
        int height = input.readUnsignedShort();
        int dpi = input.readUnsignedShort();
        Log.i(TAG, "create video stream " + width + "x" + height + "@" + dpi);

        DisplayRegistry.DisplayEntry entry = displayManager.create(width, height, dpi);
        // 先回 4B displayId（大端），客户端用它发起输入注入/RESIZE/START_APP
        socket.getOutputStream().write(new byte[]{
                (byte) (entry.id >>> 24), (byte) (entry.id >>> 16),
                (byte) (entry.id >>> 8), (byte) entry.id,
        });
        socket.getOutputStream().flush();
        DisplayEncoder encoder = new DisplayEncoder(entry.id, socket, displayManager,
                width, height, dpi);
        displayManager.attachEncoder(entry.id, encoder);
        try {
            encoder.start();
            // 推流直到连接断开（编码循环退出后 socket 关闭）
            // 编码循环结束条件：socket 写失败（客户端断开）
            // 等待编码循环结束：通过轮询 socket 输入（对端关闭时读返回 -1）
            while (true) {
                int b = input.read();
                if (b < 0) {
                    break;
                }
            }
        } finally {
            Log.i(TAG, "video connection closed, destroy display #" + entry.id);
            displayManager.destroy(entry.id);
        }
    }
}
