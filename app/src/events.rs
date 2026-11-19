use std::path::PathBuf;

use gimage_core::{ImageInfo, ImageMeta, RgbaImage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderKind {
    Preview,
    Editor,
    Conversion,
}

/// A full task-row snapshot; the UI maps it 1:1 onto the Slint model.
#[derive(Clone, Debug)]
pub struct TaskSnapshot {
    pub id: u64,
    pub title: String,
    pub detail: String,
    pub status: String,
    pub progress: f32,
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    /// 0 queued, 1 running, 2 done, 3 partial, 4 failed, 5 canceled, 6 interrupted
    pub state: i32,
    pub cancelable: bool,
    pub retryable: bool,
}

#[derive(Clone, Debug)]
pub struct ScanReport {
    pub max_image_depth: usize,
    pub folder_id: i64,
    pub path: PathBuf,
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
    pub canceled: bool,
    pub truncated: bool,
    pub errors: u64,
    pub total: u64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug)]
pub struct FolderSummary {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub count: i64,
}

#[derive(Debug)]
pub enum AppEvent {
    UpdateChecked(Result<Option<crate::updater::Update>, String>),
    UpdateDownloaded(Result<crate::updater::StagedUpdate, String>),
    DroppedBrowser {
        request: u64, generation: u64, page: i32, preview: bool,
        result: Result<(crate::core::FolderSession, Vec<ImageMeta>), String>,
    },
    DroppedConversion { token: u64, items: Result<Vec<crate::core::TaskItem>, String> },
    BrowserAllSelected { generation: u64, request: u64, ids: Result<Vec<i64>, String> },
    RenameLoaded { token: u64, entries: Result<Vec<crate::ui::rename_page::RenameEntry>, String> },
    RenamePreview { token: u64, revision: u64, order: Vec<usize>, targets: Result<Vec<Option<String>>, String> },
    SettingsStats { cache: String, catalog: String, engine: String },
    ConversionLoaded {
        token: u64,
        items: Result<Vec<crate::core::TaskItem>, String>,
    },
    CopyImage(Result<RgbaImage, String>),
    EditSaved { id: i64, result: Result<(), String> },
    ExifReady { id: i64, text: String },
    ScanProgress {
        generation: u64,
        scanned: u64,
        found: u64,
        current: String,
    },
    ScanFinished {
        generation: u64,
        report: Result<ScanReport, String>,
    },
    PageReady {
        generation: u64,
        offset: u32,
        total: u64,
        items: Vec<ImageMeta>,
    },
    PageFailed {
        generation: u64,
        error: String,
    },
    ThumbReady {
        generation: u64,
        cache_epoch: u64,
        id: i64,
        px: u32,
        image: std::sync::Arc<RgbaImage>,
    },
    ThumbFailed {
        generation: u64,
        cache_epoch: u64,
        id: i64,
        px: u32,
        error: String,
    },
    ProbeReady {
        id: i64,
        info: Result<ImageInfo, String>,
    },
    RenderReady {
        seq: u64,
        kind: RenderKind,
        id: i64,
        image: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    },
    RenderFailed {
        seq: u64,
        kind: RenderKind,
        id: i64,
        error: String,
    },
    Task(TaskSnapshot),
    Toast {
        message: String,
        error: bool,
    },
    LibraryUpdated {
        folders: Vec<FolderSummary>,
        stats: String,
    },
    CacheStats(String),
    /// The catalog is stale (e.g. files were renamed) and the current folder
    /// should be rescanned.
    RefreshRequested,
}
