use std::path::Path;

use crate::edits::EditParams;
use crate::errors::EngineError;
use crate::export::ExportFormat;
use crate::types::{CodecCapabilities, ImageInfo, RgbaImage};

/// Limits applied by every engine implementation.
#[derive(Clone, Copy, Debug)]
pub struct EngineOptions {
    /// Refuse images whose decoded pixel count exceeds this (OOM/bomb guard).
    pub max_decode_pixels: u64,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self { max_decode_pixels: 180_000_000 }
    }
}

/// The only image-processing surface the rest of the app may use.
///
/// Implementations must be thread-safe: the thumbnail pool and batch workers
/// call `render`/`thumbnail`/`encode` concurrently.
pub trait ImageEngine: Send + Sync {
    /// Short engine id, e.g. "pure-rust".
    fn name(&self) -> &str;
    fn version(&self) -> &str;

    fn capabilities(&self) -> CodecCapabilities;

    /// Read only the header/metadata. Never allocates the full pixel buffer.
    fn probe(&self, path: &Path) -> Result<ImageInfo, EngineError>;

    /// Decode, apply EXIF orientation, and downscale to fit `max_edge`.
    /// Used for the grid and filmstrip.
    fn thumbnail(&self, path: &Path, max_edge: u32) -> Result<RgbaImage, EngineError>;

    /// Full pipeline used by the editor preview and by export.
    /// `max_edge` limits the output resolution (preview quality); `None` means
    /// full resolution. Crop/rotate arguments are in normalised coordinates
    /// relative to the already orientation-normalised image.
    fn render(
        &self,
        path: &Path,
        params: &EditParams,
        max_edge: Option<u32>,
    ) -> Result<RgbaImage, EngineError>;

    /// Encode `image` to `path` (caller is responsible for temp-file + rename).
    fn encode(
        &self,
        image: &RgbaImage,
        format: ExportFormat,
        quality: u8,
        path: &Path,
    ) -> Result<(), EngineError>;
}