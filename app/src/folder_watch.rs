use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gimage_core::types::SCANNABLE_EXTENSIONS;
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;

const QUIET: Duration = Duration::from_millis(500);
const MAX_WAIT: Duration = Duration::from_secs(3);

pub struct FolderWatch {
    path: PathBuf,
    depth: u8,
    generation: u64,
    pending: Arc<Mutex<Option<(Instant, Instant)>>>,
    _watcher: RecommendedWatcher,
}

impl FolderWatch {
    pub fn new(path: &Path, depth: u8, generation: u64) -> notify::Result<Self> {
        let canonical = std::fs::canonicalize(path).map_err(notify::Error::io)?;
        let watch_root = canonical.clone();
        let pending = Arc::new(Mutex::new(None));
        let changes = Arc::clone(&pending);
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            match event {
                Ok(event) if !affects_catalog(&event, &watch_root, depth) => return,
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "directory watch error"),
            }
            let now = Instant::now();
            let mut pending = changes.lock();
            let (first, _) = pending.unwrap_or((now, now));
            *pending = Some((first, now));
        })?;
        let mode = if depth > 0 {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher.watch(&canonical, mode)?;
        Ok(Self {
            path: path.to_owned(),
            depth,
            generation,
            pending,
            _watcher: watcher,
        })
    }

    pub fn matches(&self, path: &Path, depth: u8) -> bool {
        self.path == path && self.depth == depth
    }

    pub fn begin_scan(&mut self, generation: u64) {
        self.generation = generation;
        self.pending.lock().take();
    }

    pub fn take_due(&self, generation: u64, now: Instant) -> bool {
        if self.generation != generation {
            return false;
        }
        let mut pending = self.pending.lock();
        if pending.is_some_and(|(first, last)| {
            now.saturating_duration_since(last) >= QUIET
                || now.saturating_duration_since(first) >= MAX_WAIT
        }) {
            pending.take();
            return true;
        }
        false
    }
}

fn affects_catalog(event: &Event, root: &Path, depth: u8) -> bool {
    if event.need_rescan() {
        return true;
    }
    let structural = match event.kind {
        EventKind::Access(_) => return false,
        EventKind::Create(kind) => kind != CreateKind::File,
        EventKind::Remove(kind) => kind != RemoveKind::File,
        EventKind::Modify(ModifyKind::Name(_)) | EventKind::Any | EventKind::Other => true,
        EventKind::Modify(_) => false,
    };
    event.paths.is_empty()
        || event.paths.iter().any(|path| {
            if path
                .strip_prefix(root)
                .is_ok_and(|relative| relative.components().count() > usize::from(depth) + 1)
            {
                return false;
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if name.starts_with(".gimage-tmp-") && name.ends_with(".tmp") {
                return false;
            }
            structural
                || path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        SCANNABLE_EXTENSIONS
                            .iter()
                            .any(|known| known.eq_ignore_ascii_case(ext))
                    })
        })
}
