//! Shared domain model for GImage.
//!
//! No crate in the workspace may leak backend-specific types (libvips, `image`,
//! `rusqlite`, Slint) through these APIs. Everything the UI sees is defined here.

pub mod cancel;
pub mod annotations;
pub mod edits;
pub mod errors;
pub mod export;
pub mod paths;
pub mod rename;
pub mod settings;
pub mod traits;
pub mod types;

pub use cancel::CancellationToken;
pub use annotations::{Annotation, AnnotationKind, MarkPoint, MarkTool, StrokeStyle};
pub use edits::{CropRect, EditParams, EditSession, ResizeSpec, Rotation, EDIT_PARAMS_VERSION};
pub use errors::{EngineError, SettingsError};
pub use export::{ExportFormat, ExportOptions, MetadataPolicy, OutputPolicy, ResizeMode};
pub use rename::{build_rename_target, RenameOptions, RenameRule};
pub use settings::{Settings, Theme};
pub use traits::{EngineOptions, ImageEngine};
pub use types::{
    CodecCapabilities, ImageFilter, ImageFormat, ImageInfo, ImageMeta, RgbaImage,
    SCANNABLE_EXTENSIONS, SortDirection, SortKey, SortSpec,
};
