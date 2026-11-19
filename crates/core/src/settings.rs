use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::SettingsError;
use crate::types::{SortDirection, SortKey, SortSpec};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "深色",
            Self::Light => "浅色",
        }
    }
}

fn default_thumbnail_px() -> u32 {
    320
}
fn default_memory_mb() -> u32 {
    24
}
fn default_disk_mb() -> u32 {
    128
}
fn default_concurrency() -> u32 {
    2
}
fn default_max_decode() -> u64 {
    180_000_000
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    /// Maximum long edge of generated thumbnails in pixels: 160 / 320 / 640.
    pub thumbnail_px: u32,
    /// Soft cap for the in-memory thumbnail LRU.
    pub memory_cache_mb: u32,
    /// Soft cap for the on-disk thumbnail cache.
    pub disk_cache_mb: u32,
    /// Number of heavy image jobs running at once.
    pub concurrency: u32,
    pub recursive_scan: bool,
    pub scan_depth: u8,
    pub include_hidden: bool,
    /// Disables non-essential animations for slow machines.
    pub low_power: bool,
    /// Refuse to decode images above this pixel count (decompression bombs).
    pub max_decode_pixels: u64,
    pub sort: SortSpec,
    pub last_folder: Option<PathBuf>,
    pub hidden_recent_folders: Vec<PathBuf>,
    pub file_menu_positions: [u8; 3],
    pub explorer_menu_enabled: bool,
    pub favorite_formats: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            thumbnail_px: default_thumbnail_px(),
            memory_cache_mb: default_memory_mb(),
            disk_cache_mb: default_disk_mb(),
            concurrency: default_concurrency(),
            recursive_scan: true,
            scan_depth: 5,
            include_hidden: false,
            low_power: false,
            max_decode_pixels: default_max_decode(),
            sort: SortSpec { key: SortKey::Name, direction: SortDirection::Ascending },
            last_folder: None,
            hidden_recent_folders: Vec::new(),
            file_menu_positions: [0, 0, 1],
            explorer_menu_enabled: false,
            favorite_formats: Vec::new(),
        }
    }
}

impl Settings {
    pub const THUMBNAIL_SIZES: [u32; 3] = [160, 320, 640];

    pub fn normalized(&self) -> Self {
        let mut out = self.clone();
        if !Self::THUMBNAIL_SIZES.contains(&out.thumbnail_px) {
            out.thumbnail_px = default_thumbnail_px();
        }
        out.memory_cache_mb = out.memory_cache_mb.clamp(16, 512);
        out.disk_cache_mb = out.disk_cache_mb.clamp(32, 10_240);
        out.concurrency = out.concurrency.clamp(1, 8);
        out.max_decode_pixels = out.max_decode_pixels.clamp(1_000_000, 2_000_000_000);
        out.scan_depth = out.scan_depth.clamp(1, 5);
        out.file_menu_positions = out.file_menu_positions.map(|position| position.min(2));
        out
    }

    pub fn effective_scan_depth(&self) -> u8 {
        if self.recursive_scan { self.scan_depth.clamp(1, 5) } else { 0 }
    }

    /// Missing file yields defaults; a corrupt file yields defaults plus a
    /// warning instead of blocking startup.
    pub fn load_or_default(path: &Path) -> (Self, Option<SettingsError>) {
        match fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(settings) => (settings.normalized(), None),
                Err(err) => (Self::default(), Some(SettingsError::Invalid(err.to_string()))),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(err) => (Self::default(), Some(SettingsError::Read { path: path.to_path_buf(), source: err })),
        }
    }

    /// Atomic save: write `settings.json.tmp`, then rename over the target.
    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|source| SettingsError::Write { path: path.to_path_buf(), source })?;
        let json = serde_json::to_string_pretty(&self.normalized())
            .map_err(|err| SettingsError::Invalid(err.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        {
            let mut file = fs::File::create(&tmp)
                .map_err(|source| SettingsError::Write { path: tmp.clone(), source })?;
            file.write_all(json.as_bytes())
                .map_err(|source| SettingsError::Write { path: tmp.clone(), source })?;
            file.sync_all()
                .map_err(|source| SettingsError::Write { path: tmp.clone(), source })?;
        }
        fs::rename(&tmp, path)
            .map_err(|source| SettingsError::Write { path: path.to_path_buf(), source })
    }

    pub fn thumbnail_memory_bytes(&self) -> usize {
        self.memory_cache_mb as usize * 1024 * 1024
    }

    pub fn disk_cache_bytes(&self) -> u64 {
        self.disk_cache_mb as u64 * 1024 * 1024
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_clamps_extremes() {
        let settings = Settings { thumbnail_px: 999, concurrency: 99, memory_cache_mb: 1, ..Default::default() }
            .normalized();
        assert_eq!(settings.thumbnail_px, 320);
        assert_eq!(settings.concurrency, 8);
        assert_eq!(settings.memory_cache_mb, 16);
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = std::env::temp_dir().join("gimage-settings-test");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let settings = Settings { theme: Theme::Light, thumbnail_px: 160, ..Default::default() };
        settings.save(&path).unwrap();
        let (loaded, err) = Settings::load_or_default(&path);
        assert!(err.is_none());
        assert_eq!(loaded, settings.normalized());
        let _ = fs::remove_file(&path);
    }
}
