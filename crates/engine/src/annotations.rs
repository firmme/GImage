use std::{path::PathBuf, sync::OnceLock};

use ab_glyph::{Font, FontArc, FontVec, ScaleFont, point};
use gimage_core::{Annotation, AnnotationKind, EditParams, RgbaImage};
use image::{Pixel, Rgba};
use tiny_skia::{FillRule, LineCap, LineJoin, Mask, Path, PathBuilder, Stroke, Transform};

pub fn draw(image: &mut RgbaImage, params: &EditParams) {
    draw_from(image, params, 0);
}

pub fn draw_from(image: &mut RgbaImage, params: &EditParams, start: usize) {
    for mark in params.annotations.iter().skip(start) {
        let points: Vec<_> = mark
            .points
            .iter()
            .map(|point| {
                let point = point.to_canvas(params);
                (point.x * image.width as f32, point.y * image.height as f32)
            })
            .collect();
        let width = (mark.width * image.width.min(image.height) as f32).max(1.0);
        match mark.kind {
            AnnotationKind::Mosaic => mosaic(image, points[0], points[1], width),
            AnnotationKind::Text => text(image, mark, points[0], width),
            _ => stroke(image, mark, &points, width),
        }
    }
}

fn stroke(image: &mut RgbaImage, mark: &Annotation, points: &[(f32, f32)], width: f32) {
    let kind = mark.kind;
    let mut builder = PathBuilder::new();
    let (x, y) = points[0];
    let path = if points.iter().all(|p| *p == points[0]) {
        builder.push_circle(x, y, width * 0.5);
        builder.finish()
    } else {
        if matches!(kind, AnnotationKind::Line | AnnotationKind::Arrow) {
            for segment in mark.style.line_segments(points[0], points[1], width) {
                builder.move_to(segment[0].0, segment[0].1);
                for &(x, y) in &segment[1..] {
                    builder.line_to(x, y);
                }
            }
        } else {
            builder.move_to(x, y);
            for &(x, y) in &points[1..] {
                builder.line_to(x, y);
            }
        }
        if kind == AnnotationKind::Polygon {
            builder.close();
        } else if kind == AnnotationKind::Arrow {
            let (bx, by) = points[1];
            let length = (bx - x).hypot(by - y);
            let head = (width * 4.0).max(6.0).min(length * 0.65);
            let (dx, dy) = ((bx - x) / length, (by - y) / length);
            builder.move_to(
                bx - dx * head - dy * head * 0.5,
                by - dy * head + dx * head * 0.5,
            );
            builder.line_to(bx, by);
            builder.line_to(
                bx - dx * head + dy * head * 0.5,
                by - dy * head - dx * head * 0.5,
            );
        }
        builder.finish().and_then(|path| {
            path.stroke(
                &Stroke {
                    width,
                    line_cap: LineCap::Round,
                    line_join: LineJoin::Round,
                    ..Default::default()
                },
                1.0,
            )
        })
    };
    if let Some(path) = path {
        paint(
            image,
            &path,
            mark.color,
            if kind == AnnotationKind::Highlight {
                92
            } else {
                255
            },
        );
    }
}

fn paint(image: &mut RgbaImage, path: &Path, rgb: [u8; 3], alpha: u8) {
    let bounds = path.bounds();
    let x0 = (bounds.left().floor() as u32).min(image.width);
    let x1 = (bounds.right().ceil() as u32).min(image.width);
    let y0 = (bounds.top().floor() as u32).min(image.height);
    let y1 = (bounds.bottom().ceil() as u32).min(image.height);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let width = x1 - x0;
    // Bound scratch space independently of the full image height.
    for y in (y0..y1).step_by(128) {
        let Some(mut mask) = Mask::new(width, (y1 - y).min(128)) else {
            return;
        };
        mask.fill_path(
            path,
            FillRule::Winding,
            true,
            Transform::from_translate(-(x0 as f32), -(y as f32)),
        );
        for (row, coverage) in mask.data().chunks_exact(width as usize).enumerate() {
            for (column, mask) in coverage.iter().copied().enumerate() {
                if mask != 0 {
                    blend(
                        image,
                        x0 + column as u32,
                        y + row as u32,
                        rgb,
                        (u16::from(mask) * u16::from(alpha) / 255) as u8,
                    );
                }
            }
        }
    }
}

fn blend(image: &mut RgbaImage, x: u32, y: u32, [r, g, b]: [u8; 3], alpha: u8) {
    let index = (y as usize * image.width as usize + x as usize) * 4;
    let mut dst = Rgba(image.pixels[index..index + 4].try_into().unwrap());
    dst.blend(&Rgba([r, g, b, alpha]));
    image.pixels[index..index + 4].copy_from_slice(&dst.0);
}

fn mosaic(image: &mut RgbaImage, a: (f32, f32), b: (f32, f32), width: f32) {
    let left = (a.0.min(b.0).floor() as u32).min(image.width);
    let top = (a.1.min(b.1).floor() as u32).min(image.height);
    let right = (a.0.max(b.0).ceil() as u32).min(image.width);
    let bottom = (a.1.max(b.1).ceil() as u32).min(image.height);
    let step = width.round().max(2.0) as usize;
    for y in (top..bottom).step_by(step) {
        for x in (left..right).step_by(step) {
            let end_y = (y + step as u32).min(bottom);
            let end_x = (x + step as u32).min(right);
            let mut sums = [0u64; 4];
            for row in y..end_y {
                let offset = (row as usize * image.width as usize + x as usize) * 4;
                for pixel in image.pixels[offset..offset + (end_x - x) as usize * 4].chunks_exact(4)
                {
                    for (sum, value) in sums.iter_mut().zip(pixel) {
                        *sum += u64::from(*value);
                    }
                }
            }
            let count = u64::from(end_x - x) * u64::from(end_y - y);
            let rgba = sums.map(|sum| (sum / count) as u8);
            for row in y..end_y {
                let offset = (row as usize * image.width as usize + x as usize) * 4;
                for pixel in
                    image.pixels[offset..offset + (end_x - x) as usize * 4].chunks_exact_mut(4)
                {
                    pixel.copy_from_slice(&rgba);
                }
            }
        }
    }
}

fn fonts() -> &'static [FontArc] {
    static FONTS: OnceLock<Vec<FontArc>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let windows = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:/Windows"));
        [
            windows.join("Fonts/msyh.ttc"),
            windows.join("Fonts/segoeui.ttf"),
            PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
            PathBuf::from("/System/Library/Fonts/Supplemental/Arial.ttf"),
        ]
        .iter()
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|bytes| FontVec::try_from_vec_and_index(bytes, 0).ok())
        .map(FontArc::new)
        .collect()
    })
}

pub fn text_available() -> bool {
    !fonts().is_empty()
}

fn text(image: &mut RgbaImage, mark: &Annotation, at: (f32, f32), size: f32) {
    let fonts = fonts();
    let Some(first) = fonts.first() else { return };
    let size = size.max(8.0);
    let baseline = first.as_scaled(size).ascent();
    for (row, line) in mark.text.lines().enumerate() {
        let y = at.1 + baseline + row as f32 * size * 1.3;
        if y - baseline >= image.height as f32 {
            break;
        }
        let mut x = at.0;
        for ch in line.chars() {
            let font = fonts
                .iter()
                .find(|font| font.glyph_id(ch).0 != 0)
                .unwrap_or(first);
            let font = font.as_scaled(size);
            let id = font.glyph_id(ch);
            let mut glyph = id.with_scale(size);
            glyph.position = point(x, y);
            if let Some(outline) = font.outline_glyph(glyph) {
                let origin = outline.px_bounds().min;
                outline.draw(|dx, dy, coverage| {
                    let (px, py) = (origin.x as i64 + dx as i64, origin.y as i64 + dy as i64);
                    if px >= 0 && py >= 0 && px < image.width as i64 && py < image.height as i64 {
                        blend(
                            image,
                            px as u32,
                            py as u32,
                            mark.color,
                            (coverage * 255.0).round() as u8,
                        );
                    }
                });
            }
            x += font.h_advance(id);
            if x >= image.width as f32 {
                break;
            }
        }
    }
}
