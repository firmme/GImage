//! Image engine: the only crate allowed to decode/encode pixels.
//!
//! Backend choice: pure-Rust `image` codecs (see docs/adr/0002-image-engine.md).
//! The public surface is backend-neutral, so a libvips adapter can be added
//! later behind the same [`gimage_core::ImageEngine`] trait.
//!
//! # Pipeline
//!
//! Every render follows the order required by the product spec:
//!
//! `source -> EXIF orientation -> crop -> rotate -> flip -> tone -> resize -> encode`
//!
//! Crop rectangles are normalised to the already oriented image, rotations are
//! multiples of 90 degrees, and resizing never upscales. The exact tone math is
//! documented in the `pipeline` module and implemented by a single function so
//! a future GPU preview can match it.
//!
//! # Safety and atomicity
//!
//! * Source files are only ever opened for reading; [`export_image`] rejects a
//!   destination that canonicalises to the source.
//! * Export writes a sibling temp file, flushes/syncs it, then renames it into
//!   place ([`write_atomic`]). Temp files are removed on failure.
//! * Decoding is guarded by [`gimage_core::EngineOptions::max_decode_pixels`];
//!   the header is probed before the pixel buffer is allocated.
//! * Malformed/truncated files return [`gimage_core::EngineError`] instead of
//!   panicking.
//!
//! # Codec notes
//!
//! Capabilities are probed at runtime by encoding a 2x2 image and decoding it
//! again. JPEG honours `quality`; PNG/BMP/TIFF are lossless and ignore it;
//! `image` 0.25 only offers a **lossless** WebP encoder, so WebP output ignores
//! `quality` as well. Animated GIF/animated WebP/APNG and multi-size ICO files
//! are decoded as their first frame only. See the `codec` module for details.

mod codec;
mod export;
mod pipeline;
mod annotations;
mod extended;

use std::path::{Path, PathBuf};

use gimage_core::{
    CodecCapabilities, EditParams, EngineError, EngineOptions, ExportFormat, ExportOptions,
    ImageEngine, ImageInfo, RgbaImage,
};

pub use export::{export_image, resolve_destination, transform_export, unique_destination, write_atomic, write_atomic_checked};
pub use pipeline::{apply_orientation, rotate_image};
pub use annotations::text_available as annotation_text_available;
pub use annotations::draw_from as draw_annotations_from;
pub use codec::replacement_format;

/// Pure-Rust implementation of [`gimage_core::ImageEngine`].
///
/// The struct holds only immutable configuration, so it is `Send + Sync` and
/// can be shared by the thumbnail pool and batch workers. The only global
/// state is the process-wide capabilities cache.
pub struct RustImageEngine {
    options: EngineOptions,
}

impl RustImageEngine {
    pub fn new(options: EngineOptions) -> Self {
        Self { options }
    }

    pub fn options(&self) -> EngineOptions {
        self.options
    }

    pub(crate) fn max_decode_pixels(&self) -> u64 {
        self.options.max_decode_pixels
    }

    /// Reject images whose decoded pixel count exceeds the configured limit.
    /// Uses `u64` math so huge header dimensions cannot overflow.
    pub(crate) fn guard_pixels(
        &self,
        path: &Path,
        width: u32,
        height: u32,
    ) -> Result<(), EngineError> {
        let pixels = u64::from(width) * u64::from(height);
        let limit = self.options.max_decode_pixels;
        if pixels > limit {
            tracing::debug!(
                path = %path.display(),
                width,
                height,
                pixels,
                limit,
                "拒绝解码过大的图片"
            );
            return Err(EngineError::TooLarge {
                width,
                height,
                pixels,
                limit,
            });
        }
        Ok(())
    }
}

impl ImageEngine for RustImageEngine {
    fn name(&self) -> &str {
        "pure-rust"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn capabilities(&self) -> CodecCapabilities {
        codec::capabilities()
    }

    fn probe(&self, path: &Path) -> Result<ImageInfo, EngineError> {
        codec::probe(self, path)
    }

    fn thumbnail(&self, path: &Path, max_edge: u32) -> Result<RgbaImage, EngineError> {
        pipeline::thumbnail(self, path, max_edge)
    }

    fn render(
        &self,
        path: &Path,
        params: &EditParams,
        max_edge: Option<u32>,
    ) -> Result<RgbaImage, EngineError> {
        pipeline::render(self, path, params, max_edge)
    }

    fn encode(
        &self,
        image: &RgbaImage,
        format: ExportFormat,
        quality: u8,
        path: &Path,
    ) -> Result<(), EngineError> {
        codec::encode(image, format, quality, path)
    }
}

/// Everything needed to export one image.
#[derive(Clone, Debug)]
pub struct ExportRequest {
    pub source: PathBuf,
    /// Requested output path; the final path may differ when
    /// `options.output == AutoRename`.
    pub destination: PathBuf,
    pub params: EditParams,
    pub options: ExportOptions,
}

#[derive(Clone, Debug)]
pub struct ExportOutcome {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub enum ExportStatus {
    Written(ExportOutcome),
    /// Destination exists and the policy is `SkipExisting`.
    Skipped,
}

#[cfg(test)]
mod tests;
