//! 命令行参数解析（纯函数，便于单测）。
//!
//! 用法：`fusion-viewer.exe --connect <ip:port> --package <pkg> [--label <名称>] [--display <WxH/DPI>] [--scale <系数>]`
//!
//! 连接模式：viewer 不部署 server，而是直连 daemon session 已启动的
//! kulua-server（`phone IP:UDP端口`，无 adb forward），通过 CreateDisplay
//! 控制消息创建虚拟显示器。

use std::net::SocketAddr;

/// viewer 启动参数。
#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    /// phone 直连地址（IP:UDP 端口，session 已部署 server）
    pub addr: SocketAddr,
    /// 要启动的应用包名
    pub package: String,
    /// 窗口标题中的应用名（缺省用包名）
    pub label: String,
    /// 初始虚拟显示器尺寸，如 `1280x960/160`（缺省用默认值）
    pub display: String,
    /// 虚拟显示器分辨率相对窗口物理尺寸的缩放系数
    pub scale: f32,
}

impl Args {
    /// 窗口标题：`应用名`（label 缺省时用包名）
    pub fn window_title(&self) -> String {
        if self.label.is_empty() {
            self.package.clone()
        } else {
            self.label.clone()
        }
    }
}

/// 默认虚拟显示器尺寸/DPI（与 scrcpy `-x` 模式默认一致）。
pub const DEFAULT_DISPLAY: &str = "1280x960/160";

/// 默认缩放系数：视频分辨率 = 窗口物理尺寸 × 1.0。
pub const DEFAULT_SCALE: f32 = 1.0;

/// 缩放系数允许范围。
pub const SCALE_MIN: f32 = 0.1;
pub const SCALE_MAX: f32 = 4.0;

/// 解析命令行参数。
///
/// 支持 `--key value` 与 `--key=value` 两种形式；未知参数报错。
pub fn parse_args<I: IntoIterator<Item = String>>(iter: I) -> Result<Args, String> {
    let mut addr = None;
    let mut package = None;
    let mut label = String::new();
    let mut display = DEFAULT_DISPLAY.to_string();
    let mut scale = DEFAULT_SCALE;

    let mut iter = iter.into_iter();
    while let Some(arg) = iter.next() {
        let (key, inline_value) = match arg.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        let mut value = || -> Result<String, String> {
            if let Some(v) = &inline_value {
                Ok(v.clone())
            } else {
                iter.next().ok_or_else(|| format!("缺少参数值: {}", key))
            }
        };
        match key {
            "--connect" => {
                let v = value()?;
                addr = Some(
                    v.parse()
                        .map_err(|_| format!("非法地址（应为 ip:port）: {}", v))?,
                );
            }
            "--package" => package = Some(value()?),
            "--label" => label = value()?,
            "--display" => display = value()?,
            "--scale" => {
                let v = value()?;
                let s: f32 = v.parse().map_err(|_| format!("非法缩放系数: {}", v))?;
                if !(SCALE_MIN..=SCALE_MAX).contains(&s) {
                    return Err(format!(
                        "缩放系数 {} 超出范围 [{}, {}]",
                        s, SCALE_MIN, SCALE_MAX
                    ));
                }
                scale = s;
            }
            other => return Err(format!("未知参数: {}", other)),
        }
    }

    let addr = addr.ok_or_else(|| "缺少 --connect 参数".to_string())?;
    let package = package.ok_or_else(|| "缺少 --package 参数".to_string())?;

    Ok(Args {
        addr,
        package,
        label,
        display,
        scale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn addr() -> SocketAddr {
        "192.168.1.5:27183".parse().unwrap()
    }

    #[test]
    fn parses_all_options() {
        let a = parse_args(args(&[
            "--connect",
            "192.168.1.5:27183",
            "--package",
            "com.android.settings",
            "--label",
            "设置",
            "--display",
            "1024x768/160",
            "--scale",
            "0.5",
        ]))
        .unwrap();
        assert_eq!(a.addr, addr());
        assert_eq!(a.package, "com.android.settings");
        assert_eq!(a.label, "设置");
        assert_eq!(a.display, "1024x768/160");
        assert_eq!(a.scale, 0.5);
    }

    #[test]
    fn supports_equals_form() {
        let a = parse_args(args(&[
            "--connect=192.168.1.5:27183",
            "--package=com.android.settings",
        ]))
        .unwrap();
        assert_eq!(a.addr, addr());
        assert_eq!(a.package, "com.android.settings");
    }

    #[test]
    fn applies_defaults() {
        let a = parse_args(args(&[
            "--connect",
            "192.168.1.5:27183",
            "--package",
            "pkg",
        ]))
        .unwrap();
        assert_eq!(a.label, "");
        assert_eq!(a.display, DEFAULT_DISPLAY);
        assert_eq!(a.scale, DEFAULT_SCALE);
    }

    #[test]
    fn supports_scale_equals_form() {
        let a = parse_args(args(&[
            "--connect=192.168.1.5:27183",
            "--package=pkg",
            "--scale=2.0",
        ]))
        .unwrap();
        assert_eq!(a.scale, 2.0);
    }

    #[test]
    fn invalid_scale_is_error() {
        assert!(
            parse_args(args(&[
                "--connect",
                "1",
                "--package",
                "pkg",
                "--scale",
                "abc"
            ]))
            .is_err()
        );
        assert!(
            parse_args(args(&[
                "--connect",
                "1",
                "--package",
                "pkg",
                "--scale",
                "0"
            ]))
            .is_err()
        );
        assert!(
            parse_args(args(&[
                "--connect",
                "1",
                "--package",
                "pkg",
                "--scale",
                "10"
            ]))
            .is_err()
        );
    }

    #[test]
    fn window_title_uses_label_or_package() {
        let with_label = parse_args(args(&[
            "--connect",
            "192.168.1.5:27183",
            "--package",
            "pkg",
            "--label",
            "应用名",
        ]))
        .unwrap();
        assert_eq!(with_label.window_title(), "应用名");

        let without_label = parse_args(args(&[
            "--connect",
            "192.168.1.5:27183",
            "--package",
            "pkg",
        ]))
        .unwrap();
        assert_eq!(without_label.window_title(), "pkg");
    }

    #[test]
    fn missing_required_is_error() {
        assert!(parse_args(args(&["--package", "pkg"])).is_err());
        assert!(parse_args(args(&["--connect", "192.168.1.5:27183"])).is_err());
    }

    #[test]
    fn unknown_option_is_error() {
        assert!(
            parse_args(args(&[
                "--connect",
                "1",
                "--package",
                "pkg",
                "--bogus",
                "x"
            ]))
            .is_err()
        );
    }

    #[test]
    fn missing_value_is_error() {
        assert!(parse_args(args(&["--connect"])).is_err());
    }

    #[test]
    fn invalid_addr_is_error() {
        assert!(parse_args(args(&["--connect", "abc", "--package", "pkg"])).is_err());
    }
}
