use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gimage_core::{
    EditParams, ExportFormat, ExportOptions, ImageFilter, ImageMeta, MetadataPolicy, OutputPolicy,
    RenameOptions, RenameRule, Settings, SortSpec,
};
use gimage_engine::{
    ExportRequest, ExportStatus, export_image,
};
use gimage_task_queue::{BatchEvent, BatchOptions, run_batch};

use crate::core::{
    AppCore, BatchOperation, FolderSession, TaskItem,
};
use crate::events::{AppEvent, FolderSummary, RenderKind, ScanReport, TaskSnapshot};
use crate::util::{
    export_destination, file_version, format_time_ms, meta_line, now_ms, size_text,
};

pub const PAGE_SIZE: u32 = 300;

fn open_catalog(core: &AppCore) -> parking_lot::MutexGuard<'_, gimage_catalog::Catalog> {
    // parking_lot guard is not tied to a std MutexGuard; wrapper keeps callers
    // explicit about lock scope.
    core.catalog.lock()
}

/// Start a new scan generation. Cancels any in-flight scan and clears the UI
/// state through the caller (the caller owns the UI thread).
pub fn begin_scan(core: &Arc<AppCore>, path: PathBuf, sort: SortSpec, mut filter: ImageFilter) -> u64 {
    filter.max_directory_depth = Some(core.settings_snapshot().effective_scan_depth());
    let generation = core.next_generation();
    if let Some(token) = core.scan_cancel.lock().take() {
        token.cancel();
    }
    let cancel = gimage_core::CancellationToken::new();
    *core.scan_cancel.lock() = Some(cancel.clone());
    core.set_session(Some(FolderSession {
        id: 0,
        path: path.clone(),
        generation,
        sort: sort.clone(),
        filter: filter.clone(),
        total: 0,
        temporary_catalog: None,
    }));
    spawn_scan(core.clone(), path, generation, cancel, sort, filter);
    generation
}

fn spawn_scan(
    core: Arc<AppCore>,
    path: PathBuf,
    generation: u64,
    cancel: gimage_core::CancellationToken,
    sort: SortSpec,
    filter: ImageFilter,
) {
    let _ = std::thread::Builder::new()
        .name("gimage-scan".into())
        .spawn(move || {
            let started = Instant::now();
            let settings = core.settings_snapshot();
            let scan_depth = filter.max_directory_depth.unwrap_or(5);
            {
                let mut watch = core.folder_watch.lock();
                if !core.is_current(generation) {
                    return;
                }
                if let Some(watch) = watch
                    .as_mut()
                    .filter(|watch| watch.matches(&path, scan_depth))
                {
                    watch.begin_scan(generation);
                } else {
                    *watch = match crate::folder_watch::FolderWatch::new(
                        &path,
                        scan_depth,
                        generation,
                    ) {
                        Ok(watch) => Some(watch),
                        Err(error) => {
                            tracing::warn!(%error, folder = %path.display(), "could not watch folder");
                            None
                        }
                    };
                }
            }
            let folder_id = {
                let catalog = open_catalog(&core);
                match catalog.upsert_folder(&path) {
                    Ok(id) => id,
                    Err(error) => {
                        core.emit(AppEvent::ScanFinished {
                            generation,
                            report: Err(format!("无法索引文件夹：{error}")),
                        });
                        return;
                    }
                }
            };
            {
                let mut session = core.session.lock();
                if let Some(session) = session.as_mut()
                    && session.generation == generation
                {
                    session.id = folder_id;
                }
            }

            let options = gimage_catalog::ScanOptions {
                recursive: scan_depth > 0,
                include_hidden: settings.include_hidden,
                max_depth: usize::from(scan_depth) + 1,
                ..Default::default()
            };
            let mut last_progress = Instant::now();
            let scanned = {
                let core = Arc::clone(&core);
                gimage_catalog::scan_folder(&path, &options, &cancel, move |progress| {
                    if last_progress.elapsed() >= Duration::from_millis(150) {
                        last_progress = Instant::now();
                        core.emit(AppEvent::ScanProgress {
                            generation,
                            scanned: progress.files_seen,
                            found: progress.images_found,
                            current: progress.current_dir.to_string_lossy().into_owned(),
                        });
                    }
                })
            };

            let output = match scanned {
                Ok(output) => output,
                Err(error) => {
                    core.emit(AppEvent::ScanFinished {
                        generation,
                        report: Err(format!("扫描失败：{error}")),
                    });
                    return;
                }
            };

            let sync = {
                let mut catalog = open_catalog(&core);
                // A replaced scan must not overwrite a newer folder session.
                if !core.is_current(generation) {
                    return;
                }
                match catalog.sync_scan(folder_id, &output) {
                    Ok(summary) => {
                        if output.is_complete() {
                            let _ = catalog.touch_folder_scan(folder_id);
                        }
                        summary
                    }
                    Err(error) => {
                        core.emit(AppEvent::ScanFinished {
                            generation,
                            report: Err(format!("写入索引失败：{error}")),
                        });
                        return;
                    }
                }
            };

            let page = {
                let catalog = open_catalog(&core);
                catalog.images_page(folder_id, &sort, &filter, 0, PAGE_SIZE)
            };

            match page {
                Ok(page) => {
                    if !core.set_total(generation, page.total) {
                        return;
                    }
                    core.emit(AppEvent::PageReady {
                        generation,
                        offset: 0,
                        total: page.total,
                        items: page.items,
                    });
                    let total = page.total;
                    core.emit(AppEvent::ScanFinished {
                        generation,
                        report: Ok(ScanReport {
                            folder_id,
                            path: path.clone(),
                            added: sync.added,
                            updated: sync.updated,
                            removed: sync.removed,
                            canceled: output.canceled,
                            truncated: output.truncated,
                            errors: output.errors,
                            max_image_depth: output.max_image_depth,
                            total,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                        }),
                    });
                }
                Err(error) => {
                    core.emit(AppEvent::ScanFinished {
                        generation,
                        report: Err(format!("查询图片失败：{error}")),
                    });
                }
            }

            publish_library(&core);
        });
}

/// Query one page of the current session on a background thread.
pub fn load_page(core: &Arc<AppCore>, generation: u64, offset: u32) {
    let Some(session) = core.current_session() else {
        return;
    };
    if session.generation != generation || session.id == 0 {
        return;
    }
    let core = Arc::clone(core);
    let _ = std::thread::Builder::new()
        .name("gimage-page".into())
        .spawn(move || {
            let page = {
                let catalog = core.catalog_for(&session);
                catalog.images_page(session.id, &session.sort, &session.filter, offset, PAGE_SIZE)
            };
            match page {
                Ok(page) => {
                    if !core.is_current(generation) {
                        return;
                    }
                    if !core.set_total(generation, page.total) {
                        return;
                    }
                    core.emit(AppEvent::PageReady {
                        generation,
                        offset,
                        total: page.total,
                        items: page.items,
                    });
                }
                Err(error) => core.emit(AppEvent::PageFailed {
                    generation,
                    error: error.to_string(),
                }),
            }
        });
}

/// Refresh the folder sidebar + database stats. Cheap enough to run on scans.
pub fn publish_library(core: &AppCore) {
    let hidden = core.settings_snapshot().hidden_recent_folders;
    let (folders, stats) = {
        let catalog = open_catalog(core);
        let records = catalog.list_folders().unwrap_or_default();
        let mut folders = Vec::new();
        for record in records.iter().filter(|record| !hidden.contains(&record.path)).take(12) {
            let count = catalog
                .images_page(
                    record.id,
                    &SortSpec::default(),
                    &ImageFilter::default(),
                    0,
                    1,
                )
                .map(|page| page.total as i64)
                .unwrap_or(0);
            folders.push(FolderSummary {
                id: record.id,
                name: record
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| record.path.to_string_lossy().into_owned()),
                path: record.path.to_string_lossy().into_owned(),
                count,
            });
        }
        let stats = match catalog.db_stats() {
            Ok(stats) => format!(
                "{} 张图片 · {} 个文件夹\n索引 {} · 收藏 {}",
                stats.images,
                stats.folders,
                size_text(stats.db_bytes),
                stats.favorites
            ),
            Err(error) => format!("索引统计不可用：{error}"),
        };
        (folders, stats)
    };
    core.emit(AppEvent::LibraryUpdated { folders, stats });
}

pub fn request_thumbnail(core: &Arc<AppCore>, meta: &ImageMeta, px: u32, generation: u64) {
    let _ = core.thumbs.send(crate::core::ThumbRequest {
        generation,
        cache_epoch: core.cache.epoch(),
        id: meta.id,
        path: meta.path.clone(),
        size_bytes: meta.size_bytes,
        mtime_ns: meta.modified_ns,
        px,
    });
}

pub fn request_probe(core: &Arc<AppCore>, id: i64, path: PathBuf) {
    let _ = core.probes.send((id, path));
}

pub fn request_render(
    core: &Arc<AppCore>,
    kind: RenderKind,
    id: i64,
    path: PathBuf,
    params: EditParams,
    max_edge: Option<u32>,
) -> u64 {
    queue_render(core, kind, id, path, params, max_edge, None)
}

pub fn queue_render(
    core: &Arc<AppCore>, kind: RenderKind, id: i64, path: PathBuf,
    params: EditParams, max_edge: Option<u32>, conversion: Option<ExportOptions>,
) -> u64 {
    let seq = core.next_seq();
    core.render_seq
        .store(seq, std::sync::atomic::Ordering::SeqCst);
    let _ = core.renders.send(Some(crate::core::RenderRequest {
        conversion,
        seq,
        kind,
        id,
        path,
        params,
        max_edge,
    }));
    seq
}

pub fn load_edit_params(core: &AppCore, id: i64) -> EditParams {
    core.with_image_catalog(id, |catalog| catalog.load_edit_session(id))
        .and_then(Result::ok)
        .map(|session| session.params)
        .unwrap_or_default()
}

pub fn set_favorite(core: &AppCore, id: i64, favorite: bool) -> Result<(), String> {
    core.with_image_catalog(id, |catalog| catalog.set_favorite(id, favorite))
        .ok_or("临时列表已关闭")?
        .map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// Batch tasks
// ---------------------------------------------------------------------------

fn task_state_from_outcome(outcome: &gimage_task_queue::BatchOutcome) -> (i32, &'static str) {
    if outcome.canceled {
        (5, "已取消")
    } else if outcome.failed.is_empty() {
        (2, "已完成")
    } else if outcome.succeeded == 0 {
        (4, "失败")
    } else {
        (3, "部分失败")
    }
}

fn snapshot(
    id: u64,
    title: &str,
    detail: &str,
    state: i32,
    status: &str,
    done: usize,
    total: usize,
    failed: usize,
) -> TaskSnapshot {
    TaskSnapshot {
        id,
        title: title.to_string(),
        detail: detail.to_string(),
        status: status.to_string(),
        progress: if total == 0 { 0.0 } else { (done as f32 / total as f32).min(1.0) },
        done,
        total,
        failed,
        state,
        cancelable: state == 0 || state == 1,
        retryable: state == 3 || state == 4 || state == 5,
    }
}

pub fn export_detail(options: &ExportOptions) -> String {
    let mut parts = vec![options.format.label().to_string()];
    if options.format.supports_quality() {
        parts.push(format!("质量 {}", options.quality));
    }
    if let Some(resize) = options.resize {
        match options.resize_mode {
            gimage_core::ResizeMode::Original => {}
            gimage_core::ResizeMode::FitWidth => parts.push(format!("宽 {} px", resize.max_width)),
            _ => parts.push(format!("{} × {} px", resize.max_width, resize.max_height)),
        }
    }
    parts.push(options.output.label().to_string());
    parts.join(" · ")
}

fn start_task_record(core: &AppCore, id: u64, title: &str, total: usize) -> bool {
    match core.start_task_record(id, title, total) {
        Ok(()) => true,
        Err(error) => {
            let message = format!("无法保存任务恢复记录：{error}");
            core.emit(AppEvent::Task(snapshot(id, title, &message, 4, "无法启动", 0, total, 0)));
            core.toast(message, true);
            false
        }
    }
}

pub fn start_export(core: &Arc<AppCore>, items: Vec<TaskItem>, options: ExportOptions, title: String) {
    if items.is_empty() {
        core.toast("没有可导出的图片", true);
        return;
    }
    let options = options.normalized();
    let total = items.len();
    let detail = export_detail(&options);
    let (task_id, cancel) = core.register_task(
        title.clone(),
        BatchOperation::Export(options.clone()),
        items.clone(),
    );
    core.emit(AppEvent::Task(snapshot(
        task_id, &title, &detail, 0, "排队中", 0, total, 0,
    )));

    let core = Arc::clone(core);
    let _ = std::thread::Builder::new()
        .name(format!("gimage-export-{task_id}"))
        .spawn(move || {
            if !start_task_record(&core, task_id, &title, total) {
                return;
            }
            let concurrency = if options.directory.is_some() {
                1
            } else {
                core.settings_snapshot().concurrency as usize
            };
            let title_for_task = title.clone();
            let detail_for_task = detail.clone();
            let progress_title = title.clone();
            let progress_detail = detail.clone();
            let core_for_work = Arc::clone(&core);
            let core_for_progress = Arc::clone(&core);
            let options_for_work = options.clone();
            let sources: std::collections::HashSet<PathBuf> = if options.output == OutputPolicy::Overwrite {
                items.iter().filter_map(|item| item.path.canonicalize().ok()).collect()
            } else {
                Default::default()
            };

            core.emit(AppEvent::Task(snapshot(
                task_id,
                &title,
                &detail,
                1,
                "进行中",
                0,
                total,
                0,
            )));

            let outcome = run_batch(
                items.clone(),
                BatchOptions { concurrency, retry_failed: false },
                cancel,
                move |item, _token| {
                    let _editing = core_for_work.image_writes.read();
                    let params = if let Some(edits) = &item.edits {
                        if edits.source_version.is_none()
                            || file_version(&item.path).ok() != edits.source_version
                        {
                            return Err("源图已变更，请重新发起导出".into());
                        }
                        edits.params.clone()
                    } else {
                        load_edit_params(&core_for_work, item.id)
                    };
                    let destination = export_destination(&item.path, &options_for_work);
                    if !sources.is_empty()
                        && destination.canonicalize().is_ok_and(|path| sources.contains(&path))
                    {
                        return Err("目标路径是此批次中的源文件".into());
                    }
                    let request = ExportRequest {
                        source: item.path.clone(),
                        destination,
                        params,
                        options: options_for_work.clone(),
                    };
                    match export_image(core_for_work.engine.as_ref(), &request) {
                        Ok(ExportStatus::Written(_)) | Ok(ExportStatus::Skipped) => Ok(()),
                        Err(error) => Err(error.user_message()),
                    }
                },
                move |event| {
                    if let BatchEvent::Progress { done, failed, total } = event {
                        core_for_progress.emit(AppEvent::Task(snapshot(
                            task_id,
                            &progress_title,
                            &progress_detail,
                            1,
                            "进行中",
                            done,
                            total,
                            failed,
                        )));
                    }
                },
            );

            core.finish_task_record(task_id);
            let (state, status) = task_state_from_outcome(&outcome);
            let failed_items: Vec<TaskItem> = outcome
                .failed
                .iter()
                .filter_map(|failure| items.get(failure.index).cloned())
                .collect();
            core.set_task_failed(task_id, failed_items);
            let detail_finish = format!(
                "{} · 成功 {} · 失败 {} · {:.1}s",
                detail_for_task,
                outcome.succeeded,
                outcome.failed.len(),
                outcome.elapsed.as_secs_f32()
            );
            core.emit(AppEvent::Task(snapshot(
                task_id,
                &title_for_task,
                &detail_finish,
                state,
                status,
                outcome.succeeded + outcome.failed.len(),
                outcome.total,
                outcome.failed.len(),
            )));
            match outcome.failed.first() {
                Some(error) => core.toast(
                    format!("{status}：{} 项失败（{}）", outcome.failed.len(), error.error),
                    true,
                ),
                None => core.toast(
                    format!("{status}：{} 张图片", outcome.succeeded),
                    false,
                ),
            }
            let _ = core.emit(AppEvent::CacheStats(cache_stats_text(&core)));
        });
}

pub fn plan_rename<'a>(items: impl IntoIterator<Item = &'a TaskItem>, options: &RenameOptions) -> Result<Vec<Option<String>>, String> {
    let rule = RenameRule::new(options)?;
    let mut reserved = std::collections::HashSet::new();
    items.into_iter().enumerate().map(|(index, item)| {
        let stem = item.path.file_stem().and_then(|value| value.to_str()).unwrap_or_default();
        let extension = item.path.extension().and_then(|value| value.to_str()).unwrap_or_default();
        let Some(name) = rule.target(stem, extension, index as u32) else { return Ok(None); };
        if item.path.file_name().is_some_and(|old| old == std::ffi::OsStr::new(&name)) {
            return Ok(None);
        }
        let candidate = item.path.with_file_name(&name);
        let candidate_stem = candidate.file_stem().and_then(|value| value.to_str()).unwrap_or_default();
        let extension = candidate.extension().and_then(|value| value.to_str()).unwrap_or_default();
        for number in 1..=10_000 {
            let filename = if number == 1 { name.clone() }
                else if extension.is_empty() { format!("{candidate_stem} ({number})") }
                else { format!("{candidate_stem} ({number}).{extension}") };
            let destination = item.path.with_file_name(&filename);
            let key = if cfg!(windows) { destination.to_string_lossy().to_lowercase() }
                else { destination.to_string_lossy().into_owned() };
            if reserved.contains(&key) { continue; }
            let exists = destination.try_exists().map_err(|error| format!("无法检查目标文件名：{error}"))?;
            if !exists || std::fs::canonicalize(&destination).ok().is_some_and(|existing| existing == item.path) {
                reserved.insert(key);
                return Ok(Some(filename));
            }
        }
        Err("无法为个别文件找到可用名称".into())
    }).collect()
}

pub fn start_rename(core: &Arc<AppCore>, items: Vec<TaskItem>, options: RenameOptions) {
    if items.is_empty() {
        core.toast("没有可重命名的文件", true);
        return;
    }
    let options = options.normalized();
    let total = items.len();
    let detail = format!(
        "前缀「{}」· 后缀「{}」· 序号 {} 位，从 {} 开始",
        if options.prefix.is_empty() { "—" } else { &options.prefix },
        if options.suffix.is_empty() { "—" } else { &options.suffix },
        options.padding,
        options.start_index
    );
    let title = format!("批量重命名 {total} 个文件");
    let (task_id, cancel) = core.register_task(
        title.clone(),
        BatchOperation::Rename(options.clone()),
        items.clone(),
    );
    core.emit(AppEvent::Task(snapshot(
        task_id, &title, &detail, 0, "排队中", 0, total, 0,
    )));

    let core_thread = Arc::clone(core);
    let _ = std::thread::Builder::new()
        .name(format!("gimage-rename-{task_id}"))
        .spawn(move || {
            if !start_task_record(&core_thread, task_id, &title, total) {
                return;
            }
            let concurrency = core_thread.settings_snapshot().concurrency as usize;
            let work_title = title.clone();
            let work_detail = detail.clone();
            let event_title = title.clone();
            let event_detail = detail.clone();
            core_thread.emit(AppEvent::Task(snapshot(
                task_id,
                &title,
                &detail,
                1,
                "进行中",
                0,
                total,
                0,
            )));

            let planned_for_work = items.clone();
            let core_for_progress = Arc::clone(&core_thread);
            let outcome = run_batch(
                items,
                BatchOptions { concurrency, retry_failed: true },
                cancel,
                move |item, _| {
                    let new_name = item.rename_target.as_ref().ok_or("缺少预览确认的文件名")?;
                    let destination = item.path.with_file_name(new_name);
                    crate::native::rename_without_replace(&item.path, &destination).map_err(|error| {
                        format!("{} → {new_name}：{error}", item.path.display())
                    })
                },
                move |event| {
                    if let BatchEvent::Progress { done, failed, total } = event {
                        core_for_progress.emit(AppEvent::Task(snapshot(
                            task_id,
                            &event_title,
                            &event_detail,
                            1,
                            "进行中",
                            done,
                            total,
                            failed,
                        )));
                    }
                },
            );

            core_thread.finish_task_record(task_id);
            let (state, status) = task_state_from_outcome(&outcome);
            let failed_items: Vec<TaskItem> = outcome
                .failed
                .iter()
                .filter_map(|failure| planned_for_work.get(failure.index).cloned())
                .collect();
            core_thread.set_task_failed(task_id, failed_items);
            let finish_detail = format!(
                "{} · 成功 {} · 失败 {}",
                work_detail,
                outcome.succeeded,
                outcome.failed.len()
            );
            core_thread.emit(AppEvent::Task(snapshot(
                task_id,
                &work_title,
                &finish_detail,
                state,
                status,
                outcome.succeeded + outcome.failed.len(),
                outcome.total,
                outcome.failed.len(),
            )));
            core_thread.toast(
                format!("{status}：{} 张图片", outcome.succeeded),
                !outcome.failed.is_empty(),
            );
            if outcome.succeeded > 0 {
                core_thread.emit(AppEvent::RefreshRequested);
            }
        });
}
pub fn retry_task(core: &Arc<AppCore>, task_id: u64) {
    let Some((items, operation, title)) = core.task_items(task_id) else {
        return;
    };
    let failed = core.task_failed(task_id);
    let targets: Vec<TaskItem> = if failed.is_empty() { items } else { failed };
    if targets.is_empty() {
        return;
    }
    let retry_title = format!("重试：{title}");
    match operation {
        BatchOperation::Export(options) => start_export(core, targets, options, retry_title),
        BatchOperation::Rename(options) => start_rename(core, targets, options),
    }
}

pub fn cache_stats_text(core: &AppCore) -> String {
    let stats = core.cache.stats();
    format!(
        "内存缓存 {:.1} MB（{} 项）\n磁盘缓存 {}（{} 个文件）\n命中：内存 {} · 磁盘 {} · 未命中 {}",
        stats.memory_bytes as f64 / (1024.0 * 1024.0),
        stats.memory_items,
        size_text(stats.disk_bytes),
        stats.disk_files,
        stats.memory_hits,
        stats.disk_hits,
        stats.misses
    )
}

/// Convert the session filter into a description for the UI footer.
pub fn filter_description(filter: &ImageFilter) -> String {
    let mut parts = Vec::new();
    if !filter.query.trim().is_empty() {
        parts.push(format!("搜索「{}」", filter.query.trim()));
    }
    if filter.favorites_only {
        parts.push("仅收藏".to_string());
    }
    if !filter.formats.is_empty() {
        parts.push(format!("{} 种格式", filter.formats.len()));
    }
    if parts.is_empty() {
        "全部图片".to_string()
    } else {
        parts.join(" · ")
    }
}

pub fn settings_summary(settings: &Settings) -> String {
    format!(
        "并发 {} · 缩略图 {}px · 缓存 {}/{} MB",
        settings.concurrency, settings.thumbnail_px, settings.memory_cache_mb, settings.disk_cache_mb
    )
}

pub fn export_options_from_ui(
    format: ExportFormat,
    quality: u8,
    max_w: u32,
    max_h: u32,
    policy: OutputPolicy,
) -> ExportOptions {
    let resize = if max_w > 0 && max_h > 0 {
        Some(gimage_core::ResizeSpec::new(max_w, max_h))
    } else {
        None
    };
    ExportOptions {
        format,
        quality,
        resize,
        metadata: MetadataPolicy::Strip,
        output: policy,
        ..ExportOptions::default()
    }
    .normalized()
}

pub fn export_format_from_label(label: &str) -> ExportFormat {
    ExportFormat::from_label(label).unwrap_or(ExportFormat::Jpeg)
}

pub fn policy_from_index(index: i32) -> OutputPolicy {
    match index {
        1 => OutputPolicy::SkipExisting,
        2 => OutputPolicy::Overwrite,
        _ => OutputPolicy::AutoRename,
    }
}

pub fn image_meta_for(core: &AppCore, id: i64) -> Option<ImageMeta> {
    core.with_image_catalog(id, |catalog| catalog.get_image(id)).and_then(Result::ok).flatten()
}

/// Format a scan summary for the status bar / toast.
pub fn scan_summary_text(report: &ScanReport) -> String {
    let mut text = format!(
        "{} · 新增 {} · 更新 {} · 移除 {} · {:.0} ms",
        report.total, report.added, report.updated, report.removed, report.elapsed_ms
    );
    if report.canceled {
        text.push_str(" · 已取消");
    }
    if report.truncated {
        text.push_str(" · 达到扫描数量上限，保留未扫描索引");
    }
    if report.errors > 0 {
        text.push_str(&format!(" · {} 项读取失败，保留未扫描索引", report.errors));
    }
    text
}

/// Text shown in the inspector for a missing file.
pub fn missing_text(meta: &ImageMeta) -> String {
    if meta.missing {
        "文件已不存在（索引保留）".to_string()
    } else {
        String::new()
    }
}

pub fn meta_caption(meta: &ImageMeta) -> String {
    meta_line(meta)
}

pub fn folder_display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

pub fn now_timestamp() -> String {
    format_time_ms(now_ms())
}
