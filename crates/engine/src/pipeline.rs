//! Pixel pipeline: EXIF orientation, crop, rotate, flip, tone and resizing.
//!
//! The required order (fixed by the product spec) is:
//!
//! `decode -> EXIF orientation -> crop -> rotate -> flip -> tone -> resize`
//!
//! # Tone math
//!
//! [`apply_tone`] is the single source of truth for colour adjustments; a
//! future GPU preview must reproduce this function. For every pixel the RGB
//! channels are treated as `f32` values in `0.0..=255.0` and processed in this
//! order (alpha is copied through untouched):
//!
//! 1. **Exposure** (`-3..=3` EV): `v = v * 2^exposure`.
//! 2. **Contrast** (`-1..=1`): `v = (v / 255 - 0.5) * (1 + contrast) + 0.5`,
//!    then scaled back to `0..=255`. Note that the value is clamped into
//!    `0..=1` *after* the formula before being scaled back.
//! 3. **Saturation** (`-1..=1`): with
//!    `luma = 0.2126 R + 0.7152 G + 0.0722 B` (computed after step 2),
//!    `v = luma + (v - luma) * (1 + saturation)`.
//! 4. **Temperature** (`-1..=1`): `R = R * (1 + 0.2 t)`,
//!    `B = B * (1 - 0.2 t)`, `G` unchanged.
//!
//! Every stage clamps its channels back into `0.0..=255.0`. The final `u8`
//! value is `round(v)` (ties away from zero). With all four parameters at zero
//! the function is an exact no-op and returns without touching the buffer.
//!
//! Crop rectangles are normalised to the *oriented* image (`0..=1`) and are
//! snapped with [`gimage_core::CropRect::to_pixels`]. Rotations are limited to
//! exact multiples of 90 degrees. Resizing only ever downscales: images that
//! already fit are returned unchanged.
//!
//! Thumbnails use [`image::imageops::thumbnail`] (fast, box-ish filter);
//! render-time resizing uses a Lanczos3 filter for quality.

use gimage_core::{CropRect, EditParams, EngineError, RgbaImage, Rotation};
use image::RgbaImage as ImageBuffer;
use image::imageops::{self, FilterType};

use crate::RustImageEngine;
use crate::codec;

/// A decoded image converted back to the backend-neutral representation.
pub(crate) fn from_image_buffer(buffer: ImageBuffer) -> RgbaImage {
    let (width, height) = buffer.dimensions();
    RgbaImage::new(width, height, buffer.into_raw())
}

/// Consume a validated RGBA image into an `image` buffer without copying the
/// pixels. Callers must check [`valid`] first.
fn into_image_buffer(image: RgbaImage) -> ImageBuffer {
    let RgbaImage {
        width,
        height,
        pixels,
    } = image;
    ImageBuffer::from_raw(width, height, pixels)
        .expect("validated RgbaImage invariant: pixels.len() == width * height * 4")
}

fn valid(image: &RgbaImage) -> bool {
    u64::from(image.width) * u64::from(image.height) * 4 == image.pixels.len() as u64
}

/// Apply an EXIF orientation value (1..=8) to already-decoded pixels.
///
/// Mapping (matching the EXIF spec and `image`'s `Orientation` enum):
///
/// | value | transform |
/// |-------|-----------|
/// | 1 | none |
/// | 2 | flip horizontal |
/// | 3 | rotate 180 |
/// | 4 | flip vertical |
/// | 5 | rotate 90 CW then flip horizontal |
/// | 6 | rotate 90 CW |
/// | 7 | rotate 270 CW then flip horizontal |
/// | 8 | rotate 270 CW |
///
/// Any other value (and malformed pixel buffers) is returned unchanged rather
/// than panicking.
pub fn apply_orientation(image: RgbaImage, orientation: u16) -> RgbaImage {
    if !valid(&image) || image.is_empty() || orientation <= 1 || orientation > 8 {
        return image;
    }
    let mut buffer = into_image_buffer(image);
    let transformed = match orientation {
        2 => {
            imageops::flip_horizontal_in_place(&mut buffer);
            buffer
        }
        3 => {
            imageops::rotate180_in_place(&mut buffer);
            buffer
        }
        4 => {
            imageops::flip_vertical_in_place(&mut buffer);
            buffer
        }
        5 => {
            let rotated = imageops::rotate90(&buffer);
            imageops::flip_horizontal(&rotated)
        }
        6 => imageops::rotate90(&buffer),
        7 => {
            let rotated = imageops::rotate270(&buffer);
            imageops::flip_horizontal(&rotated)
        }
        8 => imageops::rotate270(&buffer),
        _ => buffer,
    };
    from_image_buffer(transformed)
}

/// Rotate by an exact multiple of 90 degrees (clockwise).
pub fn rotate_image(image: RgbaImage, rotation: Rotation) -> RgbaImage {
    if rotation == Rotation::None || !valid(&image) || image.is_empty() {
        return image;
    }
    let mut buffer = into_image_buffer(image);
    let transformed = match rotation {
        Rotation::None => buffer,
        Rotation::Cw90 => imageops::rotate90(&buffer),
        Rotation::Cw180 => {
            imageops::rotate180_in_place(&mut buffer);
            buffer
        }
        Rotation::Cw270 => imageops::rotate270(&buffer),
    };
    from_image_buffer(transformed)
}

/// Flip horizontally (mirror left/right).
pub(crate) fn flip_horizontal(image: RgbaImage) -> RgbaImage {
    if !valid(&image) || image.is_empty() {
        return image;
    }
    let mut buffer = into_image_buffer(image);
    imageops::flip_horizontal_in_place(&mut buffer);
    from_image_buffer(buffer)
}

/// Flip vertically (mirror top/bottom).
pub(crate) fn flip_vertical(image: RgbaImage) -> RgbaImage {
    if !valid(&image) || image.is_empty() {
        return image;
    }
    let mut buffer = into_image_buffer(image);
    imageops::flip_vertical_in_place(&mut buffer);
    from_image_buffer(buffer)
}

/// Crop to a normalised rectangle relative to the current (oriented) image.
pub(crate) fn crop_image(image: RgbaImage, crop: CropRect) -> RgbaImage {
    if !valid(&image) || image.is_empty() {
        return image;
    }
    let clamped = crop.clamped();
    if !clamped.is_usable() || clamped.is_full() {
        return image;
    }
    let (x, y, width, height) = clamped.to_pixels(image.width, image.height);
    // `CropRect::to_pixels` can round `x` up to the image width for very
    // extreme (but formally usable) rectangles. Clamp here so malformed or
    // edge-case parameters can never make `crop_imm` panic.
    let x = x.min(image.width - 1);
    let y = y.min(image.height - 1);
    let width = width.min(image.width - x).max(1);
    let height = height.min(image.height - y).max(1);
    let buffer = into_image_buffer(image);
    from_image_buffer(imageops::crop_imm(&buffer, x, y, width, height).to_image())
}

/// Apply exposure/contrast/saturation/temperature in the documented order.
/// Alpha is never modified. See the module docs for the exact math.
pub(crate) fn apply_tone(image: &mut RgbaImage, params: &EditParams) {
    let exposure = params.exposure;
    let contrast = params.contrast;
    let saturation = params.saturation;
    let temperature = params.temperature;
    if exposure.abs() <= f32::EPSILON
        && contrast.abs() <= f32::EPSILON
        && saturation.abs() <= f32::EPSILON
        && temperature.abs() <= f32::EPSILON
    {
        return;
    }

    let exposure_factor = 2.0f32.powf(exposure);
    let contrast_factor = 1.0 + contrast;
    let saturation_factor = 1.0 + saturation;
    let red_factor = 1.0 + 0.2 * temperature;
    let blue_factor = 1.0 - 0.2 * temperature;

    let (pixels, _) = image.pixels.as_chunks_mut::<4>();
    for pixel in pixels {
        let mut red = f32::from(pixel[0]);
        let mut green = f32::from(pixel[1]);
        let mut blue = f32::from(pixel[2]);

        // 1. Exposure.
        red = (red * exposure_factor).clamp(0.0, 255.0);
        green = (green * exposure_factor).clamp(0.0, 255.0);
        blue = (blue * exposure_factor).clamp(0.0, 255.0);

        // 2. Contrast.
        red = contrast_channel(red, contrast_factor);
        green = contrast_channel(green, contrast_factor);
        blue = contrast_channel(blue, contrast_factor);

        // 3. Saturation around the luma of the current RGB values.
        let luma = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
        red = (luma + (red - luma) * saturation_factor).clamp(0.0, 255.0);
        green = (luma + (green - luma) * saturation_factor).clamp(0.0, 255.0);
        blue = (luma + (blue - luma) * saturation_factor).clamp(0.0, 255.0);

        // 4. Temperature: warm shifts red up and blue down.
        red = (red * red_factor).clamp(0.0, 255.0);
        blue = (blue * blue_factor).clamp(0.0, 255.0);

        pixel[0] = round_channel(red);
        pixel[1] = round_channel(green);
        pixel[2] = round_channel(blue);
        // pixel[3] (alpha) is deliberately left untouched.
    }
}

fn contrast_channel(value: f32, factor: f32) -> f32 {
    ((value / 255.0 - 0.5) * factor + 0.5).clamp(0.0, 1.0) * 255.0
}

fn round_channel(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

/// Target size inside a square of `max_edge`, preserving aspect ratio and
/// never upscaling.
pub(crate) fn fit_edge(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (width, height);
    }
    let edge = u64::from(max_edge.max(1));
    if u64::from(width) <= edge && u64::from(height) <= edge {
        return (width, height);
    }
    let scale = (edge as f64 / f64::from(width)).min(edge as f64 / f64::from(height));
    let target_width = ((f64::from(width) * scale).round() as u32).max(1);
    let target_height = ((f64::from(height) * scale).round() as u32).max(1);
    (target_width, target_height)
}

/// Fast thumbnail downscale to fit a square bound. Never upscales.
pub(crate) fn resize_thumbnail(image: RgbaImage, max_edge: u32) -> RgbaImage {
    let (target_width, target_height) = fit_edge(image.width, image.height, max_edge);
    if (target_width, target_height) == (image.width, image.height) || !valid(&image) {
        return image;
    }
    let buffer = into_image_buffer(image);
    from_image_buffer(imageops::thumbnail(&buffer, target_width, target_height))
}

/// High-quality downscale to fit a square bound. Never upscales.
pub(crate) fn resize_fit(image: RgbaImage, max_edge: u32) -> RgbaImage {
    let (target_width, target_height) = fit_edge(image.width, image.height, max_edge);
    if (target_width, target_height) == (image.width, image.height) || !valid(&image) {
        return image;
    }
    let buffer = into_image_buffer(image);
    from_image_buffer(imageops::resize(
        &buffer,
        target_width,
        target_height,
        FilterType::Lanczos3,
    ))
}

/// Downscale to an exact target size with a Lanczos3 filter. Used by
/// [`crate::export_image`] after rendering; never upscales and ignores empty
/// targets.
pub(crate) fn resize_to_exact(
    image: RgbaImage,
    target_width: u32,
    target_height: u32,
) -> RgbaImage {
    if !valid(&image)
        || target_width == 0
        || target_height == 0
        || target_width > image.width
        || target_height > image.height
    {
        return image;
    }
    if (target_width, target_height) == (image.width, image.height) {
        return image;
    }
    let buffer = into_image_buffer(image);
    from_image_buffer(imageops::resize(
        &buffer,
        target_width,
        target_height,
        FilterType::Lanczos3,
    ))
}

/// Decode once and downscale before applying EXIF orientation.
pub(crate) fn thumbnail(
    engine: &RustImageEngine,
    path: &std::path::Path,
    max_edge: u32,
) -> Result<RgbaImage, EngineError> {
    let decoded = codec::decode_for_view(engine, path, Some(max_edge), true)?;
    let thumbnail = resize_thumbnail(decoded.image, max_edge);
    Ok(apply_orientation(thumbnail, decoded.orientation))
}

/// Full editor/export pipeline.
pub(crate) fn render(
    engine: &RustImageEngine,
    path: &std::path::Path,
    params: &EditParams,
    max_edge: Option<u32>,
) -> Result<RgbaImage, EngineError> {
    let params = params.normalized();
    let edge = if params.is_identity() { max_edge } else { None };
    let decoded = codec::decode_for_view(engine, path, edge, false)?;

    let mut image = apply_orientation(decoded.image, decoded.orientation);
    if let Some(crop) = params.crop {
        image = crop_image(image, crop);
    }
    image = rotate_image(image, params.rotation);
    if params.flip_horizontal {
        image = flip_horizontal(image);
    }
    if params.flip_vertical {
        image = flip_vertical(image);
    }
    apply_tone(&mut image, &params);
    if let Some(edge) = max_edge {
        image = resize_fit(image, edge);
    }
    crate::annotations::draw(&mut image, &params);
    Ok(image)
}
