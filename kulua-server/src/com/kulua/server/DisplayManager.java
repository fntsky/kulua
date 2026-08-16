package com.kulua.server;

/**
 * 虚拟显示器管理（多融合窗口）。
 *
 * 阶段 2 实现：每个 video 连接对应一个 VirtualDisplay + MediaCodec 编码器。
 * 阶段 1 仅为占位：DisplayManager 被 ControlChannel 引用，预留 display id 分配。
 */
public final class DisplayManager {

    private int nextDisplayId = 1;

    public DisplayManager() {
    }

    /** 分配一个新的内部显示器 id（阶段 2 关联 VirtualDisplay）。 */
    public synchronized int allocateDisplayId() {
        return nextDisplayId++;
    }
}
