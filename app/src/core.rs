use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use gimage_cache::ThumbnailCache;
use gimage_catalog::Catalog;
use gimage_core::{
    CancellationToken, CodecCapabilities, EditParams, ExportOptions, ImageEngine, ImageFilter,
    RenameOptions, Settings, SortSpec,
};
use parking_lot::{Mutex, RwLock};

use crate::events::{AppEvent, TaskSnapshot};
use crate::folder_watch::FolderWatch;
use crate::task_journal::{TaskJournal, TaskRecord};

/// The currently open folder plus the query state used for paging. A new scan
/// bumps `generation`, which lets every background result be validated cheaply.
#[derive(Clone, Debug)]
pub struct FolderSession {
    pub id: i64,
    pub path: PathBuf,
    pub generation: u64,
    pub sort: SortSpec,
    pub filter: ImageFilter,
    pub total: u64,
    pub temporary_catalog: Option<Arc<Mutex<Catalog>>>,
}

pub const FIRST_TEMP_IMAGE_ID: i64 = 1_500_000_000;

#[derive(Clone, Debug)]
pub struct TaskItem {
    pub id: i64,
    pub path: PathBuf,
    pub edits: Option<ExportEdits>,
    pub rename_target: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ExportEdits {
    pub params: EditParams,
    pub source_version: Option<crate::util::FileVersion>,
}

#[derive(Clone, Debug)]
pub enum BatchOperation {
    Export(ExportOptions),
    Rename(RenameOptions),
}

pub struct TaskRuntime {
    pub cancel: CancellationToken,
    pub operation: BatchOperation,
    pub items: Vec<TaskItem>,
    pub title: String,
    pub failed: Mutex<Vec<TaskItem>>,
}

#[derive(Clone, Debug)]
pub struct ThumbRequest {
    pub generation: u64,
    pub cache_epoch: u64,
    pub id: i64,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub mtime_ns: i64,
    pub px: u32,
}

#[derive(Clone, Debug)]
pub struct RenderRequest {
    pub conversion: Option<ExportOptions>,
    pub seq: u64,
    pub kind: crate::events::RenderKind,
    pub id: i64,
    pub path: PathBuf,
    pub params: EditParams,
    pub max_edge: Option<u32>,
}

pub struct AppCore {
    pub engine: Arc<dyn ImageEngine>,
    pub capabilities: CodecCapabilities,
    pub cache: Arc<ThumbnailCache>,
    pub catalog: Mutex<Catalog>,
    pub settings: RwLock<Settings>,
    pub events: Sender<AppEvent>,
    pub events_rx: Receiver<AppEvent>,
    pub thumbs: Sender<ThumbRequest>,
    pub renders: Sender<Option<RenderRequest>>,
    pub preview_buffer: crate::preview_buffer::PreviewBuffer,
    pub image_writes: RwLock<()>,
    pub probes: Sender<(i64, PathBuf)>,
    pub scan_cancel: Mutex<Option<CancellationToken>>,
    pub folder_watch: Mutex<Option<FolderWatch>>,
    /// Latest requested render sequence; workers drop stale jobs.
    pub render_seq: Arc<AtomicU64>,
    pub tasks: Mutex<HashMap<u64, TaskRuntime>>,
    journal: std::io::Result<TaskJournal>,
    interrupted_tasks: Mutex<HashMap<u64, PathBuf>>,
    next_task_id: AtomicU64,
    next_generation: AtomicU64,
    next_temp_image_id: std::sync::atomic::AtomicI64,
    next_seq: AtomicU64,
    pub session: Mutex<Option<FolderSession>>,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub db_path: PathBuf,
    pub started: Instant,
}

impl AppCore {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine: Arc<dyn ImageEngine>,
        cache: Arc<ThumbnailCache>,
        catalog: Catalog,
        settings: Settings,
        events: Sender<AppEvent>,
        events_rx: Receiver<AppEvent>,
        thumbs: Sender<ThumbRequest>,
        renders: Sender<Option<RenderRequest>>,
        probes: Sender<(i64, PathBuf)>,
        data_dir: PathBuf,
        cache_dir: PathBuf,
        db_path: PathBuf,
    ) -> Self {
        let capabilities = engine.capabilities();
        let journal = TaskJournal::open(&data_dir.join("task-recovery"));
        Self {
            engine,
            capabilities,
            cache,
            catalog: Mutex::new(catalog),
            settings: RwLock::new(settings),
            events,
            events_rx,
            thumbs,
            renders,
            preview_buffer: crate::preview_buffer::PreviewBuffer::new(),
            image_writes: RwLock::new(()),
            probes,
            scan_cancel: Mutex::new(None),
            folder_watch: Mutex::new(None),
            render_seq: Arc::new(AtomicU64::new(0)),
            tasks: Mutex::new(HashMap::new()),
            journal,
            interrupted_tasks: Mutex::new(HashMap::new()),
            next_task_id: AtomicU64::new(1),
            next_generation: AtomicU64::new(1),
            next_temp_image_id: std::sync::atomic::AtomicI64::new(FIRST_TEMP_IMAGE_ID),
            next_seq: AtomicU64::new(1),
            session: Mutex::new(None),
            data_dir,
            cache_dir,
            db_path,
            started: Instant::now(),
        }
    }

    pub fn settings_snapshot(&self) -> Settings {
        self.settings.read().clone()
    }

    pub fn update_settings(&self, settings: Settings) {
        *self.settings.write() = settings;
    }

    pub fn next_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::SeqCst)
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq.fetch_add(1, Ordering::SeqCst)
    }

    pub fn current_session(&self) -> Option<FolderSession> {
        self.session.lock().clone()
    }

    pub fn catalog_for<'a>(&'a self, session: &'a FolderSession) -> parking_lot::MutexGuard<'a, Catalog> {
        session.temporary_catalog.as_deref().unwrap_or(&self.catalog).lock()
    }

    pub fn with_image_catalog<T>(&self, id: i64, read: impl FnOnce(&Catalog) -> T) -> Option<T> {
        if id >= FIRST_TEMP_IMAGE_ID {
            let session = self.current_session()?;
            let catalog = session.temporary_catalog.as_ref()?;
            Some(read(&catalog.lock()))
        } else {
            Some(read(&self.catalog.lock()))
        }
    }

    pub fn reserve_temp_image_ids(&self, count: usize) -> Option<i64> {
        self.next_temp_image_id.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| {
            id.checked_add(count as i64).filter(|&last| last <= i64::from(i32::MAX))
        }).ok()
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.session
            .lock()
            .as_ref()
            .map(|s| s.generation == generation)
            .unwrap_or(false)
    }

    pub fn set_session(&self, session: Option<FolderSession>) {
        *self.session.lock() = session;
    }

    pub fn set_total(&self, generation: u64, total: u64) -> bool {
        if let Some(session) = self.session.lock().as_mut()
            && session.generation == generation
        {
            session.total = total;
            return true;
        }
        false
    }

    /// Register a batch task, returning its id and cancellation token.
    pub fn register_task(&self, title: String, operation: BatchOperation, items: Vec<TaskItem>) -> (u64, CancellationToken) {
        let id = self.next_task_id.fetch_add(1, Ordering::SeqCst);
        let cancel = CancellationToken::new();
        self.tasks.lock().insert(
            id,
            TaskRuntime {
                cancel: cancel.clone(),
                operation,
                items,
                title,
                failed: Mutex::new(Vec::new()),
            },
        );
        (id, cancel)
    }

    pub fn set_task_failed(&self, id: u64, items: Vec<TaskItem>) {
        if let Some(task) = self.tasks.lock().get_mut(&id) {
            *task.failed.lock() = items;
        }
    }

    pub fn start_task_record(&self, id: u64, title: &str, total: usize) -> Result<(), String> {
        self.journal.as_ref().map_err(ToString::to_string)?
            .start(id, &TaskRecord { title: title.to_owned(), total })
            .map_err(|error| error.to_string())
    }

    pub fn finish_task_record(&self, id: u64) {
        if let Ok(journal) = &self.journal && let Err(error) = journal.finish(id) {
            tracing::warn!(%error, id, "could not clear completed task record");
            self.toast("无法更新任务恢复记录", true);
        }
    }

    pub fn restore_interrupted_tasks(&self) {
        let records = self.journal.as_ref().map_err(ToString::to_string)
            .and_then(|journal| journal.interrupted().map_err(|error| error.to_string()));
        let records = match records {
            Ok(records) => records,
            Err(error) => {
                self.toast(format!("无法读取任务恢复记录：{error}"), true);
                return;
            }
        };
        for (path, record) in records {
            let id = self.next_task_id.fetch_add(1, Ordering::SeqCst);
            self.interrupted_tasks.lock().insert(id, path);
            self.emit(AppEvent::Task(TaskSnapshot {
                id,
                title: record.title,
                detail: "上次运行中断，部分文件可能已处理。请检查输出或文件名。".into(),
                status: "已中断".into(),
                progress: 0.0,
                done: 0,
                total: record.total,
                failed: 0,
                state: 6,
                cancelable: false,
                retryable: false,
            }));
        }
    }

    pub fn clear_interrupted_task(&self, id: u64) -> bool {
        let mut tasks = self.interrupted_tasks.lock();
        if let Some(path) = tasks.get(&id) && let Err(error) = TaskJournal::clear(path) {
            self.toast(format!("无法清除中断记录：{error}"), true);
            return false;
        }
        tasks.remove(&id);
        true
    }

    pub fn task_failed(&self, id: u64) -> Vec<TaskItem> {
        self.tasks
            .lock()
            .get(&id)
            .map(|task| task.failed.lock().clone())
            .unwrap_or_default()
    }

    pub fn remove_task(&self, id: u64) {
        self.tasks.lock().remove(&id);
    }

    pub fn task_cancel(&self, id: u64) -> bool {
        match self.tasks.lock().get(&id) {
            Some(task) => {
                task.cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub fn task_items(&self, id: u64) -> Option<(Vec<TaskItem>, BatchOperation, String)> {
        self.tasks
            .lock()
            .get(&id)
            .map(|t| (t.items.clone(), t.operation.clone(), t.title.clone()))
    }

    pub fn emit(&self, event: AppEvent) {
        let _ = self.events.send(event);
    }

    /// Take up to `max` queued UI events. Called from the UI poll timer.
    pub fn drain_events(&self, max: usize) -> Vec<AppEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events_rx.try_recv() {
            events.push(event);
            if events.len() >= max {
                break;
            }
        }
        events
    }

    pub fn toast(&self, message: impl Into<String>, error: bool) {
        self.emit(AppEvent::Toast { message: message.into(), error });
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

/// Collect every image matching the session filter (paged, so a 100k-image
/// folder never allocates one giant query). Runs on a background thread.
pub fn collect_filtered_images(
    core: &AppCore,
    session: &FolderSession,
    limit_pages: u32,
) -> Result<Vec<gimage_core::ImageMeta>, String> {
    let catalog = core.catalog_for(session);
    let mut all = Vec::new();
    let mut offset = 0u32;
    let page_size = 500;
    loop {
        let page = catalog
            .images_page(session.id, &session.sort, &session.filter, offset, page_size)
            .map_err(|e| e.to_string())?;
        let total = page.total;
        let count = page.items.len();
        all.extend(page.items);
        offset += count as u32;
        if count == 0 || offset as u64 >= total || all.len() >= 200_000 {
            break;
        }
        if limit_pages > 0 && offset >= limit_pages * page_size {
            break;
        }
    }
    Ok(all)
}
