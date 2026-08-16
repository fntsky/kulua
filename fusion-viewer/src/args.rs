//! 命令行参数解析（纯函数，便于单测）。
//!
//! 用法：`fusion-viewer.exe --serial <serial> --package <pkg> [--label <名称>] --jar <jar路径>`

/// viewer 启动参数。
#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    /// ADB 设备地址（`adb -s <serial>`）
    pub serial: String,
    /// 要启动的应用包名
    pub package: String,
    /// 窗口标题中的应用名（缺省用包名）
    pub label: String,
    /// 本地 scrcpy-server jar 路径（部署到设备用）
    pub jar: String,
    /// 初始虚拟显示器尺寸，如 `1280x960/160`（缺省用默认值）
    pub display: String,
    /// 视频码率（bps）
    pub video_bit_rate: u32,
}

impl Args {
    /// 窗口标题：`应用名 — 设备地址`
    pub fn window_title(&self) -> String {
        let name = if self.label.is_empty() {
            &self.package
        } else {
            &self.label
        };
        format!("{} — {}", name, self.serial)
    }
}

/// 默认虚拟显示器尺寸/DPI（与 scrcpy `-x` 模式默认一致）。
pub const DEFAULT_DISPLAY: &str = "1280x960/160";

/// 默认视频码率 8Mbps。
pub const DEFAULT_VIDEO_BIT_RATE: u32 = 8_000_000;

/// 解析命令行参数。
///
/// 支持 `--key value` 与 `--key=value` 两种形式；未知参数报错。
pub fn parse_args<I: IntoIterator<Item = String>>(iter: I) -> Result<Args, String> {
    let mut serial = None;
    let mut package = None;
    let mut label = String::new();
    let mut jar = None;
    let mut display = DEFAULT_DISPLAY.to_string();
    let mut video_bit_rate = DEFAULT_VIDEO_BIT_RATE;

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
            "--serial" => serial = Some(value()?),
            "--package" => package = Some(value()?),
            "--label" => label = value()?,
            "--jar" => jar = Some(value()?),
            "--display" => display = value()?,
            "--video-bit-rate" => {
                let v = value()?;
                video_bit_rate = v.parse().map_err(|_| format!("非法码率: {}", v))?;
            }
            other => return Err(format!("未知参数: {}", other)),
        }
    }

    let serial = serial.ok_or_else(|| "缺少 --serial 参数".to_string())?;
    let package = package.ok_or_else(|| "缺少 --package 参数".to_string())?;
    let jar = jar.ok_or_else(|| "缺少 --jar 参数".to_string())?;

    Ok(Args {
        serial,
        package,
        label,
        jar,
        display,
        video_bit_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_all_options() {
        let a = parse_args(args(&[
            "--serial",
            "192.168.1.5:5555",
            "--package",
            "com.android.settings",
            "--label",
            "设置",
            "--jar",
            "scrcpy-server",
            "--display",
            "1024x768/160",
            "--video-bit-rate",
            "4000000",
        ]))
        .unwrap();
        assert_eq!(a.serial, "192.168.1.5:5555");
        assert_eq!(a.package, "com.android.settings");
        assert_eq!(a.label, "设置");
        assert_eq!(a.jar, "scrcpy-server");
        assert_eq!(a.display, "1024x768/160");
        assert_eq!(a.video_bit_rate, 4_000_000);
    }

    #[test]
    fn supports_equals_form() {
        let a = parse_args(args(&[
            "--serial=192.168.1.5:5555",
            "--package=com.android.settings",
            "--jar=scrcpy-server",
        ]))
        .unwrap();
        assert_eq!(a.serial, "192.168.1.5:5555");
        assert_eq!(a.package, "com.android.settings");
    }

    #[test]
    fn applies_defaults() {
        let a = parse_args(args(&["--serial", "x", "--package", "pkg", "--jar", "jar"])).unwrap();
        assert_eq!(a.label, "");
        assert_eq!(a.display, DEFAULT_DISPLAY);
        assert_eq!(a.video_bit_rate, DEFAULT_VIDEO_BIT_RATE);
    }

    #[test]
    fn window_title_uses_label_or_package() {
        let with_label = parse_args(args(&[
            "--serial",
            "s",
            "--package",
            "pkg",
            "--label",
            "应用名",
            "--jar",
            "jar",
        ]))
        .unwrap();
        assert_eq!(with_label.window_title(), "应用名 — s");

        let without_label =
            parse_args(args(&["--serial", "s", "--package", "pkg", "--jar", "jar"])).unwrap();
        assert_eq!(without_label.window_title(), "pkg — s");
    }

    #[test]
    fn missing_required_is_error() {
        assert!(parse_args(args(&["--package", "pkg", "--jar", "jar"])).is_err());
        assert!(parse_args(args(&["--serial", "s", "--jar", "jar"])).is_err());
        assert!(parse_args(args(&["--serial", "s", "--package", "pkg"])).is_err());
    }

    #[test]
    fn unknown_option_is_error() {
        assert!(
            parse_args(args(&[
                "--serial",
                "s",
                "--package",
                "pkg",
                "--jar",
                "jar",
                "--bogus",
                "x",
            ]))
            .is_err()
        );
    }

    #[test]
    fn missing_value_is_error() {
        assert!(parse_args(args(&["--serial"])).is_err());
    }

    #[test]
    fn invalid_bit_rate_is_error() {
        assert!(
            parse_args(args(&[
                "--serial",
                "s",
                "--package",
                "pkg",
                "--jar",
                "jar",
                "--video-bit-rate",
                "abc",
            ]))
            .is_err()
        );
    }
}
