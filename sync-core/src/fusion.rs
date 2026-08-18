//! 融合窗口管理（scrcpy 4.0 融合模式，完全自研客户端）。
//!
//! daemon 拉起自研 `fusion-viewer.exe`（winit 窗口 + FFmpeg 软解 + 自研控制协议注入），
//! 不依赖官方 scrcpy.exe。viewer 内部自行部署 scrcpy-server（`new_display` 虚拟显示器）
//! 并注入输入；每个应用一个 viewer 进程，daemon 负责进程生命周期：
//! 启动、每 tick 回收已退出窗口、退出时优雅关闭。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 窗口状态（IPC 推送用文本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    Running,
    Exited,
    Failed,
}

impl WindowState {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowState::Running => "running",
            WindowState::Exited => "exited",
            WindowState::Failed => "failed",
        }
    }
}

/// 融合窗口快照（IPC `app.windows-updated` 事件载荷）。
#[derive(Debug, Clone, PartialEq)]
pub struct AppWindowInfo {
    pub window_id: u64,
    pub serial: String,
    pub package_name: String,
    pub label: String,
    pub state: String,
}

/// 单个融合窗口（scrcpy.exe 子进程）。
struct FusionWindow {
    id: u64,
    serial: String,
    package_name: String,
    label: String,
    child: Option<std::process::Child>,
    state: WindowState,
}

/// 融合窗口管理器（Core 持有，主循环每 tick 回收已退出窗口）。
pub struct FusionManager {
    windows: HashMap<u64, FusionWindow>,
    next_id: u64,
}

impl FusionManager {
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
            next_id: 1,
        }
    }

    /// 查找 fusion-viewer.exe：daemon 同目录 → `FUSION_VIEWER_EXE` 环境变量 → PATH。
    pub fn find_viewer_exe() -> Option<PathBuf> {
        let exe_name = if cfg!(windows) {
            "fusion-viewer.exe"
        } else {
            "fusion-viewer"
        };

        // 1. daemon 同目录（发布包布局：daemon.exe + fusion-viewer.exe 平级）
        if let Ok(exe) = std::env::current_exe() {
            let candidate = exe
                .parent()
                .map(|dir| dir.join(exe_name))
                .filter(|p| p.exists());
            if let Some(candidate) = candidate {
                return Some(candidate);
            }
        }

        // 2. FUSION_VIEWER_EXE 环境变量（显式指定完整路径）
        if let Ok(path) = std::env::var("FUSION_VIEWER_EXE") {
            let candidate = PathBuf::from(path);
            if candidate.exists() {
                return Some(candidate);
            }
        }

        // 3. PATH
        if let Ok(path) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path) {
                let candidate = dir.join(exe_name);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }

        None
    }

    /// 是否可用（存在 fusion-viewer.exe 运行时）。
    pub fn is_supported(&self) -> bool {
        Self::find_viewer_exe().is_some()
    }

    /// 构建 fusion-viewer 命令行参数（纯函数，便于单测）。
    ///
    /// `addr` 为 `ip:port`（phone 直连地址，UDP）：
    /// - daemon session 统一部署 kulua-server（单进程多显示器），viewer 连接模式：
    ///   `--connect <addr>` 直连 session 的 UDP 端口，CreateDisplay 携带显示器参数
    /// - 音频/剪贴板继续由 Kulua session 负责（viewer 只做视频 + 输入注入）
    pub fn build_args(addr: &str, package: &str, label: &str) -> Vec<String> {
        let mut args = vec![
            "--connect".to_string(),
            addr.to_string(),
            "--package".to_string(),
            package.to_string(),
        ];
        if !label.is_empty() {
            args.push("--label".to_string());
            args.push(label.to_string());
        }
        args
    }

    /// 为指定设备打开一个应用的融合窗口，返回窗口 id。
    ///
    /// `addr` 为 phone 的直连地址（`ip:port`，由 Core 解析）。
    pub fn open_window(
        &mut self,
        serial: String,
        package_name: String,
        label: String,
        addr: String,
    ) -> Result<u64, String> {
        let exe = Self::find_viewer_exe().ok_or(
            "未找到 fusion-viewer.exe：请将其放入 daemon 同目录，或设置 FUSION_VIEWER_EXE 环境变量",
        )?;
        self.open_window_with_exe(&exe, serial, package_name, label, addr)
    }

    /// 使用指定 fusion-viewer 路径打开融合窗口（`open_window` 的内部实现，测试用）。
    fn open_window_with_exe(
        &mut self,
        exe: &std::path::Path,
        serial: String,
        package_name: String,
        label: String,
        addr: String,
    ) -> Result<u64, String> {
        let args = Self::build_args(&addr, &package_name, &label);
        let child = std::process::Command::new(exe)
            .args(&args)
            .spawn()
            .map_err(|e| format!("启动 fusion-viewer 失败: {}", e))?;

        let id = self.next_id;
        self.next_id += 1;
        self.windows.insert(
            id,
            FusionWindow {
                id,
                serial,
                package_name,
                label,
                child: Some(child),
                state: WindowState::Running,
            },
        );
        Ok(id)
    }

    /// 每 tick 回收已退出窗口，返回本 tick 退出的窗口（含最终状态），
    /// 调用方负责把事件推送给 UI。
    pub fn tick(&mut self) -> Vec<AppWindowInfo> {
        let mut exited = Vec::new();
        for window in self.windows.values_mut() {
            let Some(child) = window.child.as_mut() else {
                continue;
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    window.state = if status.success() {
                        WindowState::Exited
                    } else {
                        WindowState::Failed
                    };
                    window.child = None;
                }
                Ok(None) => {}
                Err(e) => {
                    // try_wait 系统错误（极少见）：保持运行，下个 tick 再试
                    eprintln!("fusion window {} try_wait error: {}", window.id, e);
                }
            }
        }
        for window in self.windows.values() {
            if window.state != WindowState::Running {
                exited.push(window.info());
            }
        }
        self.windows.retain(|_, w| w.state == WindowState::Running);
        exited
    }

    /// 当前运行中窗口的快照。
    pub fn windows_info(&self) -> Vec<AppWindowInfo> {
        self.windows
            .values()
            .filter(|w| w.state == WindowState::Running)
            .map(FusionWindow::info)
            .collect()
    }

    /// 优雅关闭所有窗口（daemon 退出时调用）。
    ///
    /// Windows 优先 `taskkill /PID <pid> /T`（不带 `/F`，向窗口发 WM_CLOSE 走官方
    /// 优雅退出流程），2s 超时后强杀。
    pub fn shutdown(&mut self) {
        let wait = Duration::from_secs(2);
        for window in self.windows.values_mut() {
            let Some(child) = window.child.as_mut() else {
                continue;
            };
            #[cfg(windows)]
            {
                // 未 wait 过，id() 不会 panic
                let pid = child.id();
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T"])
                    .output();
            }
            // 等待优雅退出
            let deadline = Instant::now() + wait;
            loop {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // 超时强杀
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        self.windows.clear();
    }
}

impl Default for FusionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl FusionWindow {
    fn info(&self) -> AppWindowInfo {
        AppWindowInfo {
            window_id: self.id,
            serial: self.serial.clone(),
            package_name: self.package_name.clone(),
            label: self.label.clone(),
            state: self.state.as_str().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_args_contains_viewer_flags() {
        let args = FusionManager::build_args("192.168.1.5:27183", "com.android.settings", "设置");
        assert_eq!(
            args,
            vec![
                "--connect",
                "192.168.1.5:27183",
                "--package",
                "com.android.settings",
                "--label",
                "设置",
            ]
        );
        // 空 label 不传 --label
        let no_label = FusionManager::build_args("192.168.1.5:27183", "pkg", "");
        assert_eq!(
            no_label,
            vec!["--connect", "192.168.1.5:27183", "--package", "pkg"]
        );
    }

    #[test]
    fn window_ids_are_unique_and_incrementing() {
        // 用立即退出的 dummy 进程验证 id 分配，不依赖测试环境有 fusion-viewer.exe
        let mut manager = FusionManager::new();
        let id1 = manager.open_window_with_exe(
            &dummy_exe(),
            "serial-1".into(),
            "com.android.settings".into(),
            "设置".into(),
            "192.168.1.5:27183".into(),
        );
        let id2 = manager.open_window_with_exe(
            &dummy_exe(),
            "serial-1".into(),
            "com.android.chrome".into(),
            "Chrome".into(),
            "192.168.1.5:27183".into(),
        );
        assert!(id1.is_ok(), "dummy exe 应能启动: {:?}", id1);
        assert!(id2.is_ok());
        assert_ne!(id1.unwrap(), id2.unwrap());
        assert_eq!(manager.windows_info().len(), 2);
    }

    /// 无害的 dummy 进程路径（立即退出），用于不依赖 scrcpy.exe 的窗口管理测试。
    fn dummy_exe() -> std::path::PathBuf {
        #[cfg(windows)]
        {
            std::path::PathBuf::from("cmd.exe")
        }
        #[cfg(not(windows))]
        {
            std::path::PathBuf::from("/bin/true")
        }
    }

    #[test]
    fn open_window_fails_when_exe_missing() {
        // 用不存在的 exe 路径确定性验证错误路径（不依赖测试环境是否有 fusion-viewer.exe）
        let mut manager = FusionManager::new();
        let result = manager.open_window_with_exe(
            std::path::Path::new("/nonexistent/fusion-viewer.exe"),
            "serial".into(),
            "com.android.settings".into(),
            "设置".into(),
            "192.168.1.5:27183".into(),
        );
        assert!(result.is_err(), "exe 不存在应返回错误");
        assert!(manager.windows_info().is_empty());
    }

    #[test]
    fn window_state_strings() {
        assert_eq!(WindowState::Running.as_str(), "running");
        assert_eq!(WindowState::Exited.as_str(), "exited");
        assert_eq!(WindowState::Failed.as_str(), "failed");
    }

    #[test]
    fn empty_manager_is_safe() {
        let mut manager = FusionManager::new();
        assert!(manager.tick().is_empty());
        assert!(manager.windows_info().is_empty());
        manager.shutdown(); // 空管理器关闭不应 panic
    }
}
