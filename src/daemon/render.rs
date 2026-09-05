//! Fit-mode geometry and RGBA→BGRA composition. Pure; no Wayland, no config
//! I/O. The image worker calls [`compose`] to turn a decoded image plus a
//! [`crate::config::Mode`] into the exact bytes of an `Xrgb8888` shm buffer.

use image::{Rgba, RgbaImage};

use crate::color::Color;
use crate::config::Mode;

/// A size in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

impl Size {
    #[must_use]
    pub fn new(w: u32, h: u32) -> Self {
        Self { w, h }
    }

    fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// How the source maps onto the destination for one fit mode.
///
/// Invariants (all modes): `src_crop` lies fully within the source, and the
/// scaled image placed at `dst_origin` lies fully within the destination
/// (`fill` / `stretch` cover it exactly, `fit` / `center` inset it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Sub-rectangle of the source image to use.
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    /// Size the cropped source is scaled to (equals the crop size for
    /// `center`, i.e. no scaling).
    pub scaled: Size,
    /// Where the scaled image's top-left sits in the destination buffer.
    pub dst_x: u32,
    pub dst_y: u32,
}

/// Compute the [`Layout`] for placing `src` into `dst` under `mode`.
///
/// `dst` with a zero dimension, or `src` with a zero dimension, yields a
/// degenerate layout with an empty `scaled` — [`compose`] then produces a
/// fill-only buffer.
#[must_use]
pub fn layout(src: Size, dst: Size, mode: Mode) -> Layout {
    if src.is_empty() || dst.is_empty() {
        return Layout {
            src_x: 0,
            src_y: 0,
            src_w: src.w,
            src_h: src.h,
            scaled: Size::new(0, 0),
            dst_x: 0,
            dst_y: 0,
        };
    }

    match mode {
        Mode::Stretch => Layout {
            src_x: 0,
            src_y: 0,
            src_w: src.w,
            src_h: src.h,
            scaled: dst,
            dst_x: 0,
            dst_y: 0,
        },
        Mode::Fit => {
            let scaled = contain(src, dst);
            let (dst_x, dst_y) = center_offset(scaled, dst);
            Layout {
                src_x: 0,
                src_y: 0,
                src_w: src.w,
                src_h: src.h,
                scaled,
                dst_x,
                dst_y,
            }
        }
        Mode::Fill => {
            // Crop the source to the destination's aspect ratio, then scale
            // that crop up to fill the destination exactly.
            let (crop_w, crop_h) = crop_to_aspect(src, dst);
            let src_x = (src.w - crop_w) / 2;
            let src_y = (src.h - crop_h) / 2;
            Layout {
                src_x,
                src_y,
                src_w: crop_w,
                src_h: crop_h,
                scaled: dst,
                dst_x: 0,
                dst_y: 0,
            }
        }
        Mode::Center => {
            // 1:1 pixels. Crop whichever axis overflows the destination;
            // centre the rest.
            let crop_w = src.w.min(dst.w);
            let crop_h = src.h.min(dst.h);
            let src_x = (src.w - crop_w) / 2;
            let src_y = (src.h - crop_h) / 2;
            let dst_x = (dst.w - crop_w) / 2;
            let dst_y = (dst.h - crop_h) / 2;
            Layout {
                src_x,
                src_y,
                src_w: crop_w,
                src_h: crop_h,
                scaled: Size::new(crop_w, crop_h),
                dst_x,
                dst_y,
            }
        }
    }
}

/// Largest size with `src`'s aspect ratio that fits inside `dst`.
fn contain(src: Size, dst: Size) -> Size {
    let w = u64::from(dst.h) * u64::from(src.w) / u64::from(src.h);
    if w <= u64::from(dst.w) {
        Size::new(w.max(1) as u32, dst.h)
    } else {
        let h = u64::from(dst.w) * u64::from(src.h) / u64::from(src.w);
        Size::new(dst.w, h.max(1) as u32)
    }
}

/// Sub-size of `src` (centred) whose aspect ratio matches `dst`.
fn crop_to_aspect(src: Size, dst: Size) -> (u32, u32) {
    // Compare src.w/src.h against dst.w/dst.h without floats.
    if u64::from(src.w) * u64::from(dst.h) > u64::from(dst.w) * u64::from(src.h) {
        // Source is wider than the target aspect → crop width.
        let w = u64::from(src.h) * u64::from(dst.w) / u64::from(dst.h);
        (w.clamp(1, u64::from(src.w)) as u32, src.h)
    } else {
        // Source is taller (or equal) → crop height.
        let h = u64::from(src.w) * u64::from(dst.h) / u64::from(dst.w);
        (src.w, h.clamp(1, u64::from(src.h)) as u32)
    }
}

fn center_offset(inner: Size, outer: Size) -> (u32, u32) {
    (
        outer.w.saturating_sub(inner.w) / 2,
        outer.h.saturating_sub(inner.h) / 2,
    )
}

/// A buffer of `size.w * size.h * 4` bytes filled with `fill` in `Xrgb8888`
/// (BGRA, little-endian, premultiplied) order — the colour-only wallpaper,
/// and the background [`compose`] paints images over.
#[must_use]
pub fn solid(fill: Color, size: Size) -> Vec<u8> {
    let len = (size.w as usize) * (size.h as usize) * 4;
    if len == 0 {
        return Vec::new();
    }
    let bg = fill.to_shm_bgra();
    if bg.iter().all(|&b| b == bg[0]) {
        return vec![bg[0]; len];
    }
    // Seed 4 bytes, then repeatedly copy the buffer onto itself: O(log n)
    // memcpys instead of n tiny appends.
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&bg);
    while out.len() < len {
        let take = (len - out.len()).min(out.len());
        out.extend_from_within(..take);
    }
    out
}

/// Compose `src` over a `fill` background into an `out_size` buffer, returning
/// `out_size.w * out_size.h * 4` bytes in `Xrgb8888` (BGRA, little-endian)
/// order, premultiplied — ready to `memcpy` into an shm slot of matching
/// stride (`out_size.w * 4`).
#[must_use]
pub fn compose(src: &RgbaImage, mode: Mode, fill: Color, out_size: Size) -> Vec<u8> {
    let mut out = solid(fill, out_size);
    if out_size.is_empty() {
        return out;
    }

    let lay = layout(Size::new(src.width(), src.height()), out_size, mode);
    if lay.scaled.is_empty() {
        return out;
    }

    let cropped = image::imageops::crop_imm(src, lay.src_x, lay.src_y, lay.src_w, lay.src_h);
    let scaled: RgbaImage = if lay.scaled == Size::new(lay.src_w, lay.src_h) {
        cropped.to_image()
    } else {
        image::imageops::resize(
            &*cropped,
            lay.scaled.w,
            lay.scaled.h,
            image::imageops::FilterType::Lanczos3,
        )
    };

    blit_over(&mut out, out_size, &scaled, lay.dst_x, lay.dst_y, fill);
    out
}

/// Alpha-composite `fg` (RGBA) over the already-`fill`-painted `out` (BGRA),
/// at `(ox, oy)`. `fg` is guaranteed by [`layout`] to fit within `out`.
fn blit_over(out: &mut [u8], out_size: Size, fg: &RgbaImage, ox: u32, oy: u32, fill: Color) {
    let stride = out_size.w as usize * 4;
    for (fx, fy, &Rgba([r, g, b, a])) in fg.enumerate_pixels() {
        let x = ox as usize + fx as usize;
        let y = oy as usize + fy as usize;
        let i = y * stride + x * 4;
        if a == 255 {
            out[i] = b;
            out[i + 1] = g;
            out[i + 2] = r;
            out[i + 3] = 255;
        } else if a == 0 {
            // leave the fill pixel
        } else {
            let af = u16::from(a);
            let comp = |c: u8, under: u8| {
                ((u16::from(c) * af + u16::from(under) * (255 - af)) / 255) as u8
            };
            out[i] = comp(b, fill.b);
            out[i + 1] = comp(g, fill.g);
            out[i + 2] = comp(r, fill.r);
            out[i + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Color = Color {
        r: 255,
        g: 0,
        b: 0,
        a: 255,
    };

    fn solid(w: u32, h: u32, px: Rgba<u8>) -> RgbaImage {
        RgbaImage::from_pixel(w, h, px)
    }

    fn bgra_at(buf: &[u8], size: Size, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * size.w as usize + x as usize) * 4;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn stretch_covers_everything() {
        let l = layout(Size::new(4, 4), Size::new(10, 6), Mode::Stretch);
        assert_eq!(l.scaled, Size::new(10, 6));
        assert_eq!((l.dst_x, l.dst_y), (0, 0));
        assert_eq!((l.src_w, l.src_h), (4, 4));
    }

    #[test]
    fn fit_letterboxes_wide_dst() {
        // 1:1 source into 10x6 → scaled 6x6, centred with 2px side bars.
        let l = layout(Size::new(100, 100), Size::new(10, 6), Mode::Fit);
        assert_eq!(l.scaled, Size::new(6, 6));
        assert_eq!((l.dst_x, l.dst_y), (2, 0));
    }

    #[test]
    fn fit_pillarboxes_tall_dst() {
        let l = layout(Size::new(100, 100), Size::new(6, 10), Mode::Fit);
        assert_eq!(l.scaled, Size::new(6, 6));
        assert_eq!((l.dst_x, l.dst_y), (0, 2));
    }

    #[test]
    fn fill_crops_source_to_dst_aspect() {
        // 20x10 source into 10x10 dst → crop to 10x10 (centred x), scale to dst.
        let l = layout(Size::new(20, 10), Size::new(10, 10), Mode::Fill);
        assert_eq!((l.src_w, l.src_h), (10, 10));
        assert_eq!(l.src_x, 5);
        assert_eq!(l.scaled, Size::new(10, 10));
        assert_eq!((l.dst_x, l.dst_y), (0, 0));
    }

    #[test]
    fn center_small_image_is_bordered_not_scaled() {
        let l = layout(Size::new(4, 2), Size::new(10, 6), Mode::Center);
        assert_eq!(l.scaled, Size::new(4, 2)); // no scaling
        assert_eq!((l.src_w, l.src_h), (4, 2));
        assert_eq!((l.dst_x, l.dst_y), (3, 2));
    }

    #[test]
    fn center_large_image_is_cropped() {
        let l = layout(Size::new(20, 20), Size::new(10, 6), Mode::Center);
        assert_eq!(l.scaled, Size::new(10, 6));
        assert_eq!((l.src_x, l.src_y), (5, 7));
        assert_eq!((l.dst_x, l.dst_y), (0, 0));
    }

    #[test]
    fn layout_invariants_hold_across_sizes() {
        let sizes = [
            Size::new(1, 1),
            Size::new(3, 7),
            Size::new(16, 9),
            Size::new(1920, 1080),
            Size::new(200, 200),
        ];
        for &src in &sizes {
            for &dst in &sizes {
                for mode in [Mode::Fill, Mode::Fit, Mode::Stretch, Mode::Center] {
                    let l = layout(src, dst, mode);
                    assert!(
                        l.src_x + l.src_w <= src.w,
                        "src x overflow {src:?}->{dst:?} {mode}"
                    );
                    assert!(
                        l.src_y + l.src_h <= src.h,
                        "src y overflow {src:?}->{dst:?} {mode}"
                    );
                    assert!(
                        l.dst_x + l.scaled.w <= dst.w,
                        "dst x overflow {src:?}->{dst:?} {mode}"
                    );
                    assert!(
                        l.dst_y + l.scaled.h <= dst.h,
                        "dst y overflow {src:?}->{dst:?} {mode}"
                    );
                    assert!(l.src_w >= 1 && l.src_h >= 1);
                }
            }
        }
    }

    #[test]
    fn compose_stretch_fills_output_with_image() {
        let src = solid(2, 2, Rgba([0, 128, 255, 255]));
        let out = compose(&src, Mode::Stretch, RED, Size::new(4, 3));
        assert_eq!(out.len(), 4 * 3 * 4);
        // every pixel is the image colour in BGRA
        for y in 0..3 {
            for x in 0..4 {
                assert_eq!(bgra_at(&out, Size::new(4, 3), x, y), [255, 128, 0, 255]);
            }
        }
    }

    #[test]
    fn compose_fit_paints_bars_with_fill_colour() {
        let src = solid(10, 10, Rgba([0, 0, 0, 255]));
        let size = Size::new(10, 6);
        let out = compose(&src, Mode::Fit, RED, size);
        // corner is a bar → fill (red = BGRA 0,0,255,255)
        assert_eq!(bgra_at(&out, size, 0, 0), [0, 0, 255, 255]);
        // centre is the image → black
        assert_eq!(bgra_at(&out, size, 5, 3), [0, 0, 0, 255]);
    }

    #[test]
    fn compose_center_borders_small_image() {
        let src = solid(2, 2, Rgba([10, 20, 30, 255]));
        let size = Size::new(6, 6);
        let out = compose(&src, Mode::Center, RED, size);
        assert_eq!(bgra_at(&out, size, 0, 0), [0, 0, 255, 255]); // border = red
        assert_eq!(bgra_at(&out, size, 3, 3), [30, 20, 10, 255]); // image, BGRA
    }

    #[test]
    fn compose_blends_semi_transparent_over_fill() {
        // 50% alpha white over red → roughly (128,128,255) in BGRA-ish
        let src = solid(1, 1, Rgba([255, 255, 255, 128]));
        let out = compose(&src, Mode::Stretch, RED, Size::new(1, 1));
        let [b, g, r, a] = [out[0], out[1], out[2], out[3]];
        assert_eq!(a, 255);
        assert!((120..=136).contains(&b), "b={b}");
        assert!((120..=136).contains(&g), "g={g}");
        assert!(r >= 250, "r={r}"); // red channel stays high
    }

    #[test]
    fn compose_zero_dst_is_empty() {
        let src = solid(4, 4, Rgba([1, 2, 3, 255]));
        assert!(compose(&src, Mode::Fill, RED, Size::new(0, 5)).is_empty());
    }

    #[test]
    fn solid_fills_the_whole_buffer_with_the_pattern() {
        for size in [
            Size::new(1, 1),
            Size::new(3, 5),
            Size::new(64, 40),
            Size::new(2879, 1),
        ] {
            let c = Color {
                r: 10,
                g: 20,
                b: 30,
                a: 255,
            };
            let out = super::solid(c, size);
            assert_eq!(out.len(), size.w as usize * size.h as usize * 4);
            for px in out.chunks_exact(4) {
                assert_eq!(px, c.to_shm_bgra());
            }
        }
    }

    #[test]
    fn solid_black_is_opaque_zero_rgb() {
        let out = super::solid(Color::BLACK, Size::new(17, 9));
        assert_eq!(out.len(), 17 * 9 * 4);
        for px in out.chunks_exact(4) {
            assert_eq!(px, [0, 0, 0, 255]); // Xrgb8888: R=G=B=0, X=0xff
        }
    }

    #[test]
    fn compose_output_stride_is_width_times_four() {
        let src = solid(3, 3, Rgba([9, 9, 9, 255]));
        let out = compose(&src, Mode::Fill, RED, Size::new(7, 5));
        assert_eq!(out.len(), 7 * 5 * 4);
    }
}
