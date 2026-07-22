#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use crate::{
    types::{AdbError, Device},
    wireless_pair,
};

/// Trait abstracting ADB CLI operations.
///
/// Allows swapping the real `AdbCmd` implementation for testing or alternative backends.
/// `Send + Sync` is required so that implementations can be shared across threads
/// (e.g. background device-refresh loop in `Core::run`).
pub trait AdbOps: Send + Sync {
    #[allow(dead_code)]
    fn check(&self) -> Result<(), AdbError>;
    #[allow(dead_code)]
    fn devices(&self) -> Result<Vec<Device>, AdbError>;
    fn wireless_pair(
        &self,
        addr: &str,
        info: &wireless_pair::WirelessPairing,
    ) -> Result<(), AdbError>;
    fn connect(&self, addr: &str) -> Result<(), AdbError>;
    fn push(&self, device: &Device, local: &str, remote: &str) -> Result<(), AdbError>;
    fn forward(
        &self,
        device: &Device,
        local_port: u16,
        remote_abstract: &str,
    ) -> Result<(), AdbError>;
    fn spawn_shell(
        &self,
        device: &Device,
        shell_args: &[&str],
    ) -> Result<std::process::Child, AdbError>;
    /// 启动 `adb track-devices` 长连接进程，其 stdout 输出可逐行读取。
    fn track_devices(&self) -> Result<std::process::Child, AdbError>;
    fn run(&self, args: &[&str]) -> Result<std::process::Output, AdbError>;
}

#[derive(Clone)]
pub struct AdbCmd {
    adb_path: PathBuf,
}

impl AdbOps for AdbCmd {
    fn check(&self) -> Result<(), AdbError> {
        let output = Command::new(&self.adb_path)
            .arg("version")
            .output()
            .map_err(|_| AdbError::AdbNotFound {
                tried: self.adb_path.display().to_string(),
            })?;
        if !output.status.success() {
            return Err(AdbError::AdbNotFound {
                tried: self.adb_path.display().to_string(),
            });
        }
        Ok(())
    }

    fn devices(&self) -> Result<Vec<Device>, AdbError> {
        let output = self.run(&["devices", "-l"])?;
        self.parse_devices(&output.stdout)
    }

    fn wireless_pair(
        &self,
        addr: &str,
        info: &wireless_pair::WirelessPairing,
    ) -> Result<(), AdbError> {
        let output = self.run(&["pair", addr, &info.psk])?;
        // run() 已经检查了 exit code == 0，走到这里一定成功了
        let _text = String::from_utf8_lossy(&output.stdout);
        Ok(())
    }
    fn connect(&self, addr: &str) -> Result<(), AdbError> {
        let output = self.run(&["connect", addr])?;

        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        if !output.status.success() {
            return Err(AdbError::CommandFailed(text));
        }

        let lower = text.to_lowercase();

        if lower.contains("connected to") || lower.contains("already connected") {
            return Ok(());
        }

        Err(AdbError::CommandFailed(text))
    }

    fn push(&self, device: &Device, local: &str, remote: &str) -> Result<(), AdbError> {
        let serial = &device.serial;
        let output = Command::new(&self.adb_path)
            .args(["-s", serial, "push", local, remote])
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => AdbError::AdbNotFound {
                    tried: self.adb_path.display().to_string(),
                },
                _ => AdbError::Io(e),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AdbError::CommandFailed(format!(
                "adb -s {} push {} {}: {}",
                serial,
                local,
                remote,
                stderr.trim()
            )));
        }

        // exit code == 0 即成功
        Ok(())
    }

    fn forward(
        &self,
        device: &Device,
        local_port: u16,
        remote_abstract: &str,
    ) -> Result<(), AdbError> {
        let serial = &device.serial.as_str();
        let _ = self.run(&[
            "-s",
            serial,
            "forward",
            &format!("tcp:{}", local_port),
            &format!("localabstract:{}", remote_abstract),
        ])?;
        Ok(())
    }

    fn spawn_shell(
        &self,
        device: &Device,
        shell_args: &[&str],
    ) -> Result<std::process::Child, AdbError> {
        let serial = &device.serial.as_str();
        let mut cmd = Command::new(&self.adb_path);
        cmd.args(&["-s", serial, "shell"]);
        cmd.args(shell_args);
        #[cfg(windows)]
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        // pipe stdout/stderr 以便捕获 shell 内的报错信息
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().map_err(AdbError::Io)?;
        Ok(child)
    }
    fn track_devices(&self) -> Result<std::process::Child, AdbError> {
        let mut cmd = std::process::Command::new(&self.adb_path);
        cmd.arg("track-devices")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        cmd.spawn().map_err(AdbError::Io)
    }

    fn run(&self, args: &[&str]) -> Result<std::process::Output, AdbError> {
        let output = Command::new(&self.adb_path)
            .args(args)
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => AdbError::AdbNotFound {
                    tried: self.adb_path.display().to_string(),
                },

                _ => AdbError::Io(e),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            return Err(AdbError::CommandFailed(format!(
                "adb {}: {}",
                args.join(" "),
                stderr.trim()
            )));
        }

        Ok(output)
    }
}

impl AdbCmd {
    pub fn new() -> Self {
        #[cfg(windows)]
        let local = "./adb.exe";
        #[cfg(not(windows))]
        let local = "./adb";

        if std::path::Path::new(local).exists() {
            return Self {
                adb_path: PathBuf::from(local),
            };
        }
        // 兜底：使用 PATH 中的 adb；若也不存在，run() 会给出友好提示
        Self {
            adb_path: PathBuf::from("adb"),
        }
    }

    #[allow(dead_code)]
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            adb_path: path.into(),
        }
    }

    pub fn parse_devices(&self, stdout: &[u8]) -> Result<Vec<Device>, AdbError> {
        crate::protocol::devices::parse_devices(stdout)
    }
}


#[cfg(test)]
pub mod mock {
    use super::AdbOps;
    use crate::types::{AdbError, Device};
    use crate::wireless_pair;
    use std::sync::Mutex;

    /// Mock adb for controlled testing.
    ///
    /// Default: all operations succeed with empty/zero values.
    /// Set `fail_*` to `true` to make the corresponding method return an error.
    /// Use `calls` to assert which methods were invoked.
    pub struct MockAdb {
        /// Devices returned by `devices()`
        pub devices_result: Vec<Device>,
        pub fail_check: bool,
        pub fail_devices: bool,
        pub fail_wireless_pair: bool,
        pub fail_connect: bool,
        pub fail_push: bool,
        pub fail_forward: bool,
        /// Track which methods were called (for assertions)
        pub calls: Mutex<Vec<&'static str>>,
        /// Dummy successful output for `run()`
        default_output: std::process::Output,
    }

    impl MockAdb {
        pub fn new() -> Self {
            let status = std::process::Command::new("cmd")
                .arg("/c")
                .status()
                .expect("spawn mock dummy cmd process");
            Self {
                devices_result: vec![],
                fail_check: false,
                fail_devices: false,
                fail_wireless_pair: false,
                fail_connect: false,
                fail_push: false,
                fail_forward: false,
                calls: Mutex::new(vec![]),
                default_output: std::process::Output {
                    status,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
            }
        }
    }

    impl AdbOps for MockAdb {
        fn check(&self) -> Result<(), AdbError> {
            self.calls.lock().unwrap().push("check");
            if self.fail_check {
                Err(AdbError::CommandFailed("mock: check failed".into()))
            } else {
                Ok(())
            }
        }

        fn devices(&self) -> Result<Vec<Device>, AdbError> {
            self.calls.lock().unwrap().push("devices");
            if self.fail_devices {
                Err(AdbError::CommandFailed("mock: devices failed".into()))
            } else {
                Ok(self.devices_result.clone())
            }
        }

        fn wireless_pair(
            &self,
            _addr: &str,
            _info: &wireless_pair::WirelessPairing,
        ) -> Result<(), AdbError> {
            self.calls.lock().unwrap().push("wireless_pair");
            if self.fail_wireless_pair {
                Err(AdbError::CommandFailed("mock: wireless_pair failed".into()))
            } else {
                Ok(())
            }
        }

        fn connect(&self, _addr: &str) -> Result<(), AdbError> {
            self.calls.lock().unwrap().push("connect");
            if self.fail_connect {
                Err(AdbError::CommandFailed("mock: connect failed".into()))
            } else {
                Ok(())
            }
        }

        fn push(&self, _device: &Device, _local: &str, _remote: &str) -> Result<(), AdbError> {
            self.calls.lock().unwrap().push("push");
            if self.fail_push {
                Err(AdbError::CommandFailed("mock: push failed".into()))
            } else {
                Ok(())
            }
        }

        fn forward(
            &self,
            _device: &Device,
            _local_port: u16,
            _remote_abstract: &str,
        ) -> Result<(), AdbError> {
            self.calls.lock().unwrap().push("forward");
            if self.fail_forward {
                Err(AdbError::CommandFailed("mock: forward failed".into()))
            } else {
                Ok(())
            }
        }

        fn spawn_shell(
            &self,
            _device: &Device,
            _shell_args: &[&str],
        ) -> Result<std::process::Child, AdbError> {
            self.calls.lock().unwrap().push("spawn_shell");
            let dummy = std::process::Command::new("cmd")
                .arg("/c")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(AdbError::Io)?;
            Ok(dummy)
        }
        fn track_devices(&self) -> Result<std::process::Child, AdbError> {
            self.calls.lock().unwrap().push("track_devices");
            let dummy = std::process::Command::new("cmd")
                .arg("/c")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .map_err(AdbError::Io)?;
            Ok(dummy)
        }

        fn run(&self, _args: &[&str]) -> Result<std::process::Output, AdbError> {
            self.calls.lock().unwrap().push("run");
            Ok(self.default_output.clone())
        }
    }
}
