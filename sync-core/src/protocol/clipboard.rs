use crate::types::AdbError;
use std::io::Read;

/// 从 scrcpy 控制连接中读取一条设备剪贴板消息。
///
/// 协议格式：
/// - 1 byte type（0x00 = CLIPBOARD, 其他 = 忽略）
/// - 4 bytes big-endian text length
/// - N bytes UTF-8 text
///
/// 返回 `Ok(None)` 表示非剪贴板消息（已忽略），`Ok(Some(text))` 表示成功。
/// 返回 `Err` 表示读取失败（连接断开或协议错误）。
pub fn parse_clipboard_event<R: Read>(reader: &mut R) -> Result<Option<String>, AdbError> {
    let mut type_buf = [0u8; 1];
    reader.read_exact(&mut type_buf).map_err(AdbError::Io)?;

    if type_buf[0] != 0x00 {
        // 非 TYPE_CLIPBOARD 消息，读尽可能多的字节做调试输出
        let mut extra = [0u8; 64];
        let n = reader.read(&mut extra).unwrap_or(0);
        println!(
            "scrcpy msg: type=0x{:02X}, payload ({} bytes): {:02X?}",
            type_buf[0],
            n,
            &extra[..n]
        );
        return Ok(None);
    }

    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(AdbError::Io)?;
    let text_len = u32::from_be_bytes(len_buf) as usize;

    let mut text = vec![0u8; text_len];
    reader.read_exact(&mut text).map_err(AdbError::Io)?;

    // 格式化输出完整消息
    let mut raw_buf = Vec::with_capacity(1 + 4 + text_len);
    raw_buf.extend_from_slice(&type_buf);
    raw_buf.extend_from_slice(&len_buf);
    raw_buf.extend_from_slice(&text);
    println!("scrcpy clipboard msg ({text_len} bytes): {:02X?}", raw_buf);

    let clip_text = String::from_utf8(text).map_err(|e| AdbError::Other(format!("{}", e)))?;
    Ok(Some(clip_text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn build_clipboard_msg(text: &str) -> Vec<u8> {
        let mut msg = vec![0x00u8]; // TYPE_CLIPBOARD
        msg.extend_from_slice(&(text.len() as u32).to_be_bytes());
        msg.extend_from_slice(text.as_bytes());
        msg
    }

    #[test]
    fn test_parse_clipboard_event_normal() {
        let mut cursor = Cursor::new(build_clipboard_msg("Hello"));
        let result = parse_clipboard_event(&mut cursor).unwrap();
        assert_eq!(result, Some("Hello".to_string()));
    }

    #[test]
    fn test_parse_clipboard_event_chinese() {
        let text = "你好世界";
        let mut cursor = Cursor::new(build_clipboard_msg(text));
        let result = parse_clipboard_event(&mut cursor).unwrap();
        assert_eq!(result, Some(text.to_string()));
    }

    #[test]
    fn test_parse_clipboard_event_empty() {
        let mut cursor = Cursor::new(build_clipboard_msg(""));
        let result = parse_clipboard_event(&mut cursor).unwrap();
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn test_parse_clipboard_event_large_text() {
        let text = "a".repeat(10_000);
        let mut cursor = Cursor::new(build_clipboard_msg(&text));
        let result = parse_clipboard_event(&mut cursor).unwrap();
        assert_eq!(result, Some(text));
    }

    #[test]
    fn test_parse_clipboard_event_skip_other_types() {
        // TYPE_ACK_CLIPBOARD = 1
        let data = [0x01u8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut cursor = Cursor::new(data);
        let result = parse_clipboard_event(&mut cursor).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_clipboard_event_skip_unknown_types() {
        for &msg_type in &[0x02u8, 0x09u8, 0x0Au8] {
            let data = [msg_type];
            let mut cursor = Cursor::new(data);
            let result = parse_clipboard_event(&mut cursor)
                .unwrap_or_else(|_| panic!("msg type 0x{:02X} should not error", msg_type));
            assert_eq!(
                result, None,
                "msg type 0x{msg_type:02X} should return None (ignored)"
            );
        }
    }

    #[test]
    fn test_parse_clipboard_event_truncated_stream() {
        let data = [0x00u8, 0x00, 0x00, 0x00];
        let mut cursor = Cursor::new(data);
        let result = parse_clipboard_event(&mut cursor);
        assert!(matches!(result, Err(AdbError::Io(_))));
    }

    #[test]
    fn test_parse_clipboard_event_empty_reader() {
        let data = [];
        let mut cursor = Cursor::new(data);
        let result = parse_clipboard_event(&mut cursor);
        assert!(matches!(result, Err(AdbError::Io(_))));
    }
}
