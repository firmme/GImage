use super::*;
use crate::core::FolderSession;
use gimage_catalog::{Catalog, ScannedFile};
use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};

pub fn wire(window: &MainWindow, queue: ActionQueue) {
    let pending = Rc::new(RefCell::new(Vec::new()));
    let timer = slint::Timer::default();
    window.window().on_winit_window_event(move |_, event| {
        let WindowEvent::DroppedFile(path) = event else {
            return EventResult::Propagate;
        };
        pending.borrow_mut().push(path.clone());
        let pending = Rc::clone(&pending);
        let queue = Rc::clone(&queue);
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(45),
            move || {
                let paths = std::mem::take(&mut *pending.borrow_mut());
                push(&queue, move |state| state.drop_paths(paths));
            },
        );
        EventResult::PreventDefault
    });
}

pub(crate) fn collect_paths(
    inputs: Vec<PathBuf>,
    depth: u8,
    include_hidden: bool,
    images_only: bool,
) -> Result<Vec<PathBuf>, String> {
    const MAX_FILES: usize = 200_000;
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut failed = None;
    let mut pending: Vec<_> = inputs.into_iter().rev().map(|path| (path, 0)).collect();
    while let Some((path, level)) = pending.pop() {
        let metadata = if level == 0 {
            path.metadata()
        } else {
            path.symlink_metadata()
        };
        let metadata = match metadata {
            Ok(metadata) => metadata,
            Err(error) => {
                failed.get_or_insert_with(|| format!("无法读取 {}：{error}", display_path(&path)));
                continue;
            }
        };
        if level > 0
            && !include_hidden
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if metadata.is_dir() && level <= depth {
            if let Ok(dir) = path.canonicalize()
                && seen.insert(dir.clone())
            {
                match std::fs::read_dir(&dir) {
                    Ok(entries) => pending.extend(
                        entries
                            .filter_map(Result::ok)
                            .map(|entry| (entry.path(), level + 1)),
                    ),
                    Err(error) => {
                        failed.get_or_insert_with(|| {
                            format!("无法读取 {}：{error}", display_path(&path))
                        });
                    }
                }
            }
        } else if metadata.is_file() && (!images_only || ImageFormat::from_path(&path).is_some()) {
            match path.canonicalize() {
                Ok(path) if seen.insert(path.clone()) => {
                    if files.len() == MAX_FILES {
                        return Err("拖入文件超过 20 万个，请分批添加。".into());
                    }
                    files.push(path);
                }
                Ok(_) => {}
                Err(error) => {
                    failed.get_or_insert_with(|| {
                        format!("无法读取 {}：{error}", display_path(&path))
                    });
                }
            }
        }
    }
    if files.is_empty() {
        return Err(failed.unwrap_or_else(|| {
            if images_only {
                "拖入的项目中没有可识别的图片".into()
            } else {
                "拖入的项目中没有可添加的文件".into()
            }
        }));
    }
    Ok(files)
}

impl UiState {
    pub(super) fn drop_paths(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() || self.saving.is_some() || self.pending_confirm.is_some() {
            return;
        }
        match self.page {
            0..=2 => {
                if self.page == 2
                    && self.confirm_editor_leave(EditorDestination::Drop(paths.clone()))
                {
                    return;
                }
                self.browse_dropped_paths(paths);
            }
            5 => self.conversion_drop_paths(paths),
            6 => self.rename_drop_paths(paths),
            _ => {}
        }
    }

    fn browse_dropped_paths(&mut self, paths: Vec<PathBuf>) {
        self.drop_request = self.drop_request.wrapping_add(1);
        let request = self.drop_request;
        let page = self.page;
        let generation = self.generation;
        let core = Arc::clone(&self.core);
        let settings = core.settings_snapshot();
        let sort = self.sort.clone();
        let started = std::thread::Builder::new()
            .name("gimage-drop-browser".into())
            .spawn(move || {
                let preview = paths.len() == 1 && paths[0].is_file();
                let result = collect_paths(
                    paths,
                    settings.effective_scan_depth(),
                    settings.include_hidden,
                    true,
                )
                .and_then(|paths| {
                    let files: Vec<_> = paths
                        .into_iter()
                        .filter_map(|path| {
                            let (size_bytes, modified) = file_version(&path).ok()?;
                            Some(ScannedFile {
                                format: ImageFormat::from_path(&path)?,
                                modified_ns: modified
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map_or(0, |time| time.as_nanos().min(i64::MAX as u128) as i64),
                                size_bytes,
                                path,
                            })
                        })
                        .collect();
                    let directory = files
                        .first()
                        .and_then(|file| file.path.parent())
                        .map(PathBuf::from)
                        .ok_or("拖入的图片已不可访问")?;
                    let id_offset = core
                        .reserve_temp_image_ids(files.len())
                        .ok_or("临时列表编号已用完，请重新启动应用")?;
                    let (catalog, id) = Catalog::from_file_list(files, id_offset)
                        .map_err(|error| error.to_string())?;
                    let filter = ImageFilter::default();
                    let first = catalog
                        .images_page(id, &sort, &filter, 0, actions::PAGE_SIZE)
                        .map_err(|error| error.to_string())?;
                    Ok((
                        FolderSession {
                            id,
                            path: directory,
                            generation: 0,
                            sort,
                            filter,
                            total: first.total,
                            temporary_catalog: Some(Arc::new(parking_lot::Mutex::new(catalog))),
                        },
                        first.items,
                    ))
                });
                core.emit(AppEvent::DroppedBrowser {
                    request,
                    generation,
                    page,
                    preview,
                    result,
                });
            });
        if let Err(error) = started {
            self.toast(format!("无法读取拖入的文件：{error}"), true);
        }
    }

    pub(super) fn dropped_browser_ready(
        &mut self,
        request: u64,
        generation: u64,
        page: i32,
        preview: bool,
        result: Result<(FolderSession, Vec<ImageMeta>), String>,
    ) {
        if self.drop_request != request
            || self.generation != generation
            || self.page != page
            || self.pending_confirm.is_some()
            || self.saving.is_some()
        {
            return;
        }
        if self.page == 2 && !self.edit.params.is_identity() {
            self.toast("读取文件期间编辑有变更，请保存后重新拖入。".into(), false);
            return;
        }
        let (mut session, items) = match result {
            Ok(result) => result,
            Err(error) => {
                self.toast(error, true);
                return;
            }
        };
        if let Some(cancel) = self.core.scan_cancel.lock().take() {
            cancel.cancel();
        }
        self.generation = self.core.next_generation();
        session.generation = self.generation;
        let total = session.total;
        self.core.set_session(Some(session));
        *self.core.folder_watch.lock() = None;
        self.pending_reload = None;
        self.filter = ImageFilter::default();
        self.set_marked(HashSet::new());
        self.selection_anchor = None;
        self.clear_selection();
        self.clear_preview_buffer();
        self.set_page(0);
        self.preview = PreviewState {
            seq: 0,
            id: -1,
            width: 0,
            height: 0,
            detail_requested: false,
        };
        let app = self.ui.global::<App>();
        app.set_preview_id(-1);
        app.set_temporary_browser(true);
        app.set_folder_name("临时文件列表".into());
        app.set_folder_path("".into());
        app.set_scanning(false);
        app.set_search_text("".into());
        app.set_favorites_only(false);
        self.rebuild_formats();
        let first = items.first().map(|entry| entry.id);
        self.handle_event(AppEvent::PageReady {
            generation: self.generation,
            offset: 0,
            total,
            items,
        });
        if preview && let Some(id) = first {
            self.open_preview(id);
        }
    }
}
