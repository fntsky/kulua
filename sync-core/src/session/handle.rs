use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, Ordering};
use std::time::Duration;

use crate::adb_cmd::AdbOps;
use crate::types::Device;
use tokio::sync::oneshot;

// ── session 生命周期状态 ──
// 存储于 Handle 共享的 AtomicU8，session 在阶段边界写入，Core 每 tick 读取推送。
// 状态流转见 docs/session-state-design.md。

pub const SESSION_STATE_IDLE: u8 = 0;
pub const SESSION_STATE_CONNECTING: u8 = 1;
pub const SESSION_STATE_RUNNING: u8 = 2;
pub const SESSION_STATE_FAILED: u8 = 3;
pub const SESSION_STATE_STOPPED: u8 = 4;

/// 状态 → IPC 字符串
pub fn session_state_str(state: u8) -> &'static str {
    match state {
        SESSION_STATE_IDLE => "idle",
        SESSION_STATE_CONNECTING => "connecting",
        SESSION_STATE_RUNNING => "running",
        SESSION_STATE_FAILED => "failed",
        SESSION_STATE_STOPPED => "stopped",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_state_str() {
        assert_eq!(session_state_str(SESSION_STATE_IDLE), "idle");
        assert_eq!(session_state_str(SESSION_STATE_CONNECTING), "connecting");
        assert_eq!(session_state_str(SESSION_STATE_RUNNING), "running");
        assert_eq!(session_state_str(SESSION_STATE_FAILED), "failed");
        assert_eq!(session_state_str(SESSION_STATE_STOPPED), "stopped");
        assert_eq!(session_state_str(255), "unknown");
    }
}

/// 轻量 session 句柄，主循环用来发停止信号 + 等待任务结束
pub struct Handle {
    #[allow(dead_code)]
    pub device: Device,
    /// session 占用的 ADB 转发端口，用于超时后的强制清理
    pub port: u16,
    pub stop_tx: Option<oneshot::Sender<()>>,
    pub task: tokio::task::JoinHandle<()>,
    /// Core 通过此标志动态控制剪贴板同步
    pub clipboard_enabled: Arc<AtomicBool>,
    /// Core 通过此标志动态控制通知同步
    pub notification_enabled: Arc<AtomicBool>,
    /// Core 通过此标志动态控制音频开关（重启 session 后生效）
    pub audio_enabled: Arc<AtomicBool>,
    /// 音量百分比（0-100），Core 可实时调整
    pub volume: Arc<AtomicU16>,
    /// session 生命周期状态（session 写，Core tick 读并推送）
    pub session_state: Arc<AtomicU8>,
    /// 音频缓冲延迟（ms）：audio_task 写，Core tick 读推 UI
    pub audio_latency: Arc<AtomicU64>,
}
impl Handle {
    /// 当前生命周期状态
    pub fn state(&self) -> u8 {
        self.session_state.load(Ordering::SeqCst)
    }

    /// 写入生命周期状态
    pub fn set_state(&self, state: u8) {
        self.session_state.store(state, Ordering::SeqCst);
    }

    /// 状态字符串（IPC 推送用）
    pub fn state_str(&self) -> &'static str {
        session_state_str(self.state())
    }

    /// 是否处于 failed 墓碑态（可重试）
    pub fn is_failed(&self) -> bool {
        self.state() == SESSION_STATE_FAILED
    }
}
impl Handle {
    /// 停止 session（模仿官方关闭流程）。
    ///
    /// 1. 发停止信号 → session 主循环退出
    /// 2. session 内部 shutdown socket → server 收到 IO 错误 → 走 Java finally
    /// 3. 等待 task 退出（1s 看门狗）
    /// 4. 超时未退 → abort + ADB 强制清理
    pub async fn stop(&mut self, adb: &dyn AdbOps) {
        // (1) 发停止信号
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }

        // (2) 等待 session 自行清理（socket shutdown → server 应快速退出）
        //     官方 1s 看门狗，此处留 2s 给 Windows TCP 栈一点余量
        if tokio::time::timeout(Duration::from_secs(2), &mut self.task)
            .await
            .is_err()
        {
            // (3) 超时：task 可能卡住，强制 abort 后直接清理远程进程
            self.task.abort();
            eprintln!(
                "session {} stop timeout, force killing remote",
                self.device.serial
            );
            let kill_cmd = "kill -9 $(ps 2>/dev/null | grep com.genymobile.scrcpy | grep -v grep | awk '{print $2}') 2>/dev/null; true";
            let _ = adb.run(&["-s", &self.device.serial, "shell", kill_cmd]);
        }

        // (4) 清理 ADB 转发
        let _ = adb.run(&[
            "-s",
            &self.device.serial,
            "forward",
            "--remove",
            &format!("tcp:{}", self.port),
        ]);
    }
}
