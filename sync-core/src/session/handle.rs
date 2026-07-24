use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::adb_cmd::AdbOps;
use crate::types::Device;
use tokio::sync::oneshot;

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
