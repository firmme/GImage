//! Engine tests. All fixtures are generated programmatically; no external
//! image files are required.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use gimage_core::{
    CropRect, EditParams, EngineError, EngineOptions, ExportFormat, ExportOptions, ImageEngine,
    ImageFormat, MetadataPolicy, OutputPolicy, ResizeSpec, RgbaImage, Rotation,
};
use tempfile::TempDir;

use super::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn engine() -> RustImageEngine {
    RustImageEngine::new(EngineOptions::default())
}

fn engine_with_limit(max_decode_pixels: u64) -> RustImageEngine {
    RustImageEngine::new(EngineOptions { max_decode_pixels })
}

fn temp_dir() -> TempDir {
    tempfile::tempdir().expect("create temp dir")
}

fn pixel(image: &RgbaImage, x: u32, y: u32) -> [u8; 4] {
    assert!(x < image.width && y < image.height, "pixel out of bounds");
    let offset = ((y as usize * image.width as usize) + x as usize) * 4;
    image.pixels[offset..offset + 4]
        .try_into()
        .expect("4 bytes per pixel")
}

fn set_pixel(image: &mut RgbaImage, x: u32, y: u32, value: [u8; 4]) {
    let offset = ((y as usize * image.width as usize) + x as usize) * 4;
    image.pixels[offset..offset + 4].copy_from_slice(&value);
}

/// 3x2 image whose six pixels are all different and asymmetric.
fn asymmetric_image() -> RgbaImage {
    let colors = [
        [255, 0, 0, 255],   // red
        [0, 255, 0, 255],   // green
        [0, 0, 255, 255],   // blue
        [255, 255, 0, 255], // yellow
        [0, 255, 255, 255], // cyan
        [255, 0, 255, 255], // magenta
    ];
    let mut pixels = Vec::with_capacity(colors.len() * 4);
    for color in colors {
        pixels.extend_from_slice(&color);
    }
    RgbaImage::new(3, 2, pixels)
}

/// Pixel whose value encodes its own coordinates, for geometry assertions.
fn coordinate_image(width: u32, height: u32) -> RgbaImage {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&[(x * 20) as u8, (y * 30) as u8, 100, 255]);
        }
    }
    RgbaImage::new(width, height, pixels)
}

/// Opaque 32x32 image with four large flat colour quadrants (JPEG-friendly).
fn block_image(size: u32) -> RgbaImage {
    let colors = [
        [200, 30, 30, 255],
        [30, 200, 30, 255],
        [30, 30, 200, 255],
        [220, 200, 30, 255],
    ];
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let column = usize::from(x >= size / 2);
            let row = usize::from(y >= size / 2);
            pixels.extend_from_slice(&colors[row * 2 + column]);
        }
    }
    RgbaImage::new(size, size, pixels)
}

/// RGBA gradient with alpha values including fully transparent/opaque.
fn gradient_image(width: u32, height: u32) -> RgbaImage {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let red = ((x * 17 + y * 3) % 256) as u8;
            let green = ((x * 5 + y * 29) % 256) as u8;
            let blue = ((x * 31 + y * 7) % 256) as u8;
            let alpha = ((x * 13 + y * 23) % 256) as u8;
            pixels.extend_from_slice(&[red, green, blue, alpha]);
        }
    }
    RgbaImage::new(width, height, pixels)
}

fn write_image(directory: &Path, name: &str, image: &RgbaImage) -> PathBuf {
    let path = directory.join(name);
    engine()
        .encode(image, ExportFormat::Png, 90, &path)
        .expect("encode fixture png");
    path
}

fn decode_with_engine(path: &Path) -> RgbaImage {
    engine()
        .render(path, &EditParams::default(), None)
        .expect("decode fixture")
}

fn tone_params(exposure: f32, contrast: f32, saturation: f32, temperature: f32) -> EditParams {
    EditParams {
        exposure,
        contrast,
        saturation,
        temperature,
        ..EditParams::default()
    }
}

fn assert_pixels_eq(actual: &RgbaImage, expected: &RgbaImage) {
    assert_eq!(
        (actual.width, actual.height),
        (expected.width, expected.height),
        "dimensions differ"
    );
    assert_eq!(actual.pixels, expected.pixels, "pixel data differs");
}

/// BMP header with huge dimensions and no pixel payload. The decoder reports
/// the dimensions, but any real decode would be impossible.
fn huge_bmp_header(width: i32, height: i32) -> Vec<u8> {
    let mut file = Vec::new();
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&0u32.to_le_bytes()); // file size (unused)
    file.extend_from_slice(&0u32.to_le_bytes()); // reserved
    file.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
    file.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
    file.extend_from_slice(&width.to_le_bytes());
    file.extend_from_slice(&height.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes()); // planes
    file.extend_from_slice(&24u16.to_le_bytes()); // bpp
    file.extend_from_slice(&0u32.to_le_bytes()); // compression
    file.extend_from_slice(&0u32.to_le_bytes()); // image size
    file.extend_from_slice(&2835i32.to_le_bytes()); // x pixels/metre
    file.extend_from_slice(&2835i32.to_le_bytes()); // y pixels/metre
    file.extend_from_slice(&0u32.to_le_bytes()); // palette colours
    file.extend_from_slice(&0u32.to_le_bytes()); // important colours
    file
}

/// Insert an EXIF APP1 segment with only the Orientation tag right after SOI.
fn inject_exif_orientation(jpeg: &[u8], orientation: u8) -> Vec<u8> {
    assert!(jpeg.starts_with(&[0xFF, 0xD8]), "JPEG must start with SOI");

    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II"); // little endian
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
    tiff.extend_from_slice(&1u16.to_le_bytes()); // one entry
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes()); // count
    tiff.extend_from_slice(&u16::from(orientation).to_le_bytes());
    tiff.extend_from_slice(&[0u8, 0u8]); // pad the 4-byte value field
    tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD

    let mut payload = b"Exif\0\0".to_vec();
    payload.extend_from_slice(&tiff);
    let length = (payload.len() + 2) as u16;

    let mut out = Vec::with_capacity(jpeg.len() + payload.len() + 4);
    out.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xE1]);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&payload);
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn write_jpeg_with_orientation(
    directory: &Path,
    name: &str,
    image: &RgbaImage,
    orientation: u8,
) -> PathBuf {
    let path = directory.join(name);
    engine()
        .encode(image, ExportFormat::Jpeg, 95, &path)
        .expect("encode jpeg fixture");
    let patched = inject_exif_orientation(&fs::read(&path).expect("read jpeg"), orientation);
    fs::write(&path, patched).expect("write patched jpeg");
    path
}

fn leftovers(directory: &Path) -> Vec<String> {
    fs::read_dir(directory)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".gimage-tmp-") || name.ends_with(".tmp"))
        .collect()
}

fn export_options(format: ExportFormat, output: OutputPolicy) -> ExportOptions {
    ExportOptions {
        format,
        quality: 90,
        resize: None,
        metadata: MetadataPolicy::Strip,
        output,
        ..ExportOptions::default()
    }
}

// ---------------------------------------------------------------------------
// Capabilities and engine properties
// ---------------------------------------------------------------------------

#[test]
fn capabilities_report_runtime_codecs() {
    let rust_engine = engine();
    let capabilities = rust_engine.capabilities();

    assert_eq!(rust_engine.name(), "pure-rust");
    assert_eq!(rust_engine.version(), env!("CARGO_PKG_VERSION"));
    assert_eq!(capabilities.engine_name, "pure-rust");
    assert_eq!(capabilities.engine_version, env!("CARGO_PKG_VERSION"));

    for format in [
        ImageFormat::Jpeg,
        ImageFormat::Png,
        ImageFormat::WebP,
        ImageFormat::Gif,
        ImageFormat::Bmp,
        ImageFormat::Tiff,
        ImageFormat::Ico,
    ] {
        assert!(
            capabilities.readable.contains(&format),
            "{format:?} should be readable"
        );
        assert!(capabilities.can_read(&format));
    }

    for format in [
        ImageFormat::Jpeg,
        ImageFormat::Png,
        ImageFormat::WebP,
        ImageFormat::Bmp,
        ImageFormat::Tiff,
    ] {
        assert!(
            capabilities.writable.contains(&format),
            "{format:?} should be writable"
        );
        assert!(capabilities.can_write(&format));
    }
    assert!(!capabilities.can_write(&ImageFormat::Gif));
    assert!(!capabilities.can_write(&ImageFormat::Ico));

    assert!(
        capabilities.single_frame_only.contains(&ImageFormat::Gif),
        "GIF must be reported as first-frame-only"
    );
    assert_eq!(
        capabilities.single_frame_only,
        vec![
            ImageFormat::Gif,
            ImageFormat::WebP,
            ImageFormat::Png,
            ImageFormat::Ico,
        ],
        "animation-capable containers are decoded as one frame only"
    );

    // The probe is cached in a process-wide OnceLock.
    assert_eq!(capabilities, engine().capabilities());
}

#[test]
fn engine_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RustImageEngine>();
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

#[test]
fn probe_reads_header_only() {
    let directory = temp_dir();
    let image = gradient_image(7, 5);
    let path = write_image(directory.path(), "probe.png", &image);

    let info = engine().probe(&path).expect("probe png");
    assert_eq!(info.width, 7);
    assert_eq!(info.height, 5);
    assert_eq!(info.format, ImageFormat::Png);
    assert_eq!(info.orientation, 1);
    assert!(info.has_alpha, "RGBA PNG should report alpha");
    assert!(!info.animated);
    assert_eq!(info.frames, 1);
}

#[test]
fn probe_reads_exif_orientation() {
    let directory = temp_dir();
    // Non-square image so orientation is observable.
    let mut source = RgbaImage::new(30, 20, vec![0; 30 * 20 * 4]);
    for y in 0..20 {
        for x in 0..30 {
            let color = if x < 15 {
                [220, 20, 20, 255]
            } else {
                [20, 20, 220, 255]
            };
            set_pixel(&mut source, x, y, color);
        }
    }
    let path = write_jpeg_with_orientation(directory.path(), "rotated.jpg", &source, 6);

    let info = engine().probe(&path).expect("probe exif jpeg");
    assert_eq!(info.format, ImageFormat::Jpeg);
    assert_eq!((info.width, info.height), (30, 20));
    assert_eq!(info.orientation, 6);
    assert_eq!(info.oriented_size(), (20, 30));

    let thumbnail = engine().thumbnail(&path, 100).expect("oriented thumbnail");
    assert_eq!((thumbnail.width, thumbnail.height), (20, 30));

    let rendered = engine()
        .render(&path, &EditParams::default(), None)
        .expect("oriented render");
    assert_eq!((rendered.width, rendered.height), (20, 30));
    // Source left half is red; after 90 degrees CW it becomes the top half.
    let top_left = pixel(&rendered, 2, 2);
    let bottom_left = pixel(&rendered, 2, 27);
    assert!(
        top_left[0] > 180 && top_left[2] < 100,
        "top should be red-ish: {top_left:?}"
    );
    assert!(
        bottom_left[2] > 180 && bottom_left[0] < 100,
        "bottom should be blue-ish: {bottom_left:?}"
    );
}

#[test]
fn probe_handles_unsupported_and_unreadable_files() {
    let directory = temp_dir();
    let missing = directory.path().join("missing.png");
    assert!(matches!(
        engine().probe(&missing),
        Err(EngineError::Io { .. })
    ));

    let random = directory.path().join("random.bin");
    fs::write(&random, b"this is not an image at all").expect("write random");
    assert!(matches!(
        engine().probe(&random),
        Err(EngineError::Unsupported(_))
    ));

    // A non-image with an image extension is still rejected (as a decode error,
    // because the extension fallback selects a decoder that then fails).
    let fake_png = directory.path().join("fake.png");
    fs::write(&fake_png, b"this is not an image at all").expect("write fake png");
    assert!(engine().probe(&fake_png).is_err());

    let empty = directory.path().join("empty.png");
    fs::write(&empty, b"").expect("write empty");
    assert!(engine().probe(&empty).is_err());

    let truncated = directory.path().join("truncated.png");
    let image = coordinate_image(4, 4);
    let full = directory.path().join("full.png");
    engine()
        .encode(&image, ExportFormat::Png, 90, &full)
        .expect("encode full");
    let bytes = fs::read(&full).expect("read full");
    fs::write(&truncated, &bytes[..8]).expect("write truncated");
    assert!(engine().probe(&truncated).is_err());
    assert!(
        engine()
            .thumbnail(&truncated, 32)
            .expect_err("thumbnail must fail")
            .to_string()
            .contains("无法")
    );
}

// ---------------------------------------------------------------------------
// Orientation
// ---------------------------------------------------------------------------

#[test]
fn orientation_maps_all_eight_exif_values() {
    let red = [255, 0, 0, 255];
    let green = [0, 255, 0, 255];
    let blue = [0, 0, 255, 255];
    let yellow = [255, 255, 0, 255];
    let cyan = [0, 255, 255, 255];
    let magenta = [255, 0, 255, 255];

    // (orientation, width, height, pixel(0,0), pixel(w-1,h-1), pixel(1,0))
    type OrientationCase = (u16, u32, u32, [u8; 4], [u8; 4], [u8; 4]);
    let cases: &[OrientationCase] = &[
        (1, 3, 2, red, magenta, green),
        (2, 3, 2, blue, yellow, green),
        (3, 3, 2, magenta, red, cyan),
        (4, 3, 2, yellow, blue, cyan),
        (5, 2, 3, red, magenta, yellow),
        (6, 2, 3, yellow, blue, red),
        (7, 2, 3, magenta, red, blue),
        (8, 2, 3, blue, yellow, magenta),
    ];

    for &(orientation, width, height, first, last, top_second) in cases {
        let source = asymmetric_image();
        let transformed = apply_orientation(source.clone(), orientation);
        assert_eq!(
            (transformed.width, transformed.height),
            (width, height),
            "orientation {orientation} dimensions"
        );
        assert_eq!(
            pixel(&transformed, 0, 0),
            first,
            "orientation {orientation} top-left"
        );
        assert_eq!(
            pixel(&transformed, width - 1, height - 1),
            last,
            "orientation {orientation} bottom-right"
        );
        assert_eq!(
            pixel(&transformed, 1, 0),
            top_second,
            "orientation {orientation} (1,0)"
        );
    }
}

#[test]
fn invalid_orientation_is_identity() {
    let source = asymmetric_image();
    for orientation in [0u16, 9, 42, u16::MAX] {
        assert_eq!(apply_orientation(source.clone(), orientation), source);
    }
}

// ---------------------------------------------------------------------------
// Crop / rotate / flip geometry
// ---------------------------------------------------------------------------

#[test]
fn crop_rotate_flip_geometry() {
    let directory = temp_dir();
    let source = coordinate_image(8, 8);
    let path = write_image(directory.path(), "geometry.png", &source);

    let params = EditParams {
        crop: Some(CropRect {
            x: 0.25,
            y: 0.25,
            width: 0.5,
            height: 0.5,
        }),
        rotation: Rotation::Cw90,
        flip_horizontal: true,
        ..EditParams::default()
    };
    let rendered = engine()
        .render(&path, &params, None)
        .expect("geometry render");
    assert_eq!((rendered.width, rendered.height), (4, 4));

    // Crop is pixels (2,2)-(5,5); rotate90 then flip horizontal maps output
    // (ox, oy) to source (2 + oy, 2 + ox).
    for oy in 0..4 {
        for ox in 0..4 {
            let expected = pixel(&source, 2 + oy, 2 + ox);
            assert_eq!(
                pixel(&rendered, ox, oy),
                expected,
                "mismatch at ({ox},{oy})"
            );
        }
    }

    // Nothing outside the crop rectangle leaked through.
    for y in 0..4 {
        for x in 0..4 {
            let value = pixel(&rendered, x, y);
            assert!((40..=100).contains(&value[0]));
            assert!((60..=150).contains(&value[1]));
        }
    }
}

#[test]
fn extreme_crop_rects_are_clamped_without_panicking() {
    let directory = temp_dir();
    let source = write_image(directory.path(), "one-pixel.png", &coordinate_image(1, 1));
    let params = EditParams {
        crop: Some(CropRect {
            x: 0.995,
            y: 0.995,
            width: 0.005,
            height: 0.005,
        }),
        ..EditParams::default()
    };
    let rendered = engine()
        .render(&source, &params, None)
        .expect("extreme crop must not panic");
    assert_eq!((rendered.width, rendered.height), (1, 1));

    let larger = write_image(directory.path(), "hundred.png", &coordinate_image(100, 100));
    let rendered = engine()
        .render(&larger, &params, None)
        .expect("extreme crop must not panic");
    // The rect is degenerate enough that normalisation drops it, so this is a
    // full-size render (but it must not panic or truncate the image).
    assert_eq!((rendered.width, rendered.height), (100, 100));

    // 0.993 * 64 rounds up to 64, i.e. one pixel past the image edge; without
    // the defensive clamp in `crop_image` this would make `crop_imm` panic.
    let edge = write_image(directory.path(), "edge.png", &coordinate_image(64, 64));
    let edge_params = EditParams {
        crop: Some(CropRect {
            x: 0.993,
            y: 0.993,
            width: 0.007,
            height: 0.007,
        }),
        ..EditParams::default()
    };
    let rendered = engine()
        .render(&edge, &edge_params, None)
        .expect("edge crop must not panic");
    assert_eq!((rendered.width, rendered.height), (1, 1));
}

#[test]
fn rotation_swaps_axes_and_is_reversible() {
    let source = coordinate_image(7, 4);
    let quarter = rotate_image(source.clone(), Rotation::Cw90);
    assert_eq!((quarter.width, quarter.height), (4, 7));
    let back = rotate_image(quarter, Rotation::Cw270);
    assert_pixels_eq(&back, &source);

    let half = rotate_image(source.clone(), Rotation::Cw180);
    assert_eq!((half.width, half.height), (7, 4));
    assert_pixels_eq(&rotate_image(half, Rotation::Cw180), &source);
}

// ---------------------------------------------------------------------------
// Tone adjustments
// ---------------------------------------------------------------------------

#[test]
fn tone_adjustments_follow_documented_math() {
    let directory = temp_dir();
    let source = RgbaImage::new(
        3,
        1,
        vec![
            64, 128, 192, 77, // p0
            192, 128, 64, 200, // p1
            200, 100, 50, 0, // p2 (fully transparent, rgb still adjusted)
        ],
    );
    let path = write_image(directory.path(), "tone.png", &source);
    let render = |params: &EditParams| {
        engine()
            .render(&path, params, None)
            .expect("render tone fixture")
    };

    // Identity: exactly unchanged (PNG is lossless).
    let identity = render(&EditParams::default());
    assert_pixels_eq(&identity, &source);

    // Exposure is a per-channel factor of 2^EV, clamped.
    let brighter = render(&tone_params(1.0, 0.0, 0.0, 0.0));
    assert_eq!(pixel(&brighter, 0, 0), [128, 255, 255, 77]);
    assert_eq!(pixel(&brighter, 1, 0), [255, 255, 128, 200]);
    assert_eq!(pixel(&brighter, 2, 0), [255, 200, 100, 0]);

    let darker = render(&tone_params(-1.0, 0.0, 0.0, 0.0));
    assert_eq!(pixel(&darker, 0, 0), [32, 64, 96, 77]);
    assert_eq!(pixel(&darker, 1, 0), [96, 64, 32, 200]);
    assert_eq!(pixel(&darker, 2, 0), [100, 50, 25, 0]);

    // Contrast pushes values away from mid-grey.
    let contrast = render(&tone_params(0.0, 0.5, 0.0, 0.0));
    assert_eq!(pixel(&contrast, 0, 0), [32, 128, 224, 77]);
    assert_eq!(pixel(&contrast, 1, 0), [224, 128, 32, 200]);
    assert_eq!(pixel(&contrast, 2, 0), [236, 86, 11, 0]);

    // Saturation -1 collapses to luma, +1 exaggerates around luma.
    let grayscale = render(&tone_params(0.0, 0.0, -1.0, 0.0));
    assert_eq!(pixel(&grayscale, 0, 0), [119, 119, 119, 77]);
    assert_eq!(pixel(&grayscale, 1, 0), [137, 137, 137, 200]);
    assert_eq!(pixel(&grayscale, 2, 0), [118, 118, 118, 0]);

    let vivid = render(&tone_params(0.0, 0.0, 1.0, 0.0));
    assert_eq!(pixel(&vivid, 0, 0), [9, 137, 255, 77]);
    assert_eq!(pixel(&vivid, 2, 0), [255, 82, 0, 0]);

    // Temperature: +1 warms (red up, blue down), -1 cools.
    let warm = render(&tone_params(0.0, 0.0, 0.0, 1.0));
    assert_eq!(pixel(&warm, 0, 0), [77, 128, 154, 77]);
    assert_eq!(pixel(&warm, 2, 0), [240, 100, 40, 0]);

    let cool = render(&tone_params(0.0, 0.0, 0.0, -1.0));
    assert_eq!(pixel(&cool, 0, 0), [51, 128, 230, 77]);

    // Alpha is preserved in every single case above.
    for result in [
        identity, brighter, darker, contrast, grayscale, vivid, warm, cool,
    ] {
        for index in 0..3 {
            assert_eq!(
                pixel(&result, index, 0)[3],
                pixel(&source, index, 0)[3],
                "alpha changed at pixel {index}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Thumbnails and render sizing
// ---------------------------------------------------------------------------

#[test]
fn thumbnail_respects_max_edge_and_never_upscales() {
    let directory = temp_dir();
    let source = coordinate_image(100, 50);
    let path = write_image(directory.path(), "thumb.png", &source);

    let thumb = engine().thumbnail(&path, 40).expect("thumbnail 40");
    assert_eq!((thumb.width, thumb.height), (40, 20));

    let tall = engine().thumbnail(&path, 25).expect("thumbnail 25");
    assert_eq!((tall.width, tall.height), (25, 13));

    // Already smaller than the bound: never upscaled.
    let original = engine().thumbnail(&path, 10_000).expect("thumbnail huge");
    assert_eq!((original.width, original.height), (100, 50));
    assert_pixels_eq(&original, &source);

    let tiny = engine().thumbnail(&path, 1).expect("thumbnail 1");
    assert_eq!((tiny.width, tiny.height), (1, 1));

    // Edge case: engine still refuses to invent pixels for 0.
    let zero = engine().thumbnail(&path, 0).expect("thumbnail 0");
    assert_eq!((zero.width, zero.height), (1, 1));
}

#[test]
fn render_max_edge_fits_square_without_upscaling() {
    let directory = temp_dir();
    let source = coordinate_image(120, 60);
    let path = write_image(directory.path(), "render-edge.png", &source);

    let preview = engine()
        .render(&path, &EditParams::default(), Some(30))
        .expect("preview render");
    assert_eq!((preview.width, preview.height), (30, 15));

    let full = engine()
        .render(&path, &EditParams::default(), None)
        .expect("full render");
    assert_eq!((full.width, full.height), (120, 60));

    let no_upscale = engine()
        .render(&path, &EditParams::default(), Some(1000))
        .expect("large bound render");
    assert_eq!((no_upscale.width, no_upscale.height), (120, 60));
}

// ---------------------------------------------------------------------------
// Pixel limit
// ---------------------------------------------------------------------------

#[test]
fn pixel_limit_is_enforced_before_decoding() {
    let directory = temp_dir();
    // 60_000 x 60_000 = 3.6 billion pixels, but still inside BMP's per-axis
    // decoder limit. The header is readable; the payload is absent.
    let path = directory.path().join("huge.bmp");
    fs::write(&path, huge_bmp_header(60_000, 60_000)).expect("write huge bmp header");

    let engine = engine();
    let error = engine.probe(&path).expect_err("probe must reject");
    match error {
        EngineError::TooLarge {
            width,
            height,
            pixels,
            limit,
        } => {
            assert_eq!((width, height), (60_000, 60_000));
            assert_eq!(pixels, 3_600_000_000);
            assert_eq!(limit, EngineOptions::default().max_decode_pixels);
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }

    for result in [
        engine.thumbnail(&path, 128).map(|_| ()),
        engine
            .render(&path, &EditParams::default(), None)
            .map(|_| ()),
    ] {
        assert!(
            matches!(&result, Err(EngineError::TooLarge { .. })),
            "expected TooLarge before decode, got {result:?}"
        );
    }
}

#[test]
fn pixel_limit_rejects_small_files_with_tiny_limit() {
    let directory = temp_dir();
    let path = write_image(directory.path(), "small.png", &coordinate_image(100, 50));
    let engine = engine_with_limit(10);

    for result in [
        engine.probe(&path).map(|_| ()),
        engine.thumbnail(&path, 16).map(|_| ()),
        engine
            .render(&path, &EditParams::default(), None)
            .map(|_| ()),
    ] {
        assert!(
            matches!(
                &result,
                Err(EngineError::TooLarge {
                    pixels: 5000,
                    limit: 10,
                    ..
                })
            ),
            "expected TooLarge, got {result:?}"
        );
    }

    // Zero limit rejects everything non-empty, including a 1x1 image.
    let one = write_image(directory.path(), "one.png", &coordinate_image(1, 1));
    let zero = engine_with_limit(0);
    assert!(matches!(
        zero.probe(&one),
        Err(EngineError::TooLarge {
            pixels: 1,
            limit: 0,
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// Encoder roundtrips
// ---------------------------------------------------------------------------

#[test]
fn lossless_formats_roundtrip_exactly() {
    let directory = temp_dir();
    let source = gradient_image(13, 11);

    for (format, name) in [
        (ExportFormat::Png, "roundtrip.png"),
        (ExportFormat::Bmp, "roundtrip.bmp"),
        (ExportFormat::Tiff, "roundtrip.tif"),
        (ExportFormat::WebP, "roundtrip.webp"),
    ] {
        let path = directory.path().join(name);
        engine()
            .encode(&source, format, 90, &path)
            .unwrap_or_else(|error| panic!("encode {format:?}: {error}"));

        let info = engine().probe(&path).expect("probe roundtrip");
        let expected_format = match format {
            ExportFormat::Png => ImageFormat::Png,
            ExportFormat::Bmp => ImageFormat::Bmp,
            ExportFormat::Tiff => ImageFormat::Tiff,
            ExportFormat::WebP => ImageFormat::WebP,
            ExportFormat::Jpeg => unreachable!(),
        };
        assert_eq!(info.format, expected_format, "{format:?}");
        assert_eq!((info.width, info.height), (13, 11));

        let decoded = decode_with_engine(&path);
        assert_pixels_eq(&decoded, &source);
    }
}

#[test]
fn jpeg_roundtrip_composites_alpha_on_white_and_honours_quality() {
    let directory = temp_dir();
    let source = block_image(32);
    let path = directory.path().join("roundtrip.jpg");

    engine()
        .encode(&source, ExportFormat::Jpeg, 95, &path)
        .expect("encode jpeg");
    let info = engine().probe(&path).expect("probe jpeg");
    assert_eq!(info.format, ImageFormat::Jpeg);
    assert_eq!((info.width, info.height), (32, 32));
    assert!(!info.has_alpha);

    let decoded = decode_with_engine(&path);
    assert_eq!((decoded.width, decoded.height), (32, 32));
    // Sample the middle of each quadrant, away from JPEG ringing at edges.
    for (x, y, expected) in [
        (7u32, 7u32, [200u8, 30, 30]),
        (24, 7, [30, 200, 30]),
        (7, 24, [30, 30, 200]),
        (24, 24, [220, 200, 30]),
    ] {
        let actual = pixel(&decoded, x, y);
        assert_eq!(actual[3], 255, "JPEG output is opaque");
        for channel in 0..3 {
            let difference = i32::from(actual[channel]) - i32::from(expected[channel]);
            assert!(
                difference.abs() <= 20,
                "quadrant ({x},{y}) channel {channel}: {actual:?} vs {expected:?}"
            );
        }
    }

    // A quality-10 file must exist and be smaller than the quality-95 one.
    let low = directory.path().join("low.jpg");
    engine()
        .encode(&source, ExportFormat::Jpeg, 10, &low)
        .expect("encode low quality jpeg");
    let high_size = fs::metadata(&path).expect("high metadata").len();
    let low_size = fs::metadata(&low).expect("low metadata").len();
    assert!(low_size < high_size, "quality should affect JPEG size");

    // Transparent input is flattened onto white, not black.
    let transparent = RgbaImage::new(2, 2, [0u8, 0, 0, 0].repeat(4));
    let flattened = directory.path().join("flattened.jpg");
    engine()
        .encode(&transparent, ExportFormat::Jpeg, 100, &flattened)
        .expect("encode transparent jpeg");
    let decoded = decode_with_engine(&flattened);
    let center = pixel(&decoded, 1, 1);
    assert!(
        center[0] > 240 && center[1] > 240 && center[2] > 240,
        "transparent pixels should become white: {center:?}"
    );
}

#[test]
fn encode_rejects_malformed_buffers() {
    let directory = temp_dir();
    let path = directory.path().join("bad.png");
    let malformed = RgbaImage {
        width: 4,
        height: 4,
        pixels: vec![0u8; 10],
    };
    let error = engine()
        .encode(&malformed, ExportFormat::Png, 90, &path)
        .expect_err("malformed buffer must fail");
    assert!(matches!(error, EngineError::Encode { .. }));
    assert!(!path.exists(), "encoder must not create a partial file");
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[test]
fn export_autorename_avoids_collisions_and_cleans_temp_files() {
    let directory = temp_dir();
    let source_image = coordinate_image(20, 10);
    let source = write_image(directory.path(), "source.png", &source_image);
    let source_bytes = fs::read(&source).expect("read source");
    let destination = directory.path().join("out.png");

    let request = ExportRequest {
        source: source.clone(),
        destination: destination.clone(),
        params: EditParams::default(),
        options: export_options(ExportFormat::Png, OutputPolicy::AutoRename),
    };

    let mut exported = Vec::new();
    for expected_name in ["out.png", "out (2).png", "out (3).png"] {
        let status = export_image(&engine(), &request).expect("export");
        let ExportStatus::Written(outcome) = status else {
            panic!("expected Written");
        };
        assert_eq!(
            outcome.path.file_name().unwrap().to_string_lossy(),
            expected_name
        );
        assert_eq!(
            outcome.bytes,
            fs::metadata(&outcome.path).expect("metadata").len()
        );
        let decoded = decode_with_engine(&outcome.path);
        assert_eq!((decoded.width, decoded.height), (20, 10));
        exported.push(outcome.path);
    }

    assert_eq!(
        fs::read(&source).expect("read source again"),
        source_bytes,
        "source must never be modified"
    );
    assert!(
        leftovers(directory.path()).is_empty(),
        "temp files left behind"
    );

    for path in exported {
        assert!(path.exists());
    }
    // Extra safety: the final destination directory contains exactly the four
    // expected files and nothing else.
    let mut names: Vec<String> = fs::read_dir(directory.path())
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "out (2).png".to_string(),
            "out (3).png".to_string(),
            "out.png".to_string(),
            "source.png".to_string(),
        ]
    );
}

#[test]
fn export_skip_existing_returns_skipped() {
    let directory = temp_dir();
    let source = write_image(directory.path(), "source.png", &coordinate_image(8, 6));
    let destination = directory.path().join("exists.png");
    fs::write(&destination, b"sentinel").expect("write sentinel");

    let request = ExportRequest {
        source: source.clone(),
        destination: destination.clone(),
        params: EditParams::default(),
        options: export_options(ExportFormat::Png, OutputPolicy::SkipExisting),
    };
    let status = export_image(&engine(), &request).expect("skip export");
    assert!(matches!(status, ExportStatus::Skipped));
    assert_eq!(fs::read(&destination).expect("read dest"), b"sentinel");
    assert!(leftovers(directory.path()).is_empty());
}

#[test]
fn export_overwrite_replaces_destination_but_keeps_source() {
    let directory = temp_dir();
    let source_image = coordinate_image(12, 9);
    let source = write_image(directory.path(), "source.png", &source_image);
    let source_bytes = fs::read(&source).expect("read source");
    let destination = directory.path().join("overwrite.png");
    fs::write(&destination, b"old contents").expect("write old destination");

    let request = ExportRequest {
        source: source.clone(),
        destination: destination.clone(),
        params: EditParams {
            rotation: Rotation::Cw90,
            ..EditParams::default()
        },
        options: export_options(ExportFormat::Png, OutputPolicy::Overwrite),
    };
    let status = export_image(&engine(), &request).expect("overwrite export");
    let ExportStatus::Written(outcome) = status else {
        panic!("expected Written");
    };
    assert_eq!(outcome.path, destination);
    let decoded = decode_with_engine(&destination);
    assert_eq!((decoded.width, decoded.height), (9, 12));
    assert_eq!(fs::read(&source).expect("read source"), source_bytes);
    assert!(leftovers(directory.path()).is_empty());
}

#[test]
fn export_refuses_to_write_over_source() {
    let directory = temp_dir();
    let source = write_image(directory.path(), "source.png", &coordinate_image(10, 10));
    let source_bytes = fs::read(&source).expect("read source");

    let request = ExportRequest {
        source: source.clone(),
        destination: source.clone(),
        params: EditParams::default(),
        options: export_options(ExportFormat::Png, OutputPolicy::Overwrite),
    };
    let error = export_image(&engine(), &request).expect_err("must refuse");
    let message = error.to_string();
    assert!(
        message.contains("源文件"),
        "error should mention the source: {message}"
    );
    assert_eq!(
        fs::read(&source).expect("read source unchanged"),
        source_bytes
    );

    // A disguised path (with a `.`/`..` component) canonicalises to the source.
    let disguised = directory.path().join(".").join("source.png");
    let request = ExportRequest {
        source: source.clone(),
        destination: disguised,
        params: EditParams::default(),
        options: export_options(ExportFormat::Png, OutputPolicy::Overwrite),
    };
    assert!(export_image(&engine(), &request).is_err());

    // AutoRename is still allowed: it writes a sibling, never the source.
    let request = ExportRequest {
        source: source.clone(),
        destination: source.clone(),
        params: EditParams::default(),
        options: export_options(ExportFormat::Png, OutputPolicy::AutoRename),
    };
    let status = export_image(&engine(), &request).expect("auto-renamed export");
    let ExportStatus::Written(outcome) = status else {
        panic!("expected Written");
    };
    assert_eq!(
        outcome.path.file_name().unwrap().to_string_lossy(),
        "source (2).png"
    );
    assert_eq!(fs::read(&source).expect("source unchanged"), source_bytes);
}

#[test]
fn export_applies_resize_after_full_resolution_render() {
    let directory = temp_dir();
    let source = write_image(directory.path(), "resize.png", &coordinate_image(40, 20));
    let destination = directory.path().join("small.png");

    let request = ExportRequest {
        source: source.clone(),
        destination,
        params: EditParams {
            rotation: Rotation::Cw90,
            ..EditParams::default()
        },
        options: ExportOptions {
            format: ExportFormat::Png,
            quality: 90,
            resize: Some(ResizeSpec::new(10, 100)),
            metadata: MetadataPolicy::Strip,
            output: OutputPolicy::Overwrite,
            ..ExportOptions::default()
        },
    };
    let status = export_image(&engine(), &request).expect("resized export");
    let ExportStatus::Written(outcome) = status else {
        panic!("expected Written");
    };
    // render: 40x20 -> rotated 20x40 -> resize fit(10,100) -> 10x20.
    let decoded = decode_with_engine(&outcome.path);
    assert_eq!((decoded.width, decoded.height), (10, 20));
}

#[test]
fn export_failure_removes_temp_file() {
    let directory = temp_dir();
    let destination = directory.path().join("never.png");
    let malformed = RgbaImage {
        width: 4,
        height: 4,
        pixels: vec![0u8; 7],
    };
    let error = write_atomic(&engine(), &malformed, ExportFormat::Png, 90, &destination)
        .expect_err("write_atomic must fail");
    assert!(matches!(error, EngineError::Encode { .. }));
    assert!(!destination.exists());
    assert!(leftovers(directory.path()).is_empty());

    // A missing parent directory fails before creating anything either.
    let missing = directory.path().join("missing").join("out.png");
    let image = coordinate_image(2, 2);
    assert!(write_atomic(&engine(), &image, ExportFormat::Png, 90, &missing).is_err());
    assert!(leftovers(directory.path()).is_empty());
}

#[test]
fn write_atomic_overwrites_existing_file_and_reports_size() {
    let directory = temp_dir();
    let destination = directory.path().join("atomic.png");
    fs::write(&destination, b"old").expect("write old");

    let image = gradient_image(9, 7);
    let bytes =
        write_atomic(&engine(), &image, ExportFormat::Png, 90, &destination).expect("atomic write");
    assert_eq!(bytes, fs::metadata(&destination).expect("metadata").len());
    assert_pixels_eq(&decode_with_engine(&destination), &image);
    assert!(leftovers(directory.path()).is_empty());
}

// ---------------------------------------------------------------------------
// Path resolution helpers
// ---------------------------------------------------------------------------

#[test]
fn unique_destination_and_policies() {
    let directory = temp_dir();
    let base = directory.path().join("photo.png");
    assert_eq!(unique_destination(&base), base);

    fs::write(&base, b"x").expect("write base");
    let second = directory.path().join("photo (2).png");
    assert_eq!(unique_destination(&base), second);
    fs::write(&second, b"x").expect("write second");
    assert_eq!(
        unique_destination(&base),
        directory.path().join("photo (3).png")
    );

    let no_extension = directory.path().join("image");
    fs::write(&no_extension, b"x").expect("write no extension");
    assert_eq!(
        unique_destination(&no_extension),
        directory.path().join("image (2)")
    );

    assert_eq!(
        resolve_destination(&base, OutputPolicy::AutoRename),
        Some(directory.path().join("photo (3).png"))
    );
    assert_eq!(resolve_destination(&base, OutputPolicy::SkipExisting), None);
    assert_eq!(
        resolve_destination(&base, OutputPolicy::Overwrite),
        Some(base.clone())
    );
    let missing = directory.path().join("missing.png");
    assert_eq!(
        resolve_destination(&missing, OutputPolicy::SkipExisting),
        Some(missing.clone())
    );
    assert_eq!(
        resolve_destination(&missing, OutputPolicy::AutoRename),
        Some(missing)
    );
}

// ---------------------------------------------------------------------------
// Concurrency and robustness
// ---------------------------------------------------------------------------

#[test]
fn concurrent_renders_are_independent() {
    let directory = temp_dir();
    let source = write_image(directory.path(), "shared.png", &coordinate_image(64, 32));
    let engine = Arc::new(engine());

    let mut handles = Vec::new();
    for index in 0..4u32 {
        let engine = Arc::clone(&engine);
        let source = source.clone();
        handles.push(thread::spawn(move || {
            let params = EditParams {
                rotation: if index % 2 == 0 {
                    Rotation::Cw90
                } else {
                    Rotation::None
                },
                flip_horizontal: index >= 2,
                ..EditParams::default()
            };
            let image = engine
                .render(&source, &params, Some(16))
                .expect("concurrent render");
            assert!(image.width <= 16 && image.height <= 16);
            assert!(!engine.capabilities().readable.is_empty());
        }));
    }
    for handle in handles {
        handle.join().expect("worker thread");
    }
}

#[test]
fn malformed_input_never_panics() {
    let directory = temp_dir();
    let cases: [(&str, &[u8]); 3] = [
        ("empty.png", b""),
        ("garbage.jpg", b"not a jpeg"),
        ("short.gif", b"GIF89a"),
    ];
    for (name, bytes) in cases {
        let path = directory.path().join(name);
        fs::write(&path, bytes).expect("write malformed");
        let _ = engine().probe(&path);
        let _ = engine().thumbnail(&path, 32);
        let _ = engine().render(&path, &EditParams::default(), None);
    }
}

// ---------------------------------------------------------------------------
// Sanity checks for test fixtures
// ---------------------------------------------------------------------------

#[test]
fn fixture_helpers_are_sane() {
    let image = asymmetric_image();
    assert_eq!((image.width, image.height), (3, 2));
    assert_eq!(image.pixels.len(), 3 * 2 * 4);

    let mut copy = asymmetric_image();
    set_pixel(&mut copy, 2, 1, [1, 2, 3, 4]);
    assert_eq!(pixel(&copy, 2, 1), [1, 2, 3, 4]);
    assert_eq!(pixel(&copy, 1, 1), [0, 255, 255, 255]);
}

#[test]
fn annotation_export_keeps_source_and_maps_marks_through_geometry() {
    use gimage_core::{Annotation, AnnotationKind, MarkPoint};

    let temp = temp_dir();
    let input = RgbaImage::new(64, 64, [15, 30, 45, 255].repeat(64 * 64));
    let source = write_image(temp.path(), "source.png", &input);
    let destination = temp.path().join("marked.png");
    let make_point = |x, y| MarkPoint { x, y };
    let mark = Annotation {
        kind: AnnotationKind::Line,
        points: vec![make_point(0.25, 0.25), make_point(0.75, 0.25)],
        color: [250, 80, 20],
        width: 0.1,
        style: Default::default(),
        text: String::new(),
    };
    let params = EditParams {
        rotation: Rotation::Cw90,
        annotations: vec![Arc::new(mark)],
        ..EditParams::default()
    };
    let restored = EditParams::from_json(&params.to_json());
    assert_eq!(restored, params);
    let preview = engine().render(&source, &restored, Some(32)).unwrap();
    assert_eq!(pixel(&preview, 24, 16), [250, 80, 20, 255]);
    assert_eq!(pixel(&preview, 16, 8), [15, 30, 45, 255]);

    export_image(
        &engine(),
        &ExportRequest {
            source: source.clone(),
            destination: destination.clone(),
            params: restored,
            options: ExportOptions {
                format: ExportFormat::Png,
                ..ExportOptions::default()
            },
        },
    )
    .unwrap();
    assert_eq!(
        pixel(&decode_with_engine(&destination), 48, 32),
        [250, 80, 20, 255]
    );
    assert_eq!(decode_with_engine(&source).pixels, input.pixels);
}

#[test]
fn mosaic_averages_selected_blocks_and_preserves_surroundings() {
    use gimage_core::{Annotation, AnnotationKind, MarkPoint};

    let mut input = RgbaImage::new(40, 40, [0, 0, 0, 255].repeat(1600));
    for y in 0..40 {
        for x in 0..40 {
            if (x + y) % 2 == 1 {
                set_pixel(&mut input, x, y, [255, 255, 255, 255]);
            }
        }
    }
    let params = EditParams {
        annotations: vec![Arc::new(Annotation {
            kind: AnnotationKind::Mosaic,
            points: vec![
                MarkPoint { x: 0.25, y: 0.25 },
                MarkPoint { x: 0.75, y: 0.75 },
            ],
            color: [0, 0, 0],
            width: 0.1,
            style: Default::default(),
            text: String::new(),
        })],
        ..EditParams::default()
    };
    crate::annotations::draw(&mut input, &params);
    assert_eq!(pixel(&input, 10, 10), [127, 127, 127, 255]);
    assert_eq!(pixel(&input, 29, 29), [127, 127, 127, 255]);
    assert_eq!(pixel(&input, 30, 29), [255, 255, 255, 255]);
}
