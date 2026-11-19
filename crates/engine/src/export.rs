//! Export path resolution and crash-safe writing.
//!
//! [`export_image`] renders at full resolution, applies the optional
//! [`gimage_core::ResizeSpec`], resolves the destination according to the
//! [`gimage_core::OutputPolicy`] and finally writes it through
//! [`write_atomic`].
//!
//! [`write_atomic`] never touches the destination until a complete, valid file
//! exists next to it:
//!
//! 1. create a uniquely named sibling temp file (`.gimage-tmp-<pid>-<n>.tmp`)
//!    in the destination directory, so the later rename stays on one volume;
//! 2. encode into that temp file, flush it and `sync_all` it;
//! 3. `rename` the temp file over the destination (on Windows this replaces an
//!    existing file atomically through `MoveFileExW`).
//!
//! On any failure the temp file is removed by a drop guard and the original
//! destination is untouched. The source image is only ever opened for reading;
//! exporting to the source path is rejected before any write happens.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gimage_core::{EngineError, ExportFormat, ExportOptions, ImageEngine, OutputPolicy, ResizeMode, RgbaImage};
use image::{Rgba, RgbaImage as ImageBuffer, imageops};

use crate::pipeline;
use crate::{ExportOutcome, ExportRequest, ExportStatus};

/// Render -> (optional resize) -> encode to a temp file -> atomic rename.
/// Never overwrites the source image.
pub fn export_image(
    engine: &dyn ImageEngine,
    request: &ExportRequest,
) -> Result<ExportStatus, EngineError> {
    let options = request.options.normalized();

    // Resolve first so `SkipExisting` can skip without decoding anything.
    let Some(destination) = resolve_destination(&request.destination, options.output) else {
        return Ok(ExportStatus::Skipped);
    };
    ensure_not_source(&request.source, &destination)?;

    let image = engine.render(&request.source, &request.params, None)?;
    let image = transform_export(image, &options, None)
        .map_err(|message| EngineError::encode(&destination, message))?;

    let bytes = write_atomic(
        engine,
        &image,
        options.format,
        options.quality,
        &destination,
    )?;
    Ok(ExportStatus::Written(ExportOutcome {
        path: destination,
        bytes,
    }))
}

pub fn transform_export(image: RgbaImage, options: &ExportOptions, preview_edge: Option<u32>) -> Result<RgbaImage, &'static str> {
    let image = pipeline::rotate_image(image, options.rotation);
    let Some(size) = options.resize.filter(|_| options.resize_mode != ResizeMode::Original) else {
        return Ok(image);
    };
    let mode = options.resize_mode;
    let (width, height) = (image.width, image.height);
    if width == 0 || height == 0 || size.max_width == 0 || size.max_height == 0 {
        return Err("图片或目标尺寸无效");
    }

    let (target_width, target_height) = match mode {
        ResizeMode::Limit => size.fit(width, height),
        ResizeMode::Contain => contain_size(width, height, size.max_width, size.max_height),
        ResizeMode::FitWidth => {
            let height = (u64::from(height) * u64::from(size.max_width) + u64::from(width) / 2) / u64::from(width);
            (size.max_width, u32::try_from(height.max(1)).map_err(|_| "按宽度调整后的图片过高")?)
        }
        _ => (size.max_width, size.max_height),
    };
    if u64::from(target_width) * u64::from(target_height) > 100_000_000 {
        return Err("输出尺寸超过 1 亿像素");
    }
    let (out_width, out_height) = if let Some(edge) = preview_edge {
        pipeline::fit_edge(target_width, target_height, edge)
    } else {
        (target_width, target_height)
    };
    if mode == ResizeMode::Limit {
        return Ok(pipeline::resize_to_exact(image, out_width, out_height));
    }
    if (width, height) == (out_width, out_height) {
        return Ok(image);
    }

    let input = ImageBuffer::from_raw(width, height, image.pixels).ok_or("图片缓冲大小无效")?;
    let filter = imageops::FilterType::Lanczos3;
    let result = match mode {
        ResizeMode::Pad => {
            let (fit_width, fit_height) = contain_size(width, height, out_width, out_height);
            let scaled = imageops::resize(&input, fit_width, fit_height, filter);
            let fill = if options.format.supports_alpha() { [0, 0, 0, 0] } else { [255; 4] };
            let mut output = ImageBuffer::from_pixel(out_width, out_height, Rgba(fill));
            imageops::overlay(&mut output, &scaled,
                i64::from((out_width - fit_width) / 2), i64::from((out_height - fit_height) / 2));
            output
        }
        ResizeMode::Crop => {
            let crop_width = (u64::from(height) * u64::from(target_width) / u64::from(target_height))
                .clamp(1, u64::from(width)) as u32;
            let crop_height = (u64::from(width) * u64::from(target_height) / u64::from(target_width))
                .clamp(1, u64::from(height)) as u32;
            let crop = imageops::crop_imm(&input, (width - crop_width) / 2, (height - crop_height) / 2, crop_width, crop_height);
            imageops::resize(&*crop, out_width, out_height, filter)
        }
        _ => imageops::resize(&input, out_width, out_height, filter),
    };
    Ok(pipeline::from_image_buffer(result))
}

fn contain_size(width: u32, height: u32, bound_width: u32, bound_height: u32) -> (u32, u32) {
    let scale = (f64::from(bound_width) / f64::from(width))
        .min(f64::from(bound_height) / f64::from(height));
    (
        ((f64::from(width) * scale).round() as u32).clamp(1, bound_width),
        ((f64::from(height) * scale).round() as u32).clamp(1, bound_height),
    )
}

/// First free sibling path: `name (2).ext`, `name (3).ext`, ...
///
/// Paths that do not exist are returned unchanged. Paths without a file name
/// (for example a root directory) are also returned unchanged because no safe
/// sibling name can be derived from them.
pub fn unique_destination(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let Some(file_name) = path.file_name() else {
        return path.to_path_buf();
    };
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path
        .file_stem()
        .map(OsString::from)
        .unwrap_or_else(|| file_name.to_os_string());
    let extension = path.extension().map(OsString::from);

    for index in 2u64.. {
        let mut candidate_name = stem.clone();
        candidate_name.push(format!(" ({index})"));
        if let Some(extension) = &extension {
            candidate_name.push(".");
            candidate_name.push(extension);
        }
        let candidate = parent.join(&candidate_name);
        if !candidate.exists() {
            return candidate;
        }
        // Extremely defensive: never spin forever on a hostile directory.
        if index >= 1_000_000 {
            break;
        }
    }
    path.to_path_buf()
}

/// Destination chosen by an output policy; `None` means "skip this file".
pub fn resolve_destination(destination: &Path, policy: OutputPolicy) -> Option<PathBuf> {
    match policy {
        OutputPolicy::AutoRename => Some(unique_destination(destination)),
        OutputPolicy::SkipExisting => {
            if destination.exists() {
                None
            } else {
                Some(destination.to_path_buf())
            }
        }
        OutputPolicy::Overwrite => Some(destination.to_path_buf()),
    }
}

/// Encode into a sibling temp file, flush/sync, then atomically rename.
/// Returns the number of bytes written.
pub fn write_atomic(
    engine: &dyn ImageEngine,
    image: &RgbaImage,
    format: ExportFormat,
    quality: u8,
    destination: &Path,
) -> Result<u64, EngineError> {
    write_atomic_checked(engine, image, format, quality, destination, || Ok(()))
}

pub fn write_atomic_checked(
    engine: &dyn ImageEngine,
    image: &RgbaImage,
    format: ExportFormat,
    quality: u8,
    destination: &Path,
    check_destination: impl FnOnce() -> Result<(), EngineError>,
) -> Result<u64, EngineError> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = TempDestination::create(parent)?;

    engine.encode(image, format, quality, temp.path())?;

    let bytes = {
        let file = OpenOptions::new()
            .write(true)
            .open(temp.path())
            .map_err(|error| EngineError::io(temp.path(), error))?;
        file.sync_all()
            .map_err(|error| EngineError::io(temp.path(), error))?;
        file.metadata().map_err(|error| EngineError::io(temp.path(), error))?.len()
    };

    check_destination()?;
    fs::rename(temp.path(), destination).map_err(|error| EngineError::io(destination, error))?;
    temp.disarm();

    Ok(bytes)
}

/// A unique temp file in the destination directory that deletes itself unless
/// it was successfully renamed.
struct TempDestination {
    path: PathBuf,
    armed: bool,
}

impl TempDestination {
    fn create(parent: &Path) -> Result<Self, EngineError> {
        for attempt in 0..64u32 {
            let path = parent.join(temp_file_name(attempt));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path, armed: true }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(EngineError::io(path, error)),
            }
        }
        Err(EngineError::io(
            parent.to_path_buf(),
            io::Error::new(io::ErrorKind::AlreadyExists, "无法创建唯一的临时文件名"),
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

/// Unique-ish temp file name without any mutable global counter. Collisions are
/// resolved by `create_new` in the caller's retry loop.
fn temp_file_name(attempt: u32) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(".gimage-tmp-{}-{nanos}-{attempt}.tmp", std::process::id())
}

impl Drop for TempDestination {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Refuse to write over the source image (canonicalised, case-insensitive on
/// Windows).
fn ensure_not_source(source: &Path, destination: &Path) -> Result<(), EngineError> {
    if same_path(source, destination) {
        return Err(EngineError::encode(
            destination,
            format!("拒绝覆盖源文件：{}", source.display()),
        ));
    }
    Ok(())
}

fn same_path(first: &Path, second: &Path) -> bool {
    if let (Ok(first), Ok(second)) = (fs::canonicalize(first), fs::canonicalize(second)) {
        return paths_equal(&first, &second);
    }
    // Fallback for destinations that cannot be canonicalised yet (for example
    // because they do not exist): compare canonical parent + file name.
    match (absolute_normalized(first), absolute_normalized(second)) {
        (Some(first), Some(second)) => paths_equal(&first, &second),
        _ => false,
    }
}

fn absolute_normalized(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let file_name = absolute.file_name()?;
    let parent = absolute.parent().unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    Some(parent.join(file_name))
}

fn paths_equal(first: &Path, second: &Path) -> bool {
    #[cfg(windows)]
    {
        first
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&second.as_os_str().to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        first == second
    }
}
