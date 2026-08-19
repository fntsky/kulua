//! YUV420P → RGBX 渲染（BT.601 limited range，Android MediaCodec 默认输出）。
//!
//! 不依赖 libswscale：手写最近邻采样转换，直接按目标窗口尺寸采样，
//! 一步完成 YUV→RGB 与等比缩放，避免中间缓冲。

/// YUV420P → RGBA8（供 egui 纹理；BT.601 limited range，alpha=255）。
///
/// 与旧 RGBX 版相同的最近邻采样，一次完成 YUV→RGB 与缩放，
/// 但输出为字节级 RGBA（egui::ColorImage 需要）。
pub fn render_yuv420_to_rgba(
    dst: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    src_w: usize,
    src_h: usize,
) {
    debug_assert!(dst.len() >= dst_w * dst_h * 4);
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return;
    }
    let uv_w = src_w / 2;
    for row in 0..dst_h {
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
            let o = (row * dst_w + col) * 4;
            dst[o] = r;
            dst[o + 1] = g;
            dst[o + 2] = b;
            dst[o + 3] = 255;
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

    /// RGBA 像素 (r,g,b,a)。
    fn rgba_px(dst: &[u8], i: usize) -> (u8, u8, u8, u8) {
        let o = i * 4;
        (dst[o], dst[o + 1], dst[o + 2], dst[o + 3])
    }

    #[test]
    fn solid_white_frame() {
        // 白：Y=235, U=V=128（BT.601 limited）→ 255, 255, 255
        let (y, u, v) = solid_frame(4, 4, 235, 128, 128);
        let mut dst = vec![0u8; 16 * 4];
        render_yuv420_to_rgba(&mut dst, 4, 4, &y, &u, &v, 4, 4);
        for i in 0..16 {
            assert_eq!(rgba_px(&dst, i), (255, 255, 255, 255), "白色帧@{i}");
        }
    }

    #[test]
    fn solid_black_frame() {
        let (y, u, v) = solid_frame(4, 4, 16, 128, 128);
        let mut dst = vec![0u8; 16 * 4];
        render_yuv420_to_rgba(&mut dst, 4, 4, &y, &u, &v, 4, 4);
        for i in 0..16 {
            assert_eq!(rgba_px(&dst, i), (0, 0, 0, 255), "黑色帧@{i}");
        }
    }

    #[test]
    fn red_channel_positive_v() {
        let (y, u, v) = solid_frame(4, 4, 82, 90, 240);
        let mut dst = vec![0u8; 16 * 4];
        render_yuv420_to_rgba(&mut dst, 4, 4, &y, &u, &v, 4, 4);
        let (r, _, b, _) = rgba_px(&dst, 0);
        assert!(r > 200, "红帧 R 应显著 got {r}");
        assert!(b < 100, "红帧 B 应低 got {b}");
    }

    #[test]
    fn downscale_nearest_keeps_solid_color() {
        let (y, u, v) = solid_frame(8, 8, 100, 128, 128);
        let mut dst = vec![0u8; 4 * 4]; // 8x8 → 2x2
        render_yuv420_to_rgba(&mut dst, 2, 2, &y, &u, &v, 8, 8);
        let (r, g, b, _) = rgba_px(&dst, 0);
        let expected = yuv_to_rgb(100, 128, 128);
        assert!(
            dst.chunks(4).all(|p| (p[0], p[1], p[2]) == expected),
            "缩小后纯色不变"
        );
        let _ = (r, g, b);
    }

    #[test]
    fn zero_size_is_noop() {
        let (y, u, v) = solid_frame(4, 4, 16, 128, 128);
        let mut dst = vec![0u8; 4 * 4];
        render_yuv420_to_rgba(&mut dst, 0, 0, &y, &u, &v, 4, 4);
        assert_eq!(dst, vec![0u8; 16]);
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(clamp_u8(-5), 0);
        assert_eq!(clamp_u8(300), 255);
        assert_eq!(clamp_u8(128), 128);
    }
}
