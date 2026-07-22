use crate::notification::NotifInfo;
use std::collections::HashSet;

/// 解析 `adb shell cmd notification list` 的输出，提取所有键名。
///
/// 输入示例（每行一个键名）：
/// ```text
/// 0|com.example.app|12345|null|10001
/// 0|com.other.app|67890|null|10002
/// ```
pub fn parse_notification_list(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                None
            } else {
                Some(line.to_string())
            }
        })
        .collect()
}

/// 从 `"类型 (值)"` 或 `"(值)"` 格式中提取括号内的值。
///
/// 例如 `String (测试通知)` → `测试通知`，`Boolean (true)` → `true`。
/// 如果值没有括号包裹则原样返回，括号内为空则返回 None。
fn extract_string_value(line: &str) -> Option<String> {
    // 找最后一个 '='
    let eq_pos = line.find('=')?;
    let after_eq = line[eq_pos + 1..].trim();

    if after_eq.is_empty() {
        return None;
    }

    // 检查末尾是否有 )
    if after_eq.ends_with(')') {
        // 找最内层 '('
        if let Some(start) = after_eq.rfind('(') {
            let inner = after_eq[start + 1..after_eq.len() - 1].trim();
            if inner.is_empty() {
                return None;
            }
            return Some(inner.to_string());
        }
    }

    // 没有括号包裹，返回原始值
    Some(after_eq.to_string())
}

/// 解析 `adb shell "cmd notification get '<key>'"` 的输出。
///
/// 实际输出以 `NotificationRecord(0x...: pkg=...` 开头，`pkg=` 在第一行中间，
/// extras 段包含 `android.title=String (...)`, `android.text=String (...)`` 等。
pub fn parse_notification_detail(output: &str, key: &str) -> Option<NotifInfo> {
    let mut pkg = None;
    let mut title = None;
    let mut body = None;

    for line in output.lines() {
        let line = line.trim();

        // 从整行中找 pkg=...（可能在行首，也可能在 NotificationRecord(...: pkg=...) 中间）
        if pkg.is_none() {
            if let Some(pos) = line.find("pkg=") {
                let val = line[pos + 4..]
                    .split(|c: char| c.is_whitespace())
                    .next()
                    .unwrap_or("");
                if !val.is_empty() {
                    pkg = Some(val.to_string());
                }
            }
        }

        if title.is_none() && line.contains("android.title=") {
            title = extract_string_value(line);
        } else if body.is_none() && line.contains("android.text=") {
            body = extract_string_value(line);
        } else if body.is_none() && line.contains("android.bigText=") {
            body = extract_string_value(line);
        }
    }

    let package = pkg?;
    Some(NotifInfo {
        key: key.to_string(),
        package,
        title,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_notification_list_normal() {
        let output = "0|com.example.app|12345|null|10001
0|com.other.app|67890|null|10002
";
        let keys = parse_notification_list(output);
        assert_eq!(keys.len(), 2);
        assert!(keys.contains("0|com.example.app|12345|null|10001"));
        assert!(keys.contains("0|com.other.app|67890|null|10002"));
    }

    #[test]
    fn test_parse_notification_list_empty() {
        let output = "";
        let keys = parse_notification_list(output);
        assert!(keys.is_empty());
    }

    #[test]
    fn test_parse_notification_list_all_lines_as_keys() {
        // 新格式：每行非空行都是键名
        let output = "0|com.a|1|null|100\n0|com.b|2|null|101\n";
        let keys = parse_notification_list(output);
        assert_eq!(keys.len(), 2);
        assert!(keys.contains("0|com.a|1|null|100"));
        assert!(keys.contains("0|com.b|2|null|101"));
    }

    #[test]
    fn test_parse_notification_detail_full() {
        let output = "pkg=com.example.app
opts=0
key=0|com.example.app|12345|null|10001
extras={
  android.title=String (Test Title)
  android.text=String (Hello World)
}
";
        let info = parse_notification_detail(output, "0|com.example.app|12345|null|10001");
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.package, "com.example.app");
        assert_eq!(info.title.as_deref(), Some("Test Title"));
        assert_eq!(info.body.as_deref(), Some("Hello World"));
    }

    #[test]
    fn test_parse_notification_detail_missing_fields() {
        let output =
            "NotificationRecord(0x123: pkg=com.example.app user=UserHandle{0} id=999 tag=null)
  extras={
    android.title=String (Missing Body)
  }
";
        let info = parse_notification_detail(output, "0|com.example.app|999|null|10001");
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.package, "com.example.app");
        assert_eq!(info.title.as_deref(), Some("Missing Body"));
        assert!(info.body.is_none());
    }

    #[test]
    fn test_parse_notification_detail_fallback_bigtext() {
        let output = "pkg=com.example.app
opts=0
key=0|com.example.app|12345|null|10001
extras={
  android.title=String (BigText Title)
  android.bigText=String (Fallback body)
}
";
        let info = parse_notification_detail(output, "0|com.example.app|12345|null|10001");
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.package, "com.example.app");
        assert_eq!(info.title.as_deref(), Some("BigText Title"));
        assert_eq!(info.body.as_deref(), Some("Fallback body"));
    }

    #[test]
    fn test_parse_notification_detail_actual_format() {
        // 真实输出格式：第一行 pkg= 在 NotificationRecord(...:) 中间
        let output = "NotificationRecord(0x07401e30: pkg=com.fantasky.sync.phone.server user=UserHandle{0} id=999 tag=null importance=3 key=0|com.fantasky.sync.phone.server|999|null|10470: ...)
  extras={
    android.title=String (fantasky)
    android.text=String (real notification text)
  }
";
        let info =
            parse_notification_detail(output, "0|com.fantasky.sync.phone.server|999|null|10470");
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.package, "com.fantasky.sync.phone.server");
        assert_eq!(info.title.as_deref(), Some("fantasky"));
        assert_eq!(info.body.as_deref(), Some("real notification text"));
    }

    #[test]
    fn test_parse_notification_detail_no_package() {
        let output = "opts=0\nkey=0|key|null|10001\nextras={}\n";
        let info = parse_notification_detail(output, "0|key|null|10001");
        assert!(info.is_none());
    }
}
