//! Codec layer: header probing, decoding and encoding with the pure-Rust
//! [`image`] crate.
//!
//! # Format support
//!
//! Everything is probed at runtime (see [`capabilities`]) instead of being
//! assumed from the enabled cargo features: a tiny 2x2 RGBA image is encoded to
//! a temporary file and decoded again for every candidate format.
//!
//! * readable: JPEG, PNG, WebP, GIF, BMP, TIFF, ICO
//! * writable: JPEG, PNG, WebP, BMP, TIFF
//! * animated formats (GIF, animated WebP/APNG) and multi-image ICO files are
//!   decoded as a **single frame** only, so they are reported in
//!   [`CodecCapabilities::single_frame_only`].
//!
//! # Encoder notes
//!
//! * **JPEG**: `quality` (1..=100) is passed to the encoder. RGBA input is
//!   composited onto an opaque white background before encoding, because the
//!   JPEG encoder only accepts RGB.
//! * **PNG**: lossless RGBA. `quality` is ignored.
//! * **WebP**: `image` 0.25 only ships a *lossless* WebP encoder
//!   (`WebPEncoder::new_lossless`). `quality` is therefore **ignored**; a lossy
//!   WebP would require linking `libwebp`, which this pure-Rust backend
//!   deliberately does not do.
//! * **BMP**: RGBA (BITMAPV4 header with an alpha mask). `quality` ignored.
//! * **TIFF**: RGBA (uncompressed). `quality` ignored.
//!
//! No metadata (EXIF/IPTC/XMP) is copied on encode. Orientation is normalised
//! into the pixels by the pipeline, so output files are always upright.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use gimage_core::{
    CodecCapabilities, EngineError, ExportFormat, ImageFormat, ImageInfo, RgbaImage,
};
use image::codecs::bmp::BmpEncoder;
use image::codecs::gif::GifEncoder;
use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::tiff::TiffEncoder;
use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageReader, Limits};

use crate::RustImageEngine;

/// Formats we try to decode, in display order.
const READABLE_CANDIDATES: &[ImageFormat] = &[
    ImageFormat::Jpeg,
    ImageFormat::Png,
    ImageFormat::WebP,
    ImageFormat::Gif,
    ImageFormat::Bmp,
    ImageFormat::Tiff,
    ImageFormat::Ico,
];

/// Formats we expose through [`gimage_core::ImageEngine::encode`].
const WRITABLE_CANDIDATES: &[ExportFormat] = &[
    ExportFormat::Jpeg,
    ExportFormat::Png,
    ExportFormat::WebP,
    ExportFormat::Bmp,
    ExportFormat::Tiff,
];

/// Formats that are containers for animation / multiple images, but whose
/// later frames are dropped by this backend.
const SINGLE_FRAME_CANDIDATES: &[ImageFormat] = &[
    ImageFormat::Gif,
    ImageFormat::WebP,
    ImageFormat::Png,
    ImageFormat::Ico,
];

/// Capabilities are identical for every engine instance: they only depend on
/// the linked codecs, so probing once per process is enough.
static CAPABILITIES: OnceLock<CodecCapabilities> = OnceLock::new();

/// Probe (once per process) which codecs really work in this build.
pub(crate) fn capabilities() -> CodecCapabilities {
    CAPABILITIES.get_or_init(build_capabilities).clone()
}

/// Result of reading a file header. Deliberately contains no pixel data.
pub(crate) struct Header {
    pub(crate) format: ImageFormat,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Map an `image` crate format onto the backend-neutral one.
pub(crate) fn map_format(format: image::ImageFormat) -> ImageFormat {
    match format {
        image::ImageFormat::Jpeg => ImageFormat::Jpeg,
        image::ImageFormat::Png => ImageFormat::Png,
        image::ImageFormat::WebP => ImageFormat::WebP,
        image::ImageFormat::Gif => ImageFormat::Gif,
        image::ImageFormat::Bmp => ImageFormat::Bmp,
        image::ImageFormat::Tiff => ImageFormat::Tiff,
        image::ImageFormat::Ico => ImageFormat::Ico,
        other => ImageFormat::Other(
            other
                .extensions_str()
                .first()
                .copied()
                .unwrap_or("unknown")
                .to_string(),
        ),
    }
}

/// Map a backend-neutral readable format onto `image`'s enum.
fn image_format_of(format: &ImageFormat) -> Option<image::ImageFormat> {
    Some(match format {
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::WebP => image::ImageFormat::WebP,
        ImageFormat::Gif => image::ImageFormat::Gif,
        ImageFormat::Bmp => image::ImageFormat::Bmp,
        ImageFormat::Tiff => image::ImageFormat::Tiff,
        ImageFormat::Ico => image::ImageFormat::Ico,
        _ => return None,
    })
}

/// Core format corresponding to a writable export format.
fn core_format_of_export(format: ExportFormat) -> ImageFormat {
    match format {
        ExportFormat::Jpeg => ImageFormat::Jpeg,
        ExportFormat::Png => ImageFormat::Png,
        ExportFormat::WebP => ImageFormat::WebP,
        ExportFormat::Bmp => ImageFormat::Bmp,
        ExportFormat::Tiff => ImageFormat::Tiff,
    }
}

fn export_format_of_core(format: &ImageFormat) -> Option<ExportFormat> {
    WRITABLE_CANDIDATES
        .iter()
        .copied()
        .find(|candidate| core_format_of_export(*candidate) == *format)
}

pub fn replacement_format(path: &Path) -> Result<ExportFormat, EngineError> {
    let open = || {
        std::fs::File::open(path)
            .map(std::io::BufReader::new)
            .map_err(|error| EngineError::io(path, error))
    };
    let image_format = ImageReader::new(open()?)
        .with_guessed_format()
        .map_err(|error| EngineError::io(path, error))?
        .format()
        .map(map_format);
    let unsupported = || EngineError::Unsupported("此图片请使用「导出副本」保存".into());
    let Some(format) = image_format else { return Err(unsupported()); };
    if ImageFormat::from_path(path).as_ref() != Some(&format) {
        return Err(unsupported());
    }
    let decode_error = |error: image::ImageError| EngineError::decode(path, error.to_string());
    let multiple = match format {
        ImageFormat::Png => image::codecs::png::PngDecoder::new(open()?)
            .and_then(|decoder| decoder.is_apng())
            .map_err(decode_error)?,
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(open()?)
            .map_err(decode_error)?
            .has_animation(),
        ImageFormat::Tiff => tiff::decoder::Decoder::new(open()?)
            .map_err(|error| EngineError::decode(path, error.to_string()))?
            .more_images(),
        _ => false,
    };
    if multiple {
        return Err(unsupported());
    }
    export_format_of_core(&format).ok_or_else(unsupported)
}

fn is_readable_candidate(format: &ImageFormat) -> bool {
    READABLE_CANDIDATES.contains(format)
}

fn unsupported_format_error(path: &Path, format: &ImageFormat) -> EngineError {
    EngineError::Unsupported(format!("{}（{}）", format.label(), path.display()))
}

fn decode_error(path: &Path, error: impl std::fmt::Display) -> EngineError {
    EngineError::decode(path, error.to_string())
}

/// Read only the header: format + dimensions. Never allocates the pixel buffer.
pub(crate) fn read_header(path: &Path) -> Result<Header, EngineError> {
    let reader = ImageReader::open(path).map_err(|error| EngineError::io(path, error))?;
    let reader = reader
        .with_guessed_format()
        .map_err(|error| EngineError::io(path, error))?;
    let detected = reader.format().ok_or_else(|| {
        EngineError::Unsupported(format!("无法识别的图片格式（{}）", path.display()))
    })?;
    let format = map_format(detected);
    if !is_readable_candidate(&format) {
        return Err(unsupported_format_error(path, &format));
    }
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| decode_error(path, error))?;
    Ok(Header {
        format,
        width,
        height,
    })
}

/// Header-only probe used by [`gimage_core::ImageEngine::probe`].
pub(crate) fn probe(engine: &RustImageEngine, path: &Path) -> Result<ImageInfo, EngineError> {
    if let Some(format) = ImageFormat::from_path(path)
        && crate::extended::handles(&format)
    {
        return crate::extended::probe(engine, path, format);
    }
    let header = read_header(path)?;
    engine.guard_pixels(path, header.width, header.height)?;
    Ok(ImageInfo {
        width: header.width,
        height: header.height,
        orientation: read_orientation(path),
        has_alpha: guess_has_alpha(path, &header.format),
        format: header.format,
        // Cheap and honest: this backend never returns more than the first
        // frame, and reliably counting frames would require a full parse.
        animated: false,
        frames: 1,
    })
}

/// Decoded pixels plus the metadata needed by the pipeline.
pub(crate) struct Decoded {
    pub(crate) image: RgbaImage,
    pub(crate) orientation: u16,
}

/// Decode a file once, enforcing `max_decode_pixels` from the header *before*
/// the decoder allocates the output buffer.
pub(crate) fn decode_for_view(
    engine: &RustImageEngine, path: &Path, edge: Option<u32>, thumbnail: bool,
) -> Result<Decoded, EngineError> {
    if let Some(format) = ImageFormat::from_path(path) {
        if crate::extended::handles(&format) {
            return crate::extended::decode(engine, path, format, edge, thumbnail);
        }
        #[cfg(windows)]
        if format == ImageFormat::Jpeg && edge.is_some()
            && let Ok(decoded) = crate::extended::windows::scaled_jpeg(engine, path, edge)
        {
            return Ok(decoded);
        }
    }
    let reader = ImageReader::open(path).map_err(|error| EngineError::io(path, error))?;
    let mut reader = reader
        .with_guessed_format()
        .map_err(|error| EngineError::io(path, error))?;
    let detected = reader.format().ok_or_else(|| {
        EngineError::Unsupported(format!("无法识别的图片格式（{}）", path.display()))
    })?;
    let format = map_format(detected);
    if !is_readable_candidate(&format) {
        return Err(unsupported_format_error(path, &format));
    }

    // Give the decoder a generous but bounded allocation budget derived from
    // the user's pixel limit. The authoritative check happens on the
    // dimensions reported by the decoder, before any pixel buffer is created.
    let max_alloc = engine
        .max_decode_pixels()
        .saturating_mul(8)
        .saturating_add(64 * 1024 * 1024);
    let mut limits = Limits::default();
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits.clone());

    let mut decoder = reader
        .into_decoder()
        .map_err(|error| decode_error(path, error))?;
    let (width, height) = decoder.dimensions();
    engine.guard_pixels(path, width, height)?;
    // `into_decoder` applies the limits for most formats already; this also
    // covers the PNG decoder path and future decoders.
    decoder
        .set_limits(limits)
        .map_err(|error| decode_error(path, error))?;

    let dynamic = DynamicImage::from_decoder(decoder).map_err(|error| decode_error(path, error))?;
    let rgba = dynamic.into_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    if width == 0 || height == 0 {
        return Err(decode_error(path, "图片尺寸为空"));
    }
    let image = RgbaImage::new(width, height, rgba.into_raw());
    Ok(Decoded {
        image,
        orientation: read_orientation(path),
    })
}

/// Read EXIF orientation (tag 0x0112) from the primary IFD. Anything missing,
/// malformed or out of range falls back to 1 (upright).
pub(crate) fn read_orientation(path: &Path) -> u16 {
    let Ok(file) = File::open(path) else {
        return 1;
    };
    let mut reader = BufReader::new(file);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut reader) else {
        return 1;
    };
    exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|field| field.value.get_uint(0))
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| (1..=8).contains(value))
        .unwrap_or(1)
}

/// Best-effort alpha guess based on cheap header bytes.
///
/// This is intentionally conservative and documented as a guess:
/// * PNG: colour type 4/6, or colour type 3 with a `tRNS` chunk in the first
///   4 KiB.
/// * WebP: `VP8X` alpha flag or `VP8L` alpha bit.
/// * GIF: a Graphics Control Extension with the transparency flag set in the
///   first 4 KiB.
/// * BMP: 32-bit pixels.
/// * ICO: 32-bit entry.
/// * JPEG and TIFF always report `false` (JPEG has no alpha; TIFF alpha would
///   require parsing IFD tags we do not otherwise need).
///
/// `false` is always safe: the engine still returns RGBA pixels.
pub(crate) fn guess_has_alpha(path: &Path, format: &ImageFormat) -> bool {
    let mut buffer = [0u8; 4096];
    let read = File::open(path)
        .and_then(|mut file| file.read(&mut buffer))
        .unwrap_or(0);
    let bytes = &buffer[..read];
    match format {
        ImageFormat::Png => match bytes.get(25).copied() {
            Some(4 | 6) => true,
            Some(3) => contains(bytes, b"tRNS"),
            _ => false,
        },
        ImageFormat::WebP => {
            let chunk = bytes.get(12..16);
            match chunk {
                Some(b"VP8X") => bytes.get(20).is_some_and(|flags| flags & 0x10 != 0),
                // VP8L: 0x2f signature at 20, alpha bit 28 of the header word.
                Some(b"VP8L") => bytes.get(24).is_some_and(|flags| flags & 0x10 != 0),
                _ => false,
            }
        }
        ImageFormat::Gif => gif_uses_transparency(bytes),
        ImageFormat::Bmp => bytes.len() >= 30 && u16::from_le_bytes([bytes[28], bytes[29]]) >= 32,
        ImageFormat::Ico => bytes.len() >= 14 && u16::from_le_bytes([bytes[12], bytes[13]]) == 32,
        _ => false,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn gif_uses_transparency(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index + 8 <= bytes.len() {
        if bytes[index] == 0x21 && bytes[index + 1] == 0xF9 && bytes[index + 2] == 0x04 {
            if bytes[index + 3] & 0x01 != 0 {
                return true;
            }
            index += 8;
        } else {
            index += 1;
        }
    }
    false
}

/// Encode `image` to `path`, overwriting it. Callers that need atomicity pass a
/// sibling temp path and rename afterwards (see [`crate::write_atomic`]).
pub(crate) fn encode(
    image: &RgbaImage,
    format: ExportFormat,
    quality: u8,
    path: &Path,
) -> Result<(), EngineError> {
    if image.is_empty() {
        return Err(EngineError::encode(path, "图像尺寸为空"));
    }
    let expected = u64::from(image.width) * u64::from(image.height) * 4;
    if expected != image.pixels.len() as u64 {
        return Err(EngineError::encode(path, "像素缓冲区长度与图像尺寸不匹配"));
    }

    let file = File::create(path).map_err(|error| EngineError::io(path, error))?;
    let mut writer = BufWriter::new(file);
    let result = match format {
        ExportFormat::Jpeg => encode_jpeg(image, quality, &mut writer),
        ExportFormat::Png => PngEncoder::new(&mut writer).write_image(
            &image.pixels,
            image.width,
            image.height,
            ExtendedColorType::Rgba8,
        ),
        ExportFormat::WebP => WebPEncoder::new_lossless(&mut writer).write_image(
            &image.pixels,
            image.width,
            image.height,
            ExtendedColorType::Rgba8,
        ),
        ExportFormat::Bmp => BmpEncoder::new(&mut writer).encode(
            &image.pixels,
            image.width,
            image.height,
            ExtendedColorType::Rgba8,
        ),
        ExportFormat::Tiff => TiffEncoder::new(&mut writer).write_image(
            &image.pixels,
            image.width,
            image.height,
            ExtendedColorType::Rgba8,
        ),
    };
    result.map_err(|error| EngineError::encode(path, error.to_string()))?;
    writer
        .flush()
        .map_err(|error| EngineError::io(path, error))?;
    Ok(())
}

fn encode_jpeg(image: &RgbaImage, quality: u8, writer: &mut impl Write) -> image::ImageResult<()> {
    let pixels = composite_on_white(image);
    JpegEncoder::new_with_quality(writer, quality.clamp(1, 100)).write_image(
        &pixels,
        image.width,
        image.height,
        ExtendedColorType::Rgb8,
    )
}

/// Flatten straight alpha onto opaque white, rounding to nearest.
fn composite_on_white(image: &RgbaImage) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(
        (image.width as usize)
            .saturating_mul(image.height as usize)
            .saturating_mul(3),
    );
    let (pixels, _) = image.pixels.as_chunks::<4>();
    for pixel in pixels {
        let alpha = u32::from(pixel[3]);
        let inverse = 255 - alpha;
        for &channel in &pixel[..3] {
            let value = u32::from(channel) * alpha + 255 * inverse;
            rgb.push(((value + 127) / 255) as u8);
        }
    }
    rgb
}

// ---------------------------------------------------------------------------
// Runtime capability probing
// ---------------------------------------------------------------------------

fn build_capabilities() -> CodecCapabilities {
    let probe_image = RgbaImage::new(
        2,
        2,
        vec![
            255, 0, 0, 255, // red
            0, 255, 0, 255, // green
            0, 0, 255, 255, // blue
            255, 255, 0, 128, // translucent yellow
        ],
    );

    let mut readable = Vec::new();
    for format in READABLE_CANDIDATES {
        if probe_roundtrip(&probe_image, format) {
            readable.push(format.clone());
        }
    }
    readable.extend([ImageFormat::Svg, ImageFormat::Psd]);
    #[cfg(windows)]
    {
        readable.push(ImageFormat::Ai);
        readable.extend(crate::extended::windows::available_formats());
    }

    let writable = WRITABLE_CANDIDATES
        .iter()
        .copied()
        .map(core_format_of_export)
        .filter(|format| readable.contains(format))
        .collect();

    let single_frame_only = SINGLE_FRAME_CANDIDATES
        .iter()
        .filter(|format| readable.contains(format))
        .cloned()
        .collect();

    CodecCapabilities {
        engine_name: "pure-rust".to_string(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        readable,
        writable,
        single_frame_only,
    }
}

/// Encode a 2x2 image to a real temporary file and decode it again.
fn probe_roundtrip(image: &RgbaImage, format: &ImageFormat) -> bool {
    let Some(detected) = image_format_of(format) else {
        return false;
    };
    let extension = detected.extensions_str().first().copied().unwrap_or("tmp");
    let Ok(temp) = ProbeTemp::new(extension) else {
        return false;
    };

    let encoded = match export_format_of_core(format) {
        Some(export) => encode(image, export, 90, temp.path()).is_ok(),
        // GIF/ICO are readable but not part of the public writable set; use
        // the codec directly so we can still prove the decoder works.
        None => encode_probe_only(image, format, temp.path()).is_ok(),
    };
    if !encoded {
        return false;
    }

    let Ok(file) = File::open(temp.path()) else {
        return false;
    };
    let Ok(reader) = ImageReader::new(BufReader::new(file)).with_guessed_format() else {
        return false;
    };
    if reader.format() != Some(detected) {
        return false;
    }
    match reader.decode() {
        Ok(decoded) => decoded.width() == 2 && decoded.height() == 2,
        Err(_) => false,
    }
}

/// Probe-only encoder for readable-but-not-writable formats.
fn encode_probe_only(
    image: &RgbaImage,
    format: &ImageFormat,
    path: &Path,
) -> Result<(), EngineError> {
    let file = File::create(path).map_err(|error| EngineError::io(path, error))?;
    let mut writer = BufWriter::new(file);
    let result = match format {
        ImageFormat::Gif => GifEncoder::new(&mut writer)
            .encode(
                &image.pixels,
                image.width,
                image.height,
                ExtendedColorType::Rgba8,
            )
            .map_err(|error| EngineError::encode(path, error.to_string())),
        ImageFormat::Ico => {
            let frame = IcoFrame::as_png(
                &image.pixels,
                image.width,
                image.height,
                ExtendedColorType::Rgba8,
            )
            .map_err(|error| EngineError::encode(path, error.to_string()))?;
            IcoEncoder::new(&mut writer)
                .encode_images(&[frame])
                .map_err(|error| EngineError::encode(path, error.to_string()))
        }
        other => Err(EngineError::Unsupported(other.label().to_string())),
    };
    result?;
    writer.flush().map_err(|error| EngineError::io(path, error))
}

/// Temporary file that removes itself on drop, used only for capability probing.
struct ProbeTemp {
    path: PathBuf,
}

impl ProbeTemp {
    fn new(extension: &str) -> io::Result<Self> {
        let directory = std::env::temp_dir();
        for attempt in 0..64u32 {
            let path = directory.join(probe_file_name(extension, attempt));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "无法创建编解码探测临时文件",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// Unique-ish probe name without any mutable global counter. Collisions are
/// resolved by `create_new` in the caller's retry loop.
fn probe_file_name(extension: &str, attempt: u32) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "gimage-codec-probe-{}-{nanos}-{attempt}.{extension}",
        std::process::id()
    )
}

impl Drop for ProbeTemp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
