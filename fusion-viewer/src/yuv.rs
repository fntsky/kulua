//! YUV420P → RGBX 渲染（BT.601 limited range，Android MediaCodec 默认输出）。
//!
//! 不依赖 libswscale：手写最近邻采样转换，直接按目标窗口尺寸采样，
//! 一步完成 YUV→RGB 与等比缩放，避免中间缓冲。

/// 将一帧 YUV420P 渲染为 RGBX u32 像素（`0x00RRGGBB`，alpha 恒为 0）。
///
/// - `dst` 长度必须 ≥ `dst_w * dst_h`
/// - `y` 长度 ≥ `src_w * src_h`；`u`/`v` 长度 ≥ `(src_w/2) * (src_h/2)`
/// - 使用最近邻采样：目标像素 (x, y) 取源像素 (x*src_w/dst_w, y*src_h/dst_h)
pub fn render_yuv420_to_rgbx(
    dst: &mut [u32],
    dst_w: usize,
    dst_h: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    src_w: usize,
    src_h: usize,
) {
    debug_assert!(dst.len() >= dst_w * dst_h);
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return;
    }
    let uv_w = src_w / 2;
    for row in 0..dst_h {
        // 最近邻源坐标（整数除法截断即 floor）
        let sy = row * src_h / dst_h;
        let sy_uv = sy / 2;
        let y_row = sy * src_w;
        let uv_row = sy_uv * uv_w;
        for col in 0..dst_w {
            let sx = col * src_w / dst_w;
            let sx_uv = sx / 2;
            let y_idx = y_row + sx;
            let uv_idx = uv_row + sx_uv;
            let (r, g, b) = yuv_to_rgb(y[y_idx], u[uv_idx], v[uv_idx]);
            dst[row * dst_w + col] = (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
        }
    }
}

/// BT.601 limited range YUV → RGB（对照 scrcpy/SDL 的转换系数）。
#[inline]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let c = i32::from(y) - 16;
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let r = (298 * c + 409 * e + 128) >> 8;
    let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
    let b = (298 * c + 516 * d + 128) >> 8;
    (clamp_u8(r), clamp_u8(g), clamp_u8(b))
}

#[inline]
fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造纯色 YUV420P 帧（所有像素相同）。
    fn solid_frame(w: usize, h: usize, y: u8, u: u8, v: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let y_plane = vec![y; w * h];
        let uv_plane = vec![u; (w / 2) * (h / 2)];
        (y_plane, uv_plane.clone(), vec![v; uv_plane.len()])
    }

    #[test]
    fn solid_white_frame() {
        // 白：Y=235, U=V=128（BT.601 limited）；公式映射后应为 255（0xFFFFFF）
        let (y, u, v) = solid_frame(4, 4, 235, 128, 128);
        let mut dst = vec![0u32; 16];
        render_yuv420_to_rgbx(&mut dst, 4, 4, &y, &u, &v, 4, 4);
        for px in dst {
            assert_eq!(px, 0x00FFFFFF, "白色帧所有像素应为 0xFFFFFF");
        }
    }

    #[test]
    fn solid_black_frame() {
        // 黑：Y=16, U=V=128
        let (y, u, v) = solid_frame(4, 4, 16, 128, 128);
        let mut dst = vec![0u32; 16];
        render_yuv420_to_rgbx(&mut dst, 4, 4, &y, &u, &v, 4, 4);
        for px in dst {
            assert_eq!(px, 0x00000000, "黑色帧所有像素应为 0");
        }
    }

    #[test]
    fn red_channel_positive_v() {
        // 红：U=90, V=240（BT.601 红近似），Y=82
        let (y, u, v) = solid_frame(4, 4, 82, 90, 240);
        let mut dst = vec![0u32; 16];
        render_yuv420_to_rgbx(&mut dst, 4, 4, &y, &u, &v, 4, 4);
        let px = dst[0];
        let r = (px >> 16) & 0xFF;
        let b = px & 0xFF;
        assert!(r > 200, "红色帧 R 通道应显著（got {:#x}）", px);
        assert!(b < 100, "红色帧 B 通道应低（got {:#x}）", px);
    }

    #[test]
    fn downscale_nearest_keeps_solid_color() {
        let (y, u, v) = solid_frame(8, 8, 100, 128, 128);
        let mut dst = vec![0u32; 4]; // 8x8 → 2x2
        render_yuv420_to_rgbx(&mut dst, 2, 2, &y, &u, &v, 8, 8);
        let expected = {
            let (r, g, b) = yuv_to_rgb(100, 128, 128);
            (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        };
        assert!(dst.iter().all(|&p| p == expected), "缩小后纯色不变");
    }

    #[test]
    fn zero_size_is_noop() {
        let (y, u, v) = solid_frame(4, 4, 16, 128, 128);
        let mut dst = vec![0u32; 4];
        render_yuv420_to_rgbx(&mut dst, 0, 0, &y, &u, &v, 4, 4);
        assert_eq!(dst, vec![0u32; 4]);
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(clamp_u8(-5), 0);
        assert_eq!(clamp_u8(300), 255);
        assert_eq!(clamp_u8(128), 128);
    }
}
