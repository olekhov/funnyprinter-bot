use std::{fs, path::Path};

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result};
use funnyprint_proto::{BYTES_PER_LINE, MAX_DOTS_PER_LINE, PackedLine};
use image::{GrayImage, Luma};
use imageproc::drawing::draw_text_mut;

#[derive(Debug, Clone)]
pub struct TextRenderOptions {
    pub width_px: u32,
    pub height_px: u32,
    pub x_px: i32,
    pub y_px: i32,
    pub font_size_px: f32,
    pub line_spacing: f32,
    pub threshold: u8,
    pub invert: bool,
    pub trim_blank_top_bottom: bool,
    pub outline_only: bool,
    pub outline_thickness_px: u32,
}

impl Default for TextRenderOptions {
    fn default() -> Self {
        Self {
            width_px: MAX_DOTS_PER_LINE as u32,
            height_px: 192,
            x_px: 0,
            y_px: 0,
            font_size_px: 48.0,
            line_spacing: 1.0,
            threshold: 180,
            invert: false,
            trim_blank_top_bottom: true,
            outline_only: false,
            outline_thickness_px: 1,
        }
    }
}

pub fn render_text_to_image(
    text: &str,
    font_path: &Path,
    opts: &TextRenderOptions,
) -> Result<GrayImage> {
    let bytes = fs::read(font_path)
        .with_context(|| format!("failed to read font file {}", font_path.display()))?;
    let font = FontArc::try_from_vec(bytes).context("failed to parse font")?;

    let mut img = GrayImage::from_pixel(opts.width_px, opts.height_px, Luma([255]));
    let scale = PxScale::from(opts.font_size_px);
    let scaled = font.as_scaled(scale);
    let line_h =
        ((scaled.ascent() - scaled.descent() + scaled.line_gap()) * opts.line_spacing).max(1.0);

    for (idx, line) in text.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let y = opts.y_px + (idx as f32 * line_h).round() as i32;
        draw_text_mut(&mut img, Luma([0]), opts.x_px, y, scale, &font, line);
    }

    if opts.outline_only {
        img = outline_from_mask(&img, opts.outline_thickness_px.max(1));
    }

    if opts.invert {
        for pixel in img.pixels_mut() {
            pixel.0[0] = 255u8.saturating_sub(pixel.0[0]);
        }
    }

    Ok(img)
}

fn outline_from_mask(src: &GrayImage, radius: u32) -> GrayImage {
    let w = src.width();
    let h = src.height();
    let mut out = GrayImage::from_pixel(w, h, Luma([255]));
    let r = radius as i32;

    for y in 0..h {
        for x in 0..w {
            // Keep only inner edge pixels (inside glyph), this produces a thinner contour.
            let center = src.get_pixel(x, y).0[0];
            if center >= 200 {
                continue;
            }
            let mut has_fg = false;
            let mut has_bg = false;
            for dy in -r..=r {
                for dx in -r..=r {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        has_bg = true;
                        continue;
                    }
                    let px = src.get_pixel(nx as u32, ny as u32).0[0];
                    if px < 200 {
                        has_fg = true;
                    } else {
                        has_bg = true;
                    }
                }
            }
            if has_fg && has_bg {
                out.put_pixel(x, y, Luma([0]));
            }
        }
    }
    out
}

pub fn image_to_packed_lines(img: &GrayImage, threshold: u8, trim_blank: bool) -> Vec<PackedLine> {
    let width = img.width().min(MAX_DOTS_PER_LINE as u32) as usize;
    let height = img.height() as usize;

    let mut out = Vec::with_capacity(height.div_ceil(2));

    for y in (0..height).step_by(2) {
        let mut line = [0u8; BYTES_PER_LINE * 2];

        for row in 0..2 {
            let yy = y + row;
            if yy >= height {
                continue;
            }
            for x in 0..width {
                let px = img.get_pixel(x as u32, yy as u32).0[0];
                let is_black = px <= threshold;
                if is_black {
                    let byte_idx = row * BYTES_PER_LINE + (x / 8);
                    let bit = 7 - (x % 8);
                    line[byte_idx] |= 1u8 << bit;
                }
            }
        }

        out.push(line);
    }

    if !trim_blank {
        return out;
    }

    let first = out.iter().position(|l| l.iter().any(|b| *b != 0));
    let last = out.iter().rposition(|l| l.iter().any(|b| *b != 0));

    match (first, last) {
        (Some(start), Some(end)) => out[start..=end].to_vec(),
        _ => Vec::new(),
    }
}

pub fn px_to_mm(px: u32, dpi: u16) -> f32 {
    px as f32 / dpi as f32 * 25.4
}
