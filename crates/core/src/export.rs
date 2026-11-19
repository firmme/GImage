use serde::{Deserialize, Serialize};

use crate::edits::{ResizeSpec, Rotation};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportFormat {
    Jpeg,
    Png,
    WebP,
    Bmp,
    Tiff,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::WebP => "webp",
            Self::Bmp => "bmp",
            Self::Tiff => "tif",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::WebP => "WebP",
            Self::Bmp => "BMP",
            Self::Tiff => "TIFF",
        }
    }

    pub fn supports_quality(self) -> bool {
        matches!(self, Self::Jpeg | Self::WebP)
    }

    pub fn supports_alpha(self) -> bool {
        matches!(self, Self::Png | Self::WebP | Self::Tiff)
    }

    pub fn all() -> &'static [ExportFormat] {
        &[Self::Jpeg, Self::Png, Self::WebP, Self::Bmp, Self::Tiff]
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::all().iter().copied().find(|f| f.label() == label)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetadataPolicy {
    /// Only what the encoder can carry over cheaply (orientation is normalised
    /// away, so the output is always upright).
    #[default]
    Strip,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputPolicy {
    /// Never overwrite: `name (2).ext`, `name (3).ext`, ...
    #[default]
    AutoRename,
    SkipExisting,
    /// Destructive; the UI must ask for confirmation before choosing this.
    Overwrite,
}

impl OutputPolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::AutoRename => "自动重命名，保留原件",
            Self::SkipExisting => "跳过已存在的文件",
            Self::Overwrite => "覆盖已存在的文件（需确认）",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// 1..=100, used by JPEG/WebP.
    pub quality: u8,
    pub resize: Option<ResizeSpec>,
    #[serde(default)]
    pub resize_mode: ResizeMode,
    pub metadata: MetadataPolicy,
    pub output: OutputPolicy,
    #[serde(default)]
    pub rotation: Rotation,
    #[serde(default)]
    pub directory: Option<PathBuf>,
    #[serde(default)]
    pub prefix: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResizeMode {
    Original,
    #[default]
    Limit,
    Contain,
    FitWidth,
    Pad,
    Crop,
    Stretch,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self { format: ExportFormat::Jpeg, quality: 90, resize: None, resize_mode: ResizeMode::Limit, metadata: MetadataPolicy::Strip, output: OutputPolicy::AutoRename, rotation: Rotation::None, directory: None, prefix: String::new() }
    }
}

impl ExportOptions {
    pub fn normalized(&self) -> Self {
        let mut out = self.clone();
        out.quality = out.quality.clamp(1, 100);
        if let Some(resize) = out.resize {
            out.resize = if resize.max_width == 0 || resize.max_height == 0 { None } else { Some(resize) };
        }
        out
    }
}
