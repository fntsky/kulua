//! FFmpeg 视频解码封装（H264/H265 → YUV420P/NV12）。
//!
//! 直接使用 `ffmpeg-sys-next` 原始 FFI（仅 avcodec + avutil），
//! 运行时依赖 `avcodec-62.dll` / `avutil-60.dll`（FFmpeg 7.1）。

use ffmpeg_sys_next::*;

/// 解码得到的原始帧（YUV 平面已按行宽拷贝，无 padding）。
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedFrame {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// H264 解码器封装。
pub struct VideoDecoder {
    codec_ctx: *mut AVCodecContext,
    packet: *mut AVPacket,
    frame: *mut AVFrame,
    opened: bool,
}

// FFmpeg 对象只在线程内使用，跨线程移动是安全的
unsafe impl Send for VideoDecoder {}

impl VideoDecoder {
    /// 创建解码器。
    ///
    /// `codec_id` 为 scrcpy v4.0 视频头的 codec id：**4 字节 ASCII 名称**
    /// （`VideoCodec.java`：`0x68_32_36_34` = "h264"，`0x68_32_36_35` = "h265"，
    /// `0x00_61_76_31` = "av1"）。v4.0 已从旧版的整数 id（H264=27）改为 ASCII，
    /// 按整数匹配会得到 "不支持的 codec id"。
    pub fn new(codec_id: u32) -> Result<Self, String> {
        let codec_id_enum = match codec_id {
            0x68_32_36_34 => AVCodecID::AV_CODEC_ID_H264, // "h264"
            0x68_32_36_35 => AVCodecID::AV_CODEC_ID_HEVC, // "h265"
            0x00_61_76_31 => AVCodecID::AV_CODEC_ID_AV1,  // "av1"
            other => return Err(format!("不支持的 codec id: {:#x}", other)),
        };
        unsafe {
            let codec = avcodec_find_decoder(codec_id_enum);
            if codec.is_null() {
                return Err(format!("找不到 codec: {:#x}", codec_id));
            }
            let codec_ctx = avcodec_alloc_context3(codec);
            if codec_ctx.is_null() {
                return Err("avcodec_alloc_context3 失败".into());
            }
            let packet = av_packet_alloc();
            let frame = av_frame_alloc();
            if packet.is_null() || frame.is_null() {
                avcodec_free_context(&mut codec_ctx.clone());
                return Err("av_packet/av_frame 分配失败".into());
            }
            Ok(Self {
                codec_ctx,
                packet,
                frame,
                opened: false,
            })
        }
    }

    /// 设置解码器 extradata（scrcpy 视频流的 config 帧，含 SPS/PPS）。
    /// 必须在 `open` 之前调用。
    pub fn set_extradata(&mut self, data: &[u8]) -> Result<(), String> {
        if self.opened {
            return Err("解码器已打开，不能设置 extradata".into());
        }
        unsafe {
            if !(*self.codec_ctx).extradata.is_null() {
                av_free((*self.codec_ctx).extradata as *mut _);
            }
            // 需要 AV_INPUT_BUFFER_PADDING_SIZE 填充（解码器可能越界读）
            let padding = AV_INPUT_BUFFER_PADDING_SIZE as usize;
            let buf = av_malloc(data.len() + padding) as *mut u8;
            if buf.is_null() {
                return Err("av_malloc 失败".into());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
            std::ptr::write_bytes(buf.add(data.len()), 0, padding);
            (*self.codec_ctx).extradata = buf;
            (*self.codec_ctx).extradata_size = data.len() as i32;
        }
        Ok(())
    }

    /// 打开解码器。
    pub fn open(&mut self) -> Result<(), String> {
        if self.opened {
            return Ok(());
        }
        unsafe {
            let ret = avcodec_open2(self.codec_ctx, std::ptr::null(), std::ptr::null_mut());
            if ret < 0 {
                return Err(format!("avcodec_open2 失败: {}", err_str(ret)));
            }
        }
        self.opened = true;
        Ok(())
    }

    /// 解码一帧（`keyframe` 标记关键帧）。
    pub fn decode(
        &mut self,
        data: &[u8],
        pts: Option<i64>,
        keyframe: bool,
    ) -> Result<Option<DecodedFrame>, String> {
        unsafe {
            (*self.packet).data = data.as_ptr() as *mut u8;
            (*self.packet).size = data.len() as i32;
            (*self.packet).pts = pts.unwrap_or(i64::MIN);
            (*self.packet).flags = if keyframe { AV_PKT_FLAG_KEY as i32 } else { 0 };

            let send_ret = avcodec_send_packet(self.codec_ctx, self.packet);
            av_packet_unref(self.packet);
            if send_ret < 0 {
                return Err(format!("avcodec_send_packet 失败: {}", err_str(send_ret)));
            }

            let recv_ret = avcodec_receive_frame(self.codec_ctx, self.frame);
            if recv_ret == -EAGAIN {
                // 帧缓冲不足，等待后续输入
                return Ok(None);
            }
            if recv_ret < 0 {
                return Err(format!("avcodec_receive_frame 失败: {}", err_str(recv_ret)));
            }

            let frame = copy_frame(self.frame)?;
            av_frame_unref(self.frame);
            Ok(Some(frame))
        }
    }

    /// 解码器是否已打开。
    pub fn is_open(&self) -> bool {
        self.opened
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.frame.is_null() {
                av_frame_free(&mut self.frame.clone());
            }
            if !self.packet.is_null() {
                av_packet_free(&mut self.packet.clone());
            }
            if !self.codec_ctx.is_null() {
                avcodec_free_context(&mut self.codec_ctx.clone());
            }
        }
    }
}

/// 从 AVFrame 拷贝出无 padding 的 YUV 平面。
unsafe fn copy_frame(frame: *mut AVFrame) -> Result<DecodedFrame, String> {
    unsafe {
        let width = (*frame).width.max(0) as usize;
        let height = (*frame).height.max(0) as usize;
        if width == 0 || height == 0 {
            return Err("空帧".into());
        }
        let format = (*frame).format;
        // bindgen 将 enum 字段生成为 i32，与枚举值比较
        if format == AVPixelFormat::AV_PIX_FMT_YUV420P as i32 {
            let linesize = (*frame).linesize[0].max(0) as usize;
            let uv_linesize = (*frame).linesize[1].max(0) as usize;
            let uv_h = height / 2;
            let uv_w = width / 2;
            Ok(DecodedFrame {
                y: copy_plane((*frame).data[0], linesize, width, height),
                u: copy_plane((*frame).data[1], uv_linesize, uv_w, uv_h),
                v: copy_plane((*frame).data[2], uv_linesize, uv_w, uv_h),
                width,
                height,
            })
        } else if format == AVPixelFormat::AV_PIX_FMT_NV12 as i32 {
            // NV12：Y 平面 + UV 交错平面（U 在偶字节，V 在奇字节）
            let linesize = (*frame).linesize[0].max(0) as usize;
            let uv_linesize = (*frame).linesize[1].max(0) as usize;
            let uv_h = height / 2;
            let uv_w = width / 2;
            let uv = copy_plane((*frame).data[1], uv_linesize, uv_w * 2, uv_h);
            let u = uv.iter().step_by(2).copied().collect();
            let v = uv.iter().skip(1).step_by(2).copied().collect();
            Ok(DecodedFrame {
                y: copy_plane((*frame).data[0], linesize, width, height),
                u,
                v,
                width,
                height,
            })
        } else {
            Err(format!("不支持的像素格式: {}", format))
        }
    }
}

/// 按行宽步进拷贝平面（丢弃行尾 padding）。
unsafe fn copy_plane(src: *mut u8, linesize: usize, width: usize, height: usize) -> Vec<u8> {
    unsafe {
        let mut out = Vec::with_capacity(width * height);
        for row in 0..height {
            let row_src = src.add(row * linesize);
            out.extend_from_slice(std::slice::from_raw_parts(row_src, width));
        }
        out
    }
}

/// 把 FFmpeg 负数错误码转成可读文本。
fn err_str(ret: i32) -> String {
    let mut buf = [0u8; 64];
    unsafe {
        let n = av_strerror(ret, buf.as_mut_ptr() as *mut i8, buf.len());
        if n >= 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            format!("error {}", ret)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_codec_id_is_ascii_name() {
        // scrcpy v4.0 视频头 codec id 是 4 字节 ASCII（VideoCodec.java 核实）
        assert_eq!(0x68_32_36_34u32, u32::from_be_bytes(*b"h264"));
        assert_eq!(0x68_32_36_35u32, u32::from_be_bytes(*b"h265"));
        assert_eq!(
            0x00_61_76_31u32,
            u32::from_be_bytes([0x00, 0x61, 0x76, 0x31])
        ); // "av1"（前导 0）
        // 对应的 FFmpeg 内部 codec id
        assert_eq!(AVCodecID::AV_CODEC_ID_H264 as i32, 27);
        assert_eq!(AVCodecID::AV_CODEC_ID_HEVC as i32, 173);
        assert_eq!(AVCodecID::AV_CODEC_ID_AV1 as i32, 225);
    }

    #[test]
    fn unknown_codec_returns_error() {
        let decoder = VideoDecoder::new(999999);
        assert!(decoder.is_err(), "未知 codec id 应报错");
    }

    #[test]
    fn ascii_codec_ids_are_accepted() {
        assert!(
            VideoDecoder::new(0x68_32_36_34).is_ok(),
            "\"h264\" 应被接受"
        );
        assert!(
            VideoDecoder::new(0x68_32_36_35).is_ok(),
            "\"h265\" 应被接受"
        );
        assert!(VideoDecoder::new(0x00_61_76_31).is_ok(), "\"av1\" 应被接受");
        // 旧版整数 id 不再被接受（v4.0 协议）
        assert!(VideoDecoder::new(27).is_err());
    }

    #[test]
    fn decoder_opens_without_extradata() {
        // 无 extradata 也能 open（首帧可能解码失败，但 open 本身应成功）
        let mut decoder = VideoDecoder::new(0x68_32_36_34).unwrap();
        assert!(decoder.open().is_ok());
        assert!(decoder.is_open());
    }

    #[test]
    fn decode_garbage_returns_err_not_panic() {
        let mut decoder = VideoDecoder::new(0x68_32_36_34).unwrap();
        decoder.open().unwrap();
        let result = decoder.decode(&[0x00, 0x01, 0x02, 0x03], Some(0), false);
        // 垃圾数据不应 panic；可能是 Err 或 Ok(None)
        let _ = result;
    }
}
