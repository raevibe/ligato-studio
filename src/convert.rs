//! Frame processing: color range (Bypass/Expand/Shrink) and picture
//! settings applied in software, plus downscaled RGBA preview conversion.
//!
//! Elgato Studio applies these on the card (`AT_Set_ColorRange`, procamp KS
//! properties). The sc0710 driver only exposes the procamp *read* (MCU sub
//! 0x12), with no write path and no hardware range conversion, so this app
//! does both on the CPU, for preview and recording alike.

use crate::settings::{ColorRange, Picture};
use crate::v4l2::{PIX_BGR24, PIX_NV12, PIX_YUYV};
use rayon::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameFormat {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl FrameFormat {
    pub fn tight_stride(&self) -> u32 {
        match self.fourcc {
            PIX_YUYV => self.width * 2,
            PIX_BGR24 => self.width * 3,
            _ => self.width,
        }
    }

    /// Size of a tightly packed frame.
    pub fn tight_size(&self) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        match self.fourcc {
            PIX_YUYV => w * h * 2,
            PIX_BGR24 => w * h * 3,
            PIX_NV12 => w * h + w * h.div_ceil(2),
            _ => 0,
        }
    }

    pub fn supported(&self) -> bool {
        matches!(self.fourcc, PIX_YUYV | PIX_BGR24 | PIX_NV12) && self.width > 0 && self.height > 0
    }

    /// Copies `src` (with driver stride) into a tightly packed buffer.
    pub fn pack(&self, src: &[u8], dst: &mut Vec<u8>) -> bool {
        let need = self.tight_size();
        dst.resize(need, 0);
        let ts = self.tight_stride() as usize;
        let ss = (self.stride as usize).max(ts);
        let rows = match self.fourcc {
            PIX_NV12 => self.height as usize + (self.height as usize).div_ceil(2),
            _ => self.height as usize,
        };
        if ss == ts {
            if src.len() < need {
                return false;
            }
            dst.copy_from_slice(&src[..need]);
            return true;
        }
        if src.len() < ss * (rows - 1) + ts {
            return false;
        }
        for r in 0..rows {
            dst[r * ts..(r + 1) * ts].copy_from_slice(&src[r * ss..r * ss + ts]);
        }
        true
    }
}

pub struct Processor {
    identity: bool,
    y_lut: [u8; 256],
    /// (u | v << 8) → (u' | v' << 8)
    uv_lut: Vec<u16>,
    rgb_lut: [u8; 256],
    /// 3x3 saturation/hue matrix in Q12 for RGB data, None when identity.
    rgb_matrix: Option<[[i32; 3]; 3]>,
}

fn clamp_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

impl Processor {
    pub fn new(range: ColorRange, pic: Picture) -> Processor {
        let identity = range == ColorRange::Bypass && pic.is_identity();
        let contrast = pic.contrast as f32 / 100.0;
        let bright = pic.brightness as f32 * 219.0 / 200.0;
        let sat = pic.saturation as f32 / 100.0;
        let (hs, hc) = (pic.hue as f32).to_radians().sin_cos();

        let range_y = |y: f32| match range {
            ColorRange::Bypass => y,
            ColorRange::Expand => (y - 16.0) * 255.0 / 219.0,
            ColorRange::Shrink => 16.0 + y * 219.0 / 255.0,
        };
        let range_c = |c: f32| match range {
            ColorRange::Bypass => c,
            ColorRange::Expand => c * 255.0 / 224.0,
            ColorRange::Shrink => c * 224.0 / 255.0,
        };
        let range_rgb = |v: f32| match range {
            ColorRange::Bypass => v,
            ColorRange::Expand => (v - 16.0) * 255.0 / 219.0,
            ColorRange::Shrink => 16.0 + v * 219.0 / 255.0,
        };

        let mut y_lut = [0u8; 256];
        let mut rgb_lut = [0u8; 256];
        for i in 0..256 {
            let y = range_y(i as f32);
            y_lut[i] = clamp_u8((y - 16.0) * contrast + 16.0 + bright);
            let v = range_rgb(i as f32);
            rgb_lut[i] = clamp_u8((v - 128.0) * contrast + 128.0 + bright);
        }
        let mut uv_lut = Vec::new();
        if !identity {
            uv_lut = vec![0u16; 65536];
            for v in 0..256usize {
                for u in 0..256usize {
                    let cu = range_c(u as f32 - 128.0);
                    let cv = range_c(v as f32 - 128.0);
                    let ru = (cu * hc - cv * hs) * sat + 128.0;
                    let rv = (cu * hs + cv * hc) * sat + 128.0;
                    uv_lut[u | (v << 8)] = clamp_u8(ru) as u16 | ((clamp_u8(rv) as u16) << 8);
                }
            }
        }
        let rgb_matrix = if pic.saturation == 100 && pic.hue == 0 {
            None
        } else {
            // Hue rotation about the luma axis combined with saturation.
            let (lr, lg, lb) = (0.2126f32, 0.7152f32, 0.0722f32);
            let k = 1.0 / 3f32.sqrt();
            let rot = [
                [hc + (1.0 - hc) / 3.0, (1.0 - hc) / 3.0 - k * hs, (1.0 - hc) / 3.0 + k * hs],
                [(1.0 - hc) / 3.0 + k * hs, hc + (1.0 - hc) / 3.0, (1.0 - hc) / 3.0 - k * hs],
                [(1.0 - hc) / 3.0 - k * hs, (1.0 - hc) / 3.0 + k * hs, hc + (1.0 - hc) / 3.0],
            ];
            let satm = [
                [lr * (1.0 - sat) + sat, lg * (1.0 - sat), lb * (1.0 - sat)],
                [lr * (1.0 - sat), lg * (1.0 - sat) + sat, lb * (1.0 - sat)],
                [lr * (1.0 - sat), lg * (1.0 - sat), lb * (1.0 - sat) + sat],
            ];
            let mut m = [[0i32; 3]; 3];
            for (r, row) in m.iter_mut().enumerate() {
                for (c, cell) in row.iter_mut().enumerate() {
                    let v: f32 = (0..3).map(|k| satm[r][k] * rot[k][c]).sum();
                    *cell = (v * 4096.0).round() as i32;
                }
            }
            Some(m)
        };
        Processor { identity, y_lut, uv_lut, rgb_lut, rgb_matrix }
    }

    #[inline]
    fn uv(&self, u: u8, v: u8) -> (u8, u8) {
        if self.identity {
            return (u, v);
        }
        let o = self.uv_lut[u as usize | ((v as usize) << 8)];
        (o as u8, (o >> 8) as u8)
    }

    #[inline]
    fn rgb(&self, r: u8, g: u8, b: u8) -> (u8, u8, u8) {
        if self.identity {
            return (r, g, b);
        }
        let (r, g, b) = (self.rgb_lut[r as usize], self.rgb_lut[g as usize], self.rgb_lut[b as usize]);
        match &self.rgb_matrix {
            None => (r, g, b),
            Some(m) => {
                let f = |row: &[i32; 3]| {
                    ((row[0] * r as i32 + row[1] * g as i32 + row[2] * b as i32 + 2048) >> 12).clamp(0, 255) as u8
                };
                (f(&m[0]), f(&m[1]), f(&m[2]))
            }
        }
    }

    /// Applies range + picture settings to a tightly packed frame in place.
    pub fn process(&self, fmt: &FrameFormat, buf: &mut [u8]) {
        if self.identity {
            return;
        }
        let w = fmt.width as usize;
        let h = fmt.height as usize;
        match fmt.fourcc {
            PIX_YUYV => buf[..w * h * 2].par_chunks_mut(w * 2).for_each(|row| {
                for px in row.as_chunks_mut::<4>().0 {
                    px[0] = self.y_lut[px[0] as usize];
                    px[2] = self.y_lut[px[2] as usize];
                    let (u, v) = self.uv(px[1], px[3]);
                    px[1] = u;
                    px[3] = v;
                }
            }),
            PIX_NV12 => {
                let (yp, uvp) = buf.split_at_mut(w * h);
                yp.par_chunks_mut(w).for_each(|row| {
                    for p in row {
                        *p = self.y_lut[*p as usize];
                    }
                });
                uvp.par_chunks_mut(w).for_each(|row| {
                    for px in row.as_chunks_mut::<2>().0 {
                        let (u, v) = self.uv(px[0], px[1]);
                        px[0] = u;
                        px[1] = v;
                    }
                });
            }
            PIX_BGR24 => buf[..w * h * 3].par_chunks_mut(w * 3).for_each(|row| {
                for px in row.as_chunks_mut::<3>().0 {
                    let (r, g, b) = self.rgb(px[2], px[1], px[0]);
                    px[0] = b;
                    px[1] = g;
                    px[2] = r;
                }
            }),
            _ => {}
        }
    }

    /// Builds a downscaled RGBA preview (nearest-neighbour) straight from the
    /// driver buffer, applying the same processing as the recorder.
    pub fn preview(&self, fmt: &FrameFormat, src: &[u8], max_w: u32, full_range: bool) -> Option<(usize, usize, Vec<u8>)> {
        if !fmt.supported() {
            return None;
        }
        let step = fmt.width.div_ceil(max_w.max(64)).max(1) as usize;
        let ow = fmt.width as usize / step;
        let oh = fmt.height as usize / step;
        let stride = (fmt.stride.max(fmt.tight_stride())) as usize;
        let rows = match fmt.fourcc {
            PIX_NV12 => fmt.height as usize + (fmt.height as usize).div_ceil(2),
            _ => fmt.height as usize,
        };
        let need = stride * (rows - 1) + fmt.tight_stride() as usize;
        if src.len() < need || ow == 0 || oh == 0 {
            return None;
        }
        let mut out = vec![0u8; ow * oh * 4];
        let yuv = |y: u8, u: u8, v: u8| -> [u8; 4] {
            let y = self.y_lut_or(y);
            let (u, v) = self.uv(u, v);
            yuv_to_rgb(y, u, v, full_range)
        };
        out.par_chunks_mut(ow * 4).enumerate().for_each(|(oy, orow)| {
            let sy = oy * step;
            for ox in 0..ow {
                let sx = ox * step;
                let px = match fmt.fourcc {
                    PIX_YUYV => {
                        let base = sy * stride + (sx & !1) * 2;
                        let y = src[sy * stride + sx * 2];
                        yuv(y, src[base + 1], src[base + 3])
                    }
                    PIX_NV12 => {
                        let y = src[sy * stride + sx];
                        let uvb = fmt.height as usize * stride + (sy / 2) * stride + (sx & !1);
                        yuv(y, src[uvb], src[uvb + 1])
                    }
                    _ => {
                        let o = sy * stride + sx * 3;
                        let (r, g, b) = self.rgb(src[o + 2], src[o + 1], src[o]);
                        [r, g, b, 255]
                    }
                };
                orow[ox * 4..ox * 4 + 4].copy_from_slice(&px);
            }
        });
        Some((ow, oh, out))
    }

    #[inline]
    fn y_lut_or(&self, y: u8) -> u8 {
        if self.identity { y } else { self.y_lut[y as usize] }
    }
}

/// BT.709 YCbCr → RGBA, fixed point.
#[inline]
pub fn yuv_to_rgb(y: u8, u: u8, v: u8, full_range: bool) -> [u8; 4] {
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let (r, g, b) = if full_range {
        let c = (y as i32) << 8;
        (c + 403 * e, c - 48 * d - 120 * e, c + 475 * d)
    } else {
        let c = 298 * (y as i32 - 16);
        (c + 459 * e, c - 55 * d - 136 * e, c + 541 * d)
    };
    let f = |x: i32| ((x + 128) >> 8).clamp(0, 255) as u8;
    [f(r), f(g), f(b), 255]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limited_white_and_black() {
        assert_eq!(yuv_to_rgb(235, 128, 128, false), [255, 255, 255, 255]);
        assert_eq!(yuv_to_rgb(16, 128, 128, false), [0, 0, 0, 255]);
        assert_eq!(yuv_to_rgb(255, 128, 128, true), [255, 255, 255, 255]);
    }

    #[test]
    fn identity_is_noop() {
        let p = Processor::new(ColorRange::Bypass, Picture::default());
        assert!(p.identity);
        let fmt = FrameFormat { fourcc: PIX_YUYV, width: 2, height: 1, stride: 4 };
        let mut b = vec![10, 20, 30, 40];
        p.process(&fmt, &mut b);
        assert_eq!(b, vec![10, 20, 30, 40]);
    }

    #[test]
    fn expand_maps_limited_to_full() {
        let p = Processor::new(ColorRange::Expand, Picture::default());
        let fmt = FrameFormat { fourcc: PIX_YUYV, width: 2, height: 1, stride: 4 };
        let mut b = vec![16, 128, 235, 128];
        p.process(&fmt, &mut b);
        assert_eq!(b, vec![0, 128, 255, 128]);
    }

    #[test]
    fn pack_strips_stride() {
        let fmt = FrameFormat { fourcc: PIX_YUYV, width: 2, height: 2, stride: 6 };
        let src = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8, 0, 0];
        let mut dst = Vec::new();
        assert!(fmt.pack(&src, &mut dst));
        assert_eq!(dst, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn preview_downscales() {
        let p = Processor::new(ColorRange::Bypass, Picture::default());
        let fmt = FrameFormat { fourcc: PIX_YUYV, width: 256, height: 4, stride: 512 };
        let src = vec![128u8; 512 * 4];
        let (w, h, px) = p.preview(&fmt, &src, 128, false).unwrap();
        assert_eq!((w, h), (128, 2));
        assert_eq!(px.len(), 128 * 2 * 4);
    }
}
