// 会话协议小工具（UDP 直连版）。
//
// 线协议本体由 kulua-proto（proto/direct.proto）提供；这里只保留 codec id
// 的可读名称（音频日志用）。

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
