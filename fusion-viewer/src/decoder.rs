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

/// H264/H265 解码器封装（优先 D3D11VA 硬解，失败自动回退软解）。
pub struct VideoDecoder {
    codec_ctx: *mut AVCodecContext,
    packet: *mut AVPacket,
    frame: *mut AVFrame,
    /// 硬解帧转回系统内存用的目标帧（软解时闲置）
    sw_frame: *mut AVFrame,
    /// 硬件设备上下文（由 codec_ctx 持有引用，Drop 随 avcodec_free_context 释放）
    hw_device_ctx: *mut AVBufferRef,
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
            let sw_frame = av_frame_alloc();
            if packet.is_null() || frame.is_null() || sw_frame.is_null() {
                avcodec_free_context(&mut codec_ctx.clone());
                return Err("av_packet/av_frame 分配失败".into());
            }
            Ok(Self {
                codec_ctx,
                packet,
                frame,
                sw_frame,
                hw_device_ctx: std::ptr::null_mut(),
                opened: false,
            })
        }
    }

    /// 设置解码器 extradata（**必须为 Annex-B 格式**：start code + SPS/PPS）。
    /// 必须在 `open` 之前调用。
    ///
    /// 注意：不能直接使用 MediaCodec 的 csd——它通常是 avcC 格式
    /// （AVCDecoderConfigurationRecord，长度前缀式），会迫使解码器进入
    /// length-prefixed 模式，与 Annex-B 媒体帧冲突。先用 [`avcc_to_annexb`] 转换。
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
    ///
    /// 优先尝试 D3D11VA 硬件解码（把解码从 CPU 卸载到 GPU，缓解与音频解码的
    /// CPU 竞争）；GPU/驱动不支持时自动回退软解。
    pub fn open(&mut self) -> Result<(), String> {
        if self.opened {
            return Ok(());
        }
        unsafe {
            // 创建 D3D11VA 硬件设备上下文（d3d11va 解码器需要 codec_ctx.hw_device_ctx）
            let mut hw_ctx: *mut AVBufferRef = std::ptr::null_mut();
            let hw_ret = av_hwdevice_ctx_create(
                &mut hw_ctx,
                AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if hw_ret >= 0 {
                (*self.codec_ctx).hw_device_ctx = hw_ctx;
                self.hw_device_ctx = hw_ctx;
                println!("[viewer] D3D11VA 硬解已启用");
            } else {
                // 无 GPU/驱动 → 回退软解（解码仍可用，仅 CPU 占用更高）
                eprintln!("[viewer] D3D11VA 不可用（回退软解）: {}", err_str(hw_ret));
            }

            let ret = avcodec_open2(self.codec_ctx, std::ptr::null(), std::ptr::null_mut());
            if ret < 0 {
                return Err(format!("avcodec_open2 失败: {}", err_str(ret)));
            }
        }
        self.opened = true;
        Ok(())
    }

    /// 解码一帧（`keyframe` 标记关键帧）。
    ///
    /// 解码错误（坏帧/瞬时异常）不返回 Err：flush 后继续，避免 resize 重置
    /// 后的首个坏帧把整个视频线程杀掉（表现为"视频流已结束"）。
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
                // 坏帧：flush 解码器内部状态后继续（AVERROR_INVALIDDATA 等）
                avcodec_flush_buffers(self.codec_ctx);
                return Ok(None);
            }

            let recv_ret = avcodec_receive_frame(self.codec_ctx, self.frame);
            if recv_ret == -EAGAIN {
                // 帧缓冲不足，等待后续输入
                return Ok(None);
            }
            if recv_ret < 0 {
                // 解码错误（如 SPS/PPS 缺失的坏帧）：flush 后继续
                avcodec_flush_buffers(self.codec_ctx);
                return Ok(None);
            }

            // 硬件帧（D3D11 纹理）：先转回系统内存（NV12）再拷贝平面；
            // 软解帧直接拷贝
            let src = if !(*self.frame).hw_frames_ctx.is_null() {
                av_frame_unref(self.sw_frame);
                let transfer_ret = av_hwframe_transfer_data(self.sw_frame, self.frame, 0);
                if transfer_ret < 0 {
                    av_frame_unref(self.frame);
                    return Err(format!(
                        "av_hwframe_transfer_data 失败: {}",
                        err_str(transfer_ret)
                    ));
                }
                self.sw_frame
            } else {
                self.frame
            };
            let frame = copy_frame(src)?;
            av_frame_unref(self.frame);
            Ok(Some(frame))
        }
    }

    /// 解码器是否已打开。
    pub fn is_open(&self) -> bool {
        self.opened
    }

    /// 已打开时用新 extradata 重新初始化解码器（resize 后调用）。
    ///
    /// 为什么需要：弹性显示器 resize 后 server 重启编码器，新 codec 发出
    /// 新分辨率 SPS/PPS（config 帧）。vivo 等设备的 IDR 帧不带 SPS/PPS，
    /// 若忽略新 config 帧，解码器继续用旧分辨率参数解新尺寸的流 → 大量
    /// "concealing DC/AC/MV errors"。做法：释放旧 context（含 hw ctx）→
    /// 按原 codec 重新分配 → 设置新 extradata → 重新 open。
    pub fn reset_with_extradata(&mut self, data: &[u8]) -> Result<(), String> {
        unsafe {
            let codec_id = (*self.codec_ctx).codec_id;
            avcodec_free_context(&mut self.codec_ctx.clone());
            self.codec_ctx = std::ptr::null_mut();
            self.hw_device_ctx = std::ptr::null_mut(); // 由旧 ctx 释放，重新创建

            let codec = avcodec_find_decoder(codec_id);
            if codec.is_null() {
                return Err("reset: 找不到 codec".into());
            }
            let codec_ctx = avcodec_alloc_context3(codec);
            if codec_ctx.is_null() {
                return Err("reset: avcodec_alloc_context3 失败".into());
            }
            self.codec_ctx = codec_ctx;
            self.opened = false;
        }
        self.set_extradata(data)?;
        self.open()
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.sw_frame.is_null() {
                av_frame_free(&mut self.sw_frame.clone());
            }
            if !self.frame.is_null() {
                av_frame_free(&mut self.frame.clone());
            }
            if !self.packet.is_null() {
                av_packet_free(&mut self.packet.clone());
            }
            if !self.codec_ctx.is_null() {
                // hw_device_ctx 由 codec_ctx 持有引用，随 avcodec_free_context 释放
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

/// 判断 config 帧 payload 是否已是 Annex-B（start code）格式。
pub fn is_annexb(data: &[u8]) -> bool {
    data.len() >= 4 && data[..4] == [0x00, 0x00, 0x00, 0x01]
}

/// 将 MediaCodec 的 H264/H265 csd（avcC / hvcC 长度前缀格式）转换为 Annex-B。
///
/// avcC（AVCDecoderConfigurationRecord）布局：
/// - byte 0: configurationVersion (1)
/// - byte 1-3: profile/compat/level（H265 不同，但长度前缀部分相同）
/// - byte 4: 低 2 位 lengthSizeMinusOne（我们不需要）
/// - byte 5: 低 5 位 numOfSPS
/// - numOfSPS × (u16 长度 + NAL 数据)
/// - byte: numOfPPS
/// - numOfPPS × (u16 长度 + NAL 数据)
///
/// 转换结果：`00 00 00 01 <SPS> 00 00 00 01 <PPS> ...`
pub fn avcc_to_annexb(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 7 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut pos = 5;
    let num_sps = (data[5] & 0x1F) as usize;
    pos += 1;
    for _ in 0..num_sps {
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() {
            return None;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    if pos >= data.len() {
        return None;
    }
    let num_pps = data[pos] as usize;
    pos += 1;
    for _ in 0..num_pps {
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() {
            return None;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    Some(out)
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

    #[test]
    fn avcc_to_annexb_converts_sps_and_pps() {
        // 构造最小 avcC：1 个 SPS（4 字节）+ 1 个 PPS（2 字节）
        // 67 42 00 1E = SPS；68 CE 3C 80 = PPS
        let avcc = [
            0x01, 0x42, 0x00, 0x1E, // configurationVersion + profile/compat/level
            0xFF, // lengthSizeMinusOne=3（低 2 位）
            0xE1, // numOfSPS=1（低 5 位）
            0x00, 0x04, 0x67, 0x42, 0x00, 0x1E, // SPS: u16 长度 + NAL
            0x01, // numOfPPS=1
            0x00, 0x02, 0x68, 0xCE, // PPS: u16 长度 + NAL
        ];
        let annexb = avcc_to_annexb(&avcc).expect("应能转换");
        assert_eq!(
            annexb,
            vec![
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E, // start code + SPS
                0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, // start code + PPS
            ]
        );
    }

    #[test]
    fn avcc_to_annexb_rejects_truncated_data() {
        assert!(avcc_to_annexb(&[0x01, 0x42]).is_none(), "过短应失败");
        // SPS 长度越界
        let bad = [0x01, 0x42, 0x00, 0x1E, 0xFF, 0xE1, 0x00, 0x10, 0x67];
        assert!(avcc_to_annexb(&bad).is_none());
    }

    #[test]
    fn is_annexb_detects_start_code() {
        assert!(is_annexb(&[0x00, 0x00, 0x00, 0x01, 0x67]));
        assert!(!is_annexb(&[0x01, 0x42, 0x00, 0x1E, 0xFF]));
        assert!(!is_annexb(&[0x00, 0x00, 0x00]));
    }
}
