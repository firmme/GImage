use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use gimage_core::types::SCANNABLE_EXTENSIONS;
use gimage_core::{CancellationToken, ImageFormat};
use walkdir::{DirEntry, WalkDir};

use crate::CatalogError;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub recursive: bool,
    pub include_hidden: bool,
    pub max_depth: usize,
    /// Hard safety cap so a mistaken drive-root scan cannot exhaust memory.
    pub max_files: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            recursive: true,
            include_hidden: false,
            max_depth: 32,
            max_files: 200_000,
        }
    }
}

/// Metadata captured during a scan. No pixels are touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified_ns: i64,
    pub format: ImageFormat,
}

#[derive(Clone, Debug)]
pub struct ScanProgress {
    pub files_seen: u64,
    pub images_found: u64,
    pub current_dir: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct ScanOutput {
    pub scope: Option<(PathBuf, usize)>,
    pub max_image_depth: usize,
    pub files: Vec<ScannedFile>,
    pub canceled: bool,
    pub files_seen: u64,
    /// Files skipped because of name/extension/hidden filters.
    pub skipped: u64,
    /// The safety cap stopped enumeration before the walk completed.
    pub truncated: bool,
    /// IO failures make absence inconclusive; filtered files do not count.
    pub errors: u64,
}

impl ScanOutput {
    pub fn is_complete(&self) -> bool {
        !self.canceled && !self.truncated && self.errors == 0
    }
}

/// Volume/system bookkeeping directories that never contain user pictures.
const JUNK_DIRS: &[&str] = &[
    "$RECYCLE.BIN",
    "System Volume Information",
    "$WinREAgent",
    "$SysReset",
    "Config.Msi",
];

fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.')
}

fn is_junk_dir(name: &str) -> bool {
    JUNK_DIRS.iter().any(|junk| name.eq_ignore_ascii_case(junk))
}

/// `filter_entry` predicate: prunes hidden/junk *directories* early. Files are
/// handled inside the walk so that hidden/non-image files are still counted as
/// skipped instead of silently disappearing.
fn entry_allowed(entry: &DirEntry, options: &ScanOptions) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    if is_junk_dir(&name) {
        return false;
    }
    if !options.include_hidden && is_hidden_name(&name) {
        return false;
    }
    true
}

/// Recognise a scannable image purely from its extension. Both the shared
/// allow-list and `ImageFormat::from_path` are consulted so that adding an
/// extension in core automatically makes it scannable here.
fn image_format_for(path: &Path) -> Option<ImageFormat> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if !SCANNABLE_EXTENSIONS.iter().any(|known| *known == extension) {
        return None;
    }
    ImageFormat::from_path(path)
}

/// `SystemTime` -> nanoseconds since the UNIX epoch, saturating instead of
/// panicking/overflowing for dates before 1970 or beyond 2922.
fn system_time_ns(time: Option<std::time::SystemTime>) -> i64 {
    let Some(time) = time else {
        return 0;
    };
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos().min(i64::MAX as u128) as i64,
        Err(err) => {
            let nanos = err.duration().as_nanos().min(i64::MAX as u128) as i64;
            -nanos
        }
    }
}

/// Walk `root` and collect recognised images.
///
/// Must be cancellation-aware (checked per directory) and must never panic on
/// unreadable entries: permission errors are skipped and counted.
pub fn scan_folder(
    root: &Path,
    options: &ScanOptions,
    cancel: &CancellationToken,
    progress: impl FnMut(ScanProgress) + Send,
) -> Result<ScanOutput, CatalogError> {
    // The catalog only ever stores canonical absolute paths. A missing root,
    // a file where a directory was expected, or an unreadable root is a caller
    // error, not an empty folder.
    let canonical_root = fs::canonicalize(root).map_err(|_| {
        CatalogError::InvalidPath(format!("不是可访问的文件夹：{}", root.display()))
    })?;
    let root_metadata = fs::metadata(&canonical_root).map_err(|_| {
        CatalogError::InvalidPath(format!("不是可访问的文件夹：{}", root.display()))
    })?;
    if !root_metadata.is_dir() {
        return Err(CatalogError::InvalidPath(format!(
            "不是文件夹：{}",
            root.display()
        )));
    }

    if cancel.is_cancelled() {
        return Ok(ScanOutput {
            canceled: true,
            ..ScanOutput::default()
        });
    }

    let max_depth = if options.recursive {
        options.max_depth
    } else {
        1
    };
    let filter_options = options.clone();
    let walker = WalkDir::new(&canonical_root)
        .follow_links(false)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(move |entry| entry_allowed(entry, &filter_options));

    let mut progress_fn = progress;
    let mut report = |files_seen: u64, images_found: u64, current_dir: &Path| {
        progress_fn(ScanProgress {
            files_seen,
            images_found,
            current_dir: current_dir.to_path_buf(),
        });
    };

    let mut files: Vec<ScannedFile> = Vec::new();
    let mut files_seen: u64 = 0;
    let mut skipped: u64 = 0;
    let mut truncated = false;
    let mut errors = 0u64;
    let mut max_image_depth = 0;
    let mut last_dir: Option<PathBuf> = None;

    for entry in walker {
        if cancel.is_cancelled() {
            return Ok(ScanOutput {
                scope: Some((canonical_root, max_depth.saturating_sub(1))),
                max_image_depth,
                files,
                canceled: true,
                files_seen,
                skipped,
                truncated,
                errors,
            });
        }

        // Safety cap: stop before touching another entry once the cap is hit.
        if files.len() as u64 >= options.max_files {
            truncated = true;
            tracing::debug!(
                files = files.len(),
                cap = options.max_files,
                "catalog scan stopped at max_files cap"
            );
            break;
        }

        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                // Permission denied, a file deleted mid-walk, an over-long
                // path, ... : never fatal, just skipped.
                tracing::debug!(error = %err, "catalog scan skipped unreadable entry");
                skipped = skipped.saturating_add(1);
                errors = errors.saturating_add(1);
                continue;
            }
        };

        let file_type = entry.file_type();
        if file_type.is_dir() {
            if last_dir.as_deref() != Some(entry.path()) {
                last_dir = Some(entry.path().to_path_buf());
                report(files_seen, files.len() as u64, entry.path());
            }
            continue;
        }

        // Symlinks (not followed), sockets, devices: not catalog material.
        if !file_type.is_file() {
            skipped = skipped.saturating_add(1);
            continue;
        }

        files_seen = files_seen.saturating_add(1);
        let path = entry.path();
        let directory = path.parent().unwrap_or(canonical_root.as_path());

        if last_dir.as_deref() != Some(directory) {
            last_dir = Some(directory.to_path_buf());
            report(files_seen, files.len() as u64, directory);
        } else if files_seen.is_multiple_of(64) {
            report(files_seen, files.len() as u64, directory);
        }

        let name = entry.file_name().to_string_lossy();
        if !options.include_hidden && is_hidden_name(&name) {
            skipped = skipped.saturating_add(1);
            continue;
        }

        let Some(format) = image_format_for(path) else {
            skipped = skipped.saturating_add(1);
            continue;
        };

        // Paths are persisted as UTF-8 TEXT; a lossy conversion would make the
        // stored path unusable for reopening, so such names are skipped.
        if path.to_str().is_none() {
            tracing::debug!(path = %path.display(), "catalog scan skipped non-UTF-8 path");
            skipped = skipped.saturating_add(1);
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                tracing::debug!(path = %path.display(), error = %err, "catalog scan skipped unreadable file");
                skipped = skipped.saturating_add(1);
                errors = errors.saturating_add(1);
                continue;
            }
        };

        max_image_depth = max_image_depth.max(entry.depth().saturating_sub(1));
        files.push(ScannedFile {
            path: path.to_path_buf(),
            size_bytes: metadata.len(),
            modified_ns: system_time_ns(metadata.modified().ok()),
            format,
        });
    }

    // Always emit a final snapshot so the UI shows the settled counts.
    let final_dir = last_dir.as_deref().unwrap_or(canonical_root.as_path());
    report(files_seen, files.len() as u64, final_dir);

    Ok(ScanOutput {
        scope: Some((canonical_root, max_depth.saturating_sub(1))),
        max_image_depth,
        files,
        canceled: cancel.is_cancelled(),
        files_seen,
        skipped,
        truncated,
        errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write_file(path: &Path, len: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, vec![0u8; len]).unwrap();
    }

    fn names(output: &ScanOutput) -> Vec<String> {
        let mut names: Vec<String> = output
            .files
            .iter()
            .map(|file| {
                file.path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn scan_finds_images_and_skips_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("top.jpg"), 3);
        write_file(&root.join("nested/mid.png"), 4);
        write_file(&root.join("nested/deep/deep.JPEG"), 5);
        write_file(&root.join("notes.txt"), 6);
        write_file(&root.join(".hidden.jpg"), 7);
        write_file(&root.join(".hidden_dir/inside.jpg"), 8);
        write_file(&root.join("$RECYCLE.BIN/trash.jpg"), 9);
        write_file(&root.join("System Volume Information/sys.jpg"), 10);

        let out = scan_folder(
            root,
            &ScanOptions::default(),
            &CancellationToken::new(),
            |_| {},
        )
        .unwrap();

        assert!(!out.canceled);
        assert_eq!(names(&out), vec!["deep.JPEG", "mid.png", "top.jpg"]);
        // top.jpg, mid.png, deep.JPEG, notes.txt, .hidden.jpg. The hidden and
        // junk directories are pruned before their contents are enumerated.
        assert_eq!(out.files_seen, 5);
        assert_eq!(out.skipped, 2);

        let format_of = |name: &str| {
            out.files
                .iter()
                .find(|file| file.path.file_name().unwrap().to_string_lossy() == name)
                .map(|file| file.format.clone())
                .unwrap()
        };
        assert_eq!(format_of("top.jpg"), ImageFormat::Jpeg);
        assert_eq!(format_of("mid.png"), ImageFormat::Png);
        assert_eq!(format_of("deep.JPEG"), ImageFormat::Jpeg);

        for file in &out.files {
            assert!(file.size_bytes > 0);
            assert!(file.modified_ns > 0);
            assert!(file.path.is_absolute());
        }
    }

    #[test]
    fn include_hidden_adds_dot_files_and_dirs_but_not_junk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("top.jpg"), 1);
        write_file(&root.join(".hidden.jpg"), 1);
        write_file(&root.join(".hidden_dir/inside.png"), 1);
        write_file(&root.join("$RECYCLE.BIN/trash.jpg"), 1);
        write_file(&root.join("notes.txt"), 1);

        let options = ScanOptions {
            include_hidden: true,
            ..ScanOptions::default()
        };
        let out = scan_folder(root, &options, &CancellationToken::new(), |_| {}).unwrap();

        assert_eq!(names(&out), vec![".hidden.jpg", "inside.png", "top.jpg"]);
        // top.jpg, .hidden.jpg, .hidden_dir/inside.png, notes.txt; the junk
        // directory is still pruned even with hidden entries enabled.
        assert_eq!(out.files_seen, 4);
        assert_eq!(out.skipped, 1);
    }

    #[test]
    fn non_recursive_scan_stays_in_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("top.jpg"), 1);
        write_file(&root.join("nested/inside.png"), 1);

        let options = ScanOptions {
            recursive: false,
            ..ScanOptions::default()
        };
        let out = scan_folder(root, &options, &CancellationToken::new(), |_| {}).unwrap();
        assert_eq!(names(&out), vec!["top.jpg"]);
        assert_eq!(out.files_seen, 1);
    }

    #[test]
    fn max_depth_is_respected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("top.jpg"), 1);
        write_file(&root.join("a/mid.png"), 1);
        write_file(&root.join("a/b/deep.png"), 1);

        let options = ScanOptions {
            max_depth: 2,
            ..ScanOptions::default()
        };
        let out = scan_folder(root, &options, &CancellationToken::new(), |_| {}).unwrap();
        assert_eq!(names(&out), vec!["mid.png", "top.jpg"]);
    }

    #[test]
    fn max_files_caps_the_collection() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for index in 0..3 {
            write_file(&root.join(format!("img{index}.jpg")), 1);
        }

        let options = ScanOptions {
            max_files: 2,
            ..ScanOptions::default()
        };
        let out = scan_folder(root, &options, &CancellationToken::new(), |_| {}).unwrap();
        assert_eq!(out.files.len(), 2);
        assert_eq!(out.files_seen, 2);
        assert!(!out.canceled);
        assert!(out.truncated);
        assert!(!out.is_complete());
    }

    #[test]
    fn reaching_cap_at_end_of_walk_is_complete() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("one.jpg"), 1);
        let options = ScanOptions {
            max_files: 1,
            ..ScanOptions::default()
        };
        let out = scan_folder(tmp.path(), &options, &CancellationToken::new(), |_| {}).unwrap();
        assert_eq!(out.files.len(), 1);
        assert!(out.is_complete());
    }

    #[test]
    fn cancellation_in_final_progress_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("one.jpg"), 1);
        let token = CancellationToken::new();
        let cancel = token.clone();
        let out = scan_folder(tmp.path(), &ScanOptions::default(), &token, move |p| {
            if p.images_found == 1 {
                cancel.cancel();
            }
        })
        .unwrap();
        assert_eq!(out.files.len(), 1);
        assert!(out.canceled);
        assert!(!out.is_complete());
    }

    #[test]
    fn pre_cancelled_token_returns_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("img.jpg"), 1);

        let token = CancellationToken::new();
        token.cancel();
        let out = scan_folder(tmp.path(), &ScanOptions::default(), &token, |_| {}).unwrap();

        assert!(out.canceled);
        assert!(out.files.is_empty());
        assert_eq!(out.files_seen, 0);
    }

    #[test]
    fn cancellation_from_progress_stops_promptly() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for index in 0..20 {
            write_file(&root.join(format!("img{index}.jpg")), 1);
        }

        let token = CancellationToken::new();
        let cancel_from_callback = token.clone();
        let out = scan_folder(root, &ScanOptions::default(), &token, move |_| {
            cancel_from_callback.cancel();
        })
        .unwrap();

        assert!(out.canceled);
        assert!(out.files.len() <= 1);
    }

    #[test]
    fn progress_reports_a_final_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("one.jpg"), 1);
        write_file(&root.join("nested/two.png"), 1);

        let mut reports = Vec::new();
        let out = scan_folder(
            tmp.path(),
            &ScanOptions::default(),
            &CancellationToken::new(),
            |p| reports.push(p),
        )
        .unwrap();

        assert!(!reports.is_empty());
        let last = reports.last().unwrap();
        assert_eq!(last.images_found, out.files.len() as u64);
        assert_eq!(last.files_seen, out.files_seen);
        assert!(root.is_dir());
    }

    #[test]
    fn invalid_roots_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(matches!(
            scan_folder(
                &missing,
                &ScanOptions::default(),
                &CancellationToken::new(),
                |_| {}
            ),
            Err(CatalogError::InvalidPath(_))
        ));

        let file = tmp.path().join("plain.jpg");
        write_file(&file, 1);
        assert!(matches!(
            scan_folder(
                &file,
                &ScanOptions::default(),
                &CancellationToken::new(),
                |_| {}
            ),
            Err(CatalogError::InvalidPath(_))
        ));
    }

    /// Unreadable and vanished entries must never panic. A dangling symlink is
    /// the portable stand-in for "exists but cannot be resolved"; on Windows it
    /// needs Developer Mode/SeCreateSymbolicLink, so creation failure is not a
    /// test failure.
    #[test]
    fn scan_tolerates_dangling_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_file(&root.join("real.jpg"), 1);

        #[cfg(windows)]
        let link_created = std::os::windows::fs::symlink_file(
            tmp.path().join("missing-target.jpg"),
            root.join("dangling.jpg"),
        )
        .is_ok();

        #[cfg(unix)]
        let link_created = std::os::unix::fs::symlink(
            tmp.path().join("missing-target.jpg"),
            root.join("dangling.jpg"),
        )
        .is_ok();

        let out = scan_folder(
            root,
            &ScanOptions::default(),
            &CancellationToken::new(),
            |_| {},
        )
        .unwrap();
        assert!(!out.canceled);
        assert_eq!(out.files.len(), 1);
        assert_eq!(out.files[0].path.file_name().unwrap(), "real.jpg");
        if link_created {
            assert!(
                out.files
                    .iter()
                    .all(|file| file.path.file_name().unwrap() != "dangling.jpg")
            );
        }
    }
}
