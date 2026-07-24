use tokio::io::AsyncReadExt;

// ── 协议常量 ──

const DEVICE_MSG_TYPE_CLIPBOARD: u8 = 0x00;
const DEVICE_MSG_TYPE_ACK_CLIPBOARD: u8 = 0x01;
const DEVICE_MSG_TYPE_UHID_OUTPUT: u8 = 0x02;

/// 从 control socket 读取一条设备消息。
///
/// 接受 `impl AsyncReadExt + Unpin`（tokio::io::ReadHalf 满足此约束）。
/// 返回:
/// - `Ok(Some(text))` — type 0x00 (CLIPBOARD)
/// - `Ok(None)` — 其他已知类型（内部已处理/静默忽略）
/// - `Err(())` — 连接断开或协议错误（未知 type 视为不可恢复）
pub(super) async fn read_device_message(
    reader: &mut (impl AsyncReadExt + Unpin),
) -> Result<Option<String>, ()> {
    let mut type_buf = [0u8; 1];
    reader.read_exact(&mut type_buf).await.map_err(|_| ())?;

    match type_buf[0] {
        DEVICE_MSG_TYPE_CLIPBOARD => {
            let mut len_buf = [0u8; 4];
            reader.read_exact(&mut len_buf).await.map_err(|_| ())?;
            let text_len = u32::from_be_bytes(len_buf) as usize;
            if text_len > 256 * 1024 {
                return Err(());
            }
            let mut text = vec![0u8; text_len];
            reader.read_exact(&mut text).await.map_err(|_| ())?;
            let clip_text = String::from_utf8(text).map_err(|_| ())?;
            Ok(Some(clip_text))
        }
        DEVICE_MSG_TYPE_ACK_CLIPBOARD => {
            // 8-byte sequence number, 转发逻辑中无需处理
            let mut seq_buf = [0u8; 8];
            reader.read_exact(&mut seq_buf).await.map_err(|_| ())?;
            Ok(None)
        }
        DEVICE_MSG_TYPE_UHID_OUTPUT => {
            // 2B id + 2B length + data
            let mut meta = [0u8; 4];
            reader.read_exact(&mut meta).await.map_err(|_| ())?;
            let _id = u16::from_be_bytes([meta[0], meta[1]]);
            let data_len = u16::from_be_bytes([meta[2], meta[3]]) as usize;
            if data_len > 4096 {
                return Err(());
            }
            let mut _data = vec![0u8; data_len];
            reader.read_exact(&mut _data).await.map_err(|_| ())?;
            // UHID 输出，当前不处理
            Ok(None)
        }
        _ => {
            // 未知 type → 协议不可恢复
            Err(())
        }
    }
}

/// 音频 codec ID → 可读名称
pub(super) fn audio_codec_name(id: u32) -> &'static str {
    match id {
        0x6f707573 => "OPUS",
        0x00616163 => "AAC",
        0x666c6163 => "FLAC",
        0x00726177 => "RAW",
        _ => "UNKNOWN",
    }
}

/// 构建剪贴板设置帧（PC→Phone 方向）。
pub(super) fn build_clipboard_frame(text: &str) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let mut buf = Vec::with_capacity(14 + text_bytes.len());
    buf.push(0x09); // TYPE_SET_CLIPBOARD
    buf.extend_from_slice(&[0u8; 8]); // 序列号（固定 0）
    buf.push(0); // paste 标记（不自动粘贴）
    buf.extend_from_slice(&(text_bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(text_bytes);
    buf
}
