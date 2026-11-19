//! Folder scanner + SQLite index.
//!
//! The scanner only reads file metadata (never decodes pixels). Dimensions and
//! EXIF orientation are filled in lazily by the app via
//! [`Catalog::update_probe`] once an image becomes visible.

pub mod scan;
pub mod store;

pub use scan::{ScanOptions, ScanOutput, ScanProgress, ScannedFile, scan_folder};
pub use store::{Catalog, DbStats, FolderRecord, QueryPage, SyncSummary};

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("数据库错误：{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("文件系统错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("路径无效：{0}")]
    InvalidPath(String),
    #[error("数据库迁移失败：{0}")]
    Migration(String),
}
