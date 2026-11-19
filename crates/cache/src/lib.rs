//! Two-level thumbnail cache.
//!
//! Level 1: a byte-budgeted in-memory LRU of decoded RGBA thumbnails.
//! Level 2: one file per thumbnail on disk, written via temp-file + rename so a
//! crash can never leave a half-written entry visible.
//!
//! The cache key includes the file size and mtime, so editing or replacing a
//! source image invalidates its thumbnails without any explicit bookkeeping.

use std::collections::HashMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use gimage_core::RgbaImage;
use image::codecs::{jpeg::JpegEncoder, png::PngEncoder};
use image::{
    DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader,
};

/// Bump when the thumbnail pipeline changes, to invalidate all cached images.
pub const CACHE_VERSION: u32 = 2;

const CACHE_EXT: &str = "gthc";

fn file_limit(edge: u32) -> usize {
    match edge {
        0..=160 => 24 * 1024,
        161..=320 => 64 * 1024,
        _ => 160 * 1024,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThumbKey {
    pub path_hash: u64,
    pub file_size: u64,
    pub mtime_ns: i64,
    pub max_edge: u32,
    pub version: u32,
}

impl ThumbKey {
    pub fn for_path(path: &Path, file_size: u64, mtime_ns: i64, max_edge: u32) -> Self {
        Self {
            path_hash: hash_path(path),
            file_size,
            mtime_ns,
            max_edge,
            version: CACHE_VERSION,
        }
    }

    fn file_stem(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

pub fn hash_path(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone, Debug, Default)]
pub struct CacheStats {
    pub memory_bytes: usize,
    pub memory_items: usize,
    pub disk_bytes: u64,
    pub disk_files: usize,
    pub disk_hits: u64,
    pub memory_hits: u64,
    pub misses: u64,
}

struct MemEntry {
    image: Arc<RgbaImage>,
    bytes: usize,
    tick: u64,
}

struct MemCache {
    entries: HashMap<ThumbKey, MemEntry>,
    budget: usize,
    used: usize,
    tick: u64,
}

impl MemCache {
    fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            budget,
            used: 0,
            tick: 0,
        }
    }

    fn get(&mut self, key: &ThumbKey) -> Option<Arc<RgbaImage>> {
        self.tick += 1;
        let tick = self.tick;
        let entry = self.entries.get_mut(key)?;
        entry.tick = tick;
        Some(entry.image.clone())
    }

    fn put(&mut self, key: ThumbKey, image: Arc<RgbaImage>) {
        let bytes = image.byte_len();
        if bytes > self.budget {
            return;
        }
        self.tick += 1;
        if let Some(old) = self.entries.remove(&key) {
            self.used = self.used.saturating_sub(old.bytes);
        }
        while self.used + bytes > self.budget {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.tick)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(removed) = self.entries.remove(&victim) {
                self.used = self.used.saturating_sub(removed.bytes);
            }
        }
        self.entries.insert(
            key,
            MemEntry {
                image,
                bytes,
                tick: self.tick,
            },
        );
        self.used += bytes;
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.used = 0;
    }
}

pub struct ThumbnailCache {
    root: PathBuf,
    mem: Mutex<MemCache>,
    counters: Mutex<(u64, u64, u64)>, // memory hits, disk hits, misses
    disk: Mutex<DiskState>,
    epoch: AtomicU64,
}

struct DiskState {
    bytes: u64,
    budget: u64,
}

impl ThumbnailCache {
    /// Creates the cache directory and removes leftover temp files.
    pub fn new(root: PathBuf, memory_budget_bytes: usize) -> io::Result<Self> {
        fs::create_dir_all(&root)?;
        remove_stale_temp_files(&root);
        let bytes = disk_usage(&root)?.0;
        Ok(Self {
            root,
            mem: Mutex::new(MemCache::new(memory_budget_bytes)),
            counters: Mutex::new((0, 0, 0)),
            disk: Mutex::new(DiskState {
                bytes,
                budget: u64::MAX,
            }),
            epoch: AtomicU64::new(0),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn file_path(&self, key: &ThumbKey) -> PathBuf {
        self.root.join(format!("{}.{CACHE_EXT}", key.file_stem()))
    }

    /// Memory hit -> disk hit -> miss. Disk hits are promoted into memory.
    pub fn get(&self, key: &ThumbKey) -> Option<Arc<RgbaImage>> {
        let epoch = self.epoch();
        if let Some(hit) = self.mem.lock().unwrap().get(key) {
            self.counters.lock().unwrap().0 += 1;
            return Some(hit);
        }
        match self.read_disk(key) {
            Some(image) => {
                let image = Arc::new(image);
                self.put_memory(key, image.clone(), epoch);
                self.counters.lock().unwrap().1 += 1;
                Some(image)
            }
            None => {
                self.counters.lock().unwrap().2 += 1;
                None
            }
        }
    }

    /// Disk write is best-effort: a failure must not break browsing.
    pub fn put(&self, key: &ThumbKey, image: Arc<RgbaImage>) {
        let epoch = self.epoch();
        self.put_memory(key, image.clone(), epoch);
        self.persist(key, &image, epoch);
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn put_memory(&self, key: &ThumbKey, image: Arc<RgbaImage>, epoch: u64) {
        let mut mem = self.mem.lock().unwrap();
        if epoch == self.epoch() {
            mem.put(key.clone(), image);
        }
    }

    pub fn set_memory_budget(&self, budget: usize) {
        let mut mem = self.mem.lock().unwrap();
        if budget < mem.used {
            mem.clear();
        }
        mem.budget = budget;
    }

    pub fn persist(&self, key: &ThumbKey, image: &RgbaImage, epoch: u64) {
        if epoch != self.epoch() {
            return;
        }
        let encoded = match encode_thumbnail(image, file_limit(key.max_edge)) {
            Ok(Some(encoded)) => encoded,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(error = %err, "thumbnail cache encoding failed");
                return;
            }
        };
        let mut disk = self.disk.lock().unwrap();
        if epoch != self.epoch() || encoded.len() as u64 > disk.budget {
            return;
        }
        let path = self.file_path(key);
        let previous = path.metadata().map_or(0, |meta| meta.len());
        match write_disk(&path, &encoded) {
            Ok(()) => {
                disk.bytes = disk.bytes.saturating_sub(previous) + encoded.len() as u64;
                if disk.bytes > disk.budget {
                    let target = disk.budget - disk.budget / 10;
                    if let Err(err) = self.prune_locked(&mut disk, target) {
                        tracing::warn!(error = %err, "thumbnail cache pruning failed");
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "thumbnail disk cache write failed"),
        }
    }

    fn read_disk(&self, key: &ThumbKey) -> Option<RgbaImage> {
        let max_bytes = file_limit(key.max_edge);
        let mut bytes = Vec::new();
        fs::File::open(self.file_path(key))
            .ok()?
            .take((max_bytes + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > max_bytes {
            return None;
        }
        let mut reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .ok()?;
        if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png)) {
            return None;
        }
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(key.max_edge);
        limits.max_image_height = Some(key.max_edge);
        limits.max_alloc = Some(8 * u64::from(key.max_edge).pow(2) + 1024 * 1024);
        reader.limits(limits);
        let decoder = reader.into_decoder().ok()?;
        let (width, height) = decoder.dimensions();
        if width == 0 || height == 0 || width.max(height) > key.max_edge {
            return None;
        }
        let image = DynamicImage::from_decoder(decoder).ok()?.into_rgba8();
        Some(RgbaImage::new(width, height, image.into_raw()))
    }

    pub fn stats(&self) -> CacheStats {
        let (memory_bytes, memory_items) = {
            let mem = self.mem.lock().unwrap();
            (mem.used, mem.entries.len())
        };
        let (disk_bytes, disk_files) = disk_usage(&self.root).unwrap_or((0, 0));
        let (mh, dh, misses) = *self.counters.lock().unwrap();
        CacheStats {
            memory_bytes,
            memory_items,
            disk_bytes,
            disk_files,
            memory_hits: mh,
            disk_hits: dh,
            misses,
        }
    }

    /// Deletes oldest entries until the cache fits `max_bytes`.
    /// Returns the number of bytes freed.
    pub fn set_disk_budget(&self, max_bytes: u64) -> io::Result<u64> {
        let mut disk = self.disk.lock().unwrap();
        disk.budget = max_bytes;
        self.prune_locked(&mut disk, max_bytes)
    }

    fn prune_locked(&self, disk: &mut DiskState, max_bytes: u64) -> io::Result<u64> {
        let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_file() && entry.path().extension().is_some_and(|ext| ext == CACHE_EXT) {
                let modified = meta.modified().unwrap_or(UNIX_EPOCH);
                files.push((modified, meta.len(), entry.path()));
            }
        }
        let total: u64 = files.iter().map(|(_, len, _)| *len).sum();
        disk.bytes = total;
        if total <= max_bytes {
            return Ok(0);
        }
        files.sort_by_key(|(modified, _, _)| *modified);
        let mut freed = 0u64;
        let mut remaining = total;
        for (_, len, path) in files {
            if remaining <= max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                freed += len;
                remaining -= len;
            }
        }
        disk.bytes = remaining;
        Ok(freed)
    }

    pub fn clear(&self) -> io::Result<()> {
        let mut disk = self.disk.lock().unwrap();
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.mem.lock().unwrap().clear();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.metadata()?.is_file() {
                let _ = fs::remove_file(entry.path());
            }
        }
        disk.bytes = disk_usage(&self.root)?.0;
        Ok(())
    }
}

fn encode_thumbnail(image: &RgbaImage, limit: usize) -> image::ImageResult<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    if image.pixels.chunks_exact(4).any(|pixel| pixel[3] < 255) {
        PngEncoder::new(&mut bytes).write_image(
            &image.pixels,
            image.width,
            image.height,
            ExtendedColorType::Rgba8,
        )?;
    } else {
        let rgb: Vec<u8> = image
            .pixels
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect();
        for quality in [83, 65, 45] {
            bytes.clear();
            JpegEncoder::new_with_quality(&mut bytes, quality).encode(
                &rgb,
                image.width,
                image.height,
                ExtendedColorType::Rgb8,
            )?;
            if bytes.len() <= limit {
                break;
            }
        }
    }
    Ok((bytes.len() <= limit).then_some(bytes))
}

fn write_disk(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("{}.{CACHE_EXT}.tmp", std::process::id()));
    let result = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

fn remove_stale_temp_files(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "gthm")
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| {
                    stem.len() == 16 && stem.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        {
            let _ = fs::remove_file(&path);
        } else if path.extension().is_some_and(|ext| ext == "tmp")
            && entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age.as_secs() >= 3600)
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn disk_usage(root: &Path) -> io::Result<(u64, usize)> {
    let mut bytes = 0u64;
    let mut files = 0usize;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() && entry.path().extension().is_some_and(|ext| ext == CACHE_EXT) {
            bytes += meta.len();
            files += 1;
        }
    }
    Ok((bytes, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gimage-cache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn sample(width: u32, height: u32) -> RgbaImage {
        RgbaImage::new(width, height, vec![128u8; (width * height * 4) as usize])
    }

    fn key(px: u32) -> ThumbKey {
        ThumbKey {
            path_hash: 42,
            file_size: 1024,
            mtime_ns: 7,
            max_edge: px,
            version: CACHE_VERSION,
        }
    }

    #[test]
    fn disk_round_trip_and_memory_hit() {
        let dir = temp_dir("roundtrip");
        let cache = ThumbnailCache::new(dir.clone(), 8 * 1024 * 1024).unwrap();
        let image = Arc::new(sample(64, 32));
        cache.put(&key(320), image.clone());
        let hit = cache.get(&key(320)).expect("cached thumbnail");
        assert_eq!(hit.pixels, image.pixels);
        assert_eq!(hit.width, 64);
        // A different variant must miss.
        assert!(cache.get(&key(160)).is_none());
        cache.clear().unwrap();
        assert!(cache.get(&key(320)).is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn memory_budget_evicts_oldest() {
        let mut mem = MemCache::new(64 * 4 * 2); // room for two 64x1 images
        mem.put(key(160), Arc::new(sample(64, 1)));
        mem.put(key(320), Arc::new(sample(64, 1)));
        mem.put(key(640), Arc::new(sample(64, 1)));
        assert_eq!(mem.entries.len(), 2);
        assert!(!mem.entries.contains_key(&key(160)));
    }

    #[test]
    fn prune_frees_space() {
        let dir = temp_dir("prune");
        let cache = ThumbnailCache::new(dir.clone(), 1024).unwrap();
        for i in 0..8 {
            let mut k = key(320);
            k.path_hash = i;
            cache.put(&k, Arc::new(sample(32, 32)));
        }
        let before = cache.stats().disk_bytes;
        assert!(before > 0);
        let freed = cache.set_disk_budget(before / 2).unwrap();
        assert!(freed > 0);
        assert!(cache.stats().disk_bytes <= before / 2 + 4 * 32 * 32);
        let _ = fs::remove_dir_all(dir);
    }
}
