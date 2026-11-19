use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Formats the scanner recognises by extension. Actual decodability is probed by
/// the image engine at startup (`CodecCapabilities`); this list is only a cheap
/// pre-filter so that scanning a folder does not touch every file.
pub const SCANNABLE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "apng", "webp", "gif", "bmp", "tif", "tiff", "avif", "heic",
    "heif", "heics", "heifs", "hif", "svg", "svgz", "ico", "psd", "psb", "ai",
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2",
    "pef", "srw", "raw", "rwl", "erf", "3fr", "fff", "iiq", "mrw", "x3f",
];

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageFormat {
    Jpeg,
    Png,
    WebP,
    Gif,
    Bmp,
    Tiff,
    Avif,
    Heic,
    Svg,
    Ico,
    Raw,
    Psd,
    Ai,
    Other(String),
}

impl ImageFormat {
    pub fn from_extension(ext: &str) -> Option<Self> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        Some(match ext.as_str() {
            "jpg" | "jpeg" | "jpe" | "jfif" => Self::Jpeg,
            "png" | "apng" => Self::Png,
            "webp" => Self::WebP,
            "gif" => Self::Gif,
            "bmp" => Self::Bmp,
            "tif" | "tiff" => Self::Tiff,
            "avif" => Self::Avif,
            "heic" | "heif" | "heics" | "heifs" | "hif" => Self::Heic,
            "svg" | "svgz" => Self::Svg,
            "ico" => Self::Ico,
            "dng" | "cr2" | "cr3" | "crw" | "nef" | "nrw" | "arw" | "srf" | "sr2" | "raf" | "orf"
            | "rw2" | "pef" | "srw" | "raw" | "rwl" | "erf" | "3fr" | "fff" | "iiq" | "mrw" | "x3f" => Self::Raw,
            "psd" | "psb" => Self::Psd,
            "ai" => Self::Ai,
            _ => return None,
        })
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(Self::from_extension)
    }

    /// Stable identifier used in the SQLite index and in cache keys.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::WebP => "webp",
            Self::Gif => "gif",
            Self::Bmp => "bmp",
            Self::Tiff => "tiff",
            Self::Avif => "avif",
            Self::Heic => "heic",
            Self::Svg => "svg",
            Self::Ico => "ico",
            Self::Raw => "raw",
            Self::Psd => "psd",
            Self::Ai => "ai",
            Self::Other(other) => other,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::WebP => "WebP",
            Self::Gif => "GIF",
            Self::Bmp => "BMP",
            Self::Tiff => "TIFF",
            Self::Avif => "AVIF",
            Self::Heic => "HEIC / HEIF",
            Self::Svg => "SVG",
            Self::Ico => "ICO",
            Self::Raw => "RAW",
            Self::Psd => "PSD / PSB",
            Self::Ai => "AI",
            Self::Other(other) => other,
        }
    }

    pub fn is_animated_capable(&self) -> bool {
        matches!(self, Self::Gif | Self::WebP | Self::Png | Self::Avif)
    }
}

impl fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SortKey {
    #[default]
    Name,
    Modified,
    Size,
    Format,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "名称",
            Self::Modified => "修改日期",
            Self::Size => "大小",
            Self::Format => "格式",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

impl SortDirection {
    pub fn toggled(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortSpec {
    pub key: SortKey,
    pub direction: SortDirection,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self { key: SortKey::Name, direction: SortDirection::Ascending }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageFilter {
    /// Case-insensitive substring match on the file name.
    pub query: String,
    pub formats: Vec<ImageFormat>,
    pub favorites_only: bool,
    pub include_missing: bool,
    pub max_directory_depth: Option<u8>,
}

impl ImageFilter {
    pub fn is_empty(&self) -> bool {
        self.query.trim().is_empty()
            && self.formats.is_empty()
            && !self.favorites_only
            && !self.include_missing
    }
}

/// One indexed image, as displayed by the browser and preview pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageMeta {
    pub id: i64,
    /// Absolute, canonicalised path. Display text is derived only at the UI edge.
    pub path: std::path::PathBuf,
    pub dir: std::path::PathBuf,
    pub file_name: String,
    pub size_bytes: u64,
    pub modified_ns: i64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub format: ImageFormat,
    /// EXIF orientation (1..=8), as read during the last probe.
    pub orientation: u16,
    pub favorite: bool,
    pub missing: bool,
}

impl ImageMeta {
    pub fn dimension_text(&self) -> Option<String> {
        match (self.width, self.height) {
            (Some(w), Some(h)) => Some(format!("{w} × {h}")),
            _ => None,
        }
    }

    pub fn aspect_ratio(&self) -> Option<f32> {
        match (self.width, self.height) {
            (Some(w), Some(h)) if h > 0 => Some(w as f32 / h as f32),
            _ => None,
        }
    }
}

/// What the active engine can really decode/encode, probed at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecCapabilities {
    pub engine_name: String,
    pub engine_version: String,
    pub readable: Vec<ImageFormat>,
    pub writable: Vec<ImageFormat>,
    /// Formats that are readable but only as a single frame (animation dropped).
    pub single_frame_only: Vec<ImageFormat>,
}

impl CodecCapabilities {
    pub fn can_read(&self, format: &ImageFormat) -> bool {
        self.readable.contains(format)
    }

    pub fn can_write(&self, format: &ImageFormat) -> bool {
        self.writable.contains(format)
    }
}

/// Header-level probe result. Cheap: never decodes the full pixel buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    /// EXIF orientation before applying it to pixels (1..=8, 1 = normal).
    pub orientation: u16,
    pub has_alpha: bool,
    pub animated: bool,
    pub frames: u32,
}

impl ImageInfo {
    /// Dimensions after EXIF orientation is normalised into pixels.
    pub fn oriented_size(&self) -> (u32, u32) {
        if (5..=8).contains(&self.orientation) {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }
}

/// Backend-neutral 8-bit RGBA image.
#[derive(Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, row-major, straight (non-premultiplied) alpha.
    pub pixels: Vec<u8>,
}

impl RgbaImage {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        debug_assert_eq!(pixels.len() as u64, width as u64 * height as u64 * 4);
        Self { width, height, pixels }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixel_count(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn byte_len(&self) -> usize {
        self.pixels.len()
    }
}

impl fmt::Debug for RgbaImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RgbaImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.pixels.len())
            .finish()
    }
}
