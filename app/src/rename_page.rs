use super::*;
use crate::RenameRow;
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub(crate) struct RenameEntry {
    item: TaskItem,
    name_key: String,
    size: u64,
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
}

#[derive(Default)]
pub(super) struct RenameState {
    entries: Arc<Vec<RenameEntry>>,
    session: u64,
    revision: u64,
    due: Option<Instant>,
    preview_running: bool,
    preview_config: Option<(RenameOptions, i32, bool)>,
    loads_pending: usize,
    preview: Vec<(usize, String)>,
}

impl UiState {
    pub(super) fn load_rename(&mut self) {
        if let Some(item) = self.workbench_file.take() {
            self.queue_rename_load(move || Ok(vec![(item.id, item.path)]));
            return;
        }
        let mut ids = self.marked.clone();
        if ids.is_empty() {
            ids.extend(self.selected);
        }
        if ids.is_empty() {
            return;
        }
        let Some(session) = self.core.current_session() else {
            return;
        };
        let core = Arc::clone(&self.core);
        self.queue_rename_load(move || {
            let catalog = core.catalog_for(&session);
            ids.into_iter()
                .filter_map(|id| {
                    catalog
                        .get_image(id)
                        .transpose()
                        .map(|result| result.map(|image| (id, image.path)))
                })
                .collect::<Result<_, _>>()
                .map_err(|error| error.to_string())
        });
    }

    pub(super) fn rename_drop_paths(&mut self, paths: Vec<PathBuf>) {
        let settings = self.core.settings_snapshot();
        self.queue_rename_load(move || {
            super::drop_files::collect_paths(
                paths,
                settings.effective_scan_depth(),
                settings.include_hidden,
                false,
            )
            .map(|paths| paths.into_iter().map(|path| (-1, path)).collect())
        });
    }

    pub(super) fn rename_add_files(&mut self) {
        let app = self.ui.global::<App>();
        if self.page != 6 || app.get_rename_loading() {
            return;
        }
        let mut dialog = rfd::FileDialog::new().set_title("添加要重命名的文件");
        if let Some(session) = self.core.current_session() {
            dialog = dialog.set_directory(session.path);
        }
        if let Some(paths) = dialog.pick_files() {
            self.queue_rename_load(move || Ok(paths.into_iter().map(|path| (-1, path)).collect()));
        }
    }

    pub(super) fn rename_open_path(&mut self, path: PathBuf) {
        self.queue_rename_load(move || {
            if path.is_dir() {
                let mut files = Vec::new();
                for entry in std::fs::read_dir(path).map_err(|error| error.to_string())? {
                    let entry = entry.map_err(|error| error.to_string())?;
                    if entry
                        .file_type()
                        .map_err(|error| error.to_string())?
                        .is_file()
                    {
                        files.push((-1, entry.path()));
                    }
                }
                Ok(files)
            } else {
                Ok(vec![(-1, path)])
            }
        });
    }

    fn queue_rename_load(
        &mut self,
        paths: impl FnOnce() -> Result<Vec<(i64, PathBuf)>, String> + Send + 'static,
    ) {
        let token = self.rename.session;
        let core = Arc::clone(&self.core);
        let app = self.ui.global::<App>();
        self.rename.loads_pending += 1;
        app.set_rename_loading(true);
        let started = std::thread::Builder::new()
            .name("gimage-rename-list".into())
            .spawn(move || {
                let entries = paths().and_then(|paths| {
                    paths
                        .into_iter()
                        .map(|(id, path)| {
                            let read_error =
                                |error| format!("无法读取 {}：{error}", display_path(&path));
                            let resolved = path.canonicalize().map_err(read_error)?;
                            let metadata = resolved.metadata().map_err(read_error)?;
                            if !metadata.is_file() {
                                return Err(format!("所选路径不是文件：{}", display_path(&path)));
                            }
                            Ok(RenameEntry {
                                name_key: file_name_of(&resolved).to_lowercase(),
                                size: metadata.len(),
                                created: metadata.created().ok(),
                                modified: metadata.modified().ok(),
                                item: TaskItem {
                                    id,
                                    path: resolved,
                                    edits: None,
                                    rename_target: None,
                                },
                            })
                        })
                        .collect()
                });
                core.emit(AppEvent::RenameLoaded { token, entries });
            });
        if let Err(error) = started {
            self.rename_loaded(token, Err(format!("无法读取文件：{error}")));
        }
    }

    pub(super) fn rename_loaded(&mut self, token: u64, entries: Result<Vec<RenameEntry>, String>) {
        if self.page != 6 || self.rename.session != token {
            return;
        }
        let app = self.ui.global::<App>();
        self.rename.loads_pending = self.rename.loads_pending.saturating_sub(1);
        app.set_rename_loading(self.rename.loads_pending > 0);
        match entries {
            Ok(entries) => {
                let mut paths: HashSet<_> = self
                    .rename
                    .entries
                    .iter()
                    .map(|entry| entry.item.path.clone())
                    .collect();
                let combined = Arc::make_mut(&mut self.rename.entries);
                combined.extend(
                    entries
                        .into_iter()
                        .filter(|entry| paths.insert(entry.item.path.clone())),
                );
                app.set_rename_target_count(combined.len() as i32);
                self.schedule_rename_preview();
            }
            Err(error) => self.toast(error, true),
        }
    }

    fn rename_options(&self) -> Result<RenameOptions, String> {
        let app = self.ui.global::<App>();
        let numbered = app.get_rename_numbered();
        let start_index = if numbered {
            app.get_rename_start()
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|value| (1..=999_999).contains(value))
                .ok_or("起始序号请输入 1 到 999999 之间的整数")?
        } else {
            1
        };
        Ok(RenameOptions {
            prefix: app.get_rename_prefix().into(),
            suffix: app.get_rename_suffix().into(),
            extension: app.get_rename_extension().into(),
            find: app.get_rename_find().into(),
            replace: app.get_rename_replace().into(),
            start_index,
            padding: app.get_rename_padding().clamp(1, 9) as u8,
            regex: app.get_rename_regex(),
            numbered,
        })
    }

    pub(super) fn schedule_rename_preview(&mut self) {
        if self.page != 6 || self.rename.entries.is_empty() {
            return;
        }
        self.rename.revision += 1;
        self.rename.due = Some(Instant::now() + Duration::from_millis(90));
        self.ui.global::<App>().set_rename_preview_pending(true);
    }

    pub(super) fn tick_rename(&mut self, now: Instant) {
        if self.page != 6
            || self.rename.preview_running
            || !self.rename.due.is_some_and(|at| now >= at)
        {
            return;
        }
        self.rename.due = None;
        let entries = Arc::clone(&self.rename.entries);
        let options = self.rename_options();
        let app = self.ui.global::<App>();
        let key = app.get_rename_sort();
        let descending = app.get_rename_sort_descending();
        self.rename.preview_config = options
            .as_ref()
            .ok()
            .cloned()
            .map(|options| (options, key, descending));
        let (token, revision) = (self.rename.session, self.rename.revision);
        let core = Arc::clone(&self.core);
        let started = std::thread::Builder::new()
            .name("gimage-rename-preview".into())
            .spawn(move || {
                let mut order: Vec<_> = (0..entries.len()).collect();
                order.sort_unstable_by(|&a, &b| {
                    let (a, b) = (&entries[a], &entries[b]);
                    let time =
                        |left: Option<SystemTime>, right: Option<SystemTime>| match (left, right) {
                            (Some(left), Some(right)) => {
                                if descending {
                                    right.cmp(&left)
                                } else {
                                    left.cmp(&right)
                                }
                            }
                            _ => left.is_none().cmp(&right.is_none()),
                        };
                    let direction = |order: std::cmp::Ordering| {
                        if descending { order.reverse() } else { order }
                    };
                    let primary = match key {
                        1 => direction(a.size.cmp(&b.size)),
                        2 => time(a.created, b.created),
                        3 => time(a.modified, b.modified),
                        _ => direction(a.name_key.cmp(&b.name_key)),
                    };
                    primary
                        .then_with(|| a.name_key.cmp(&b.name_key))
                        .then_with(|| a.item.path.cmp(&b.item.path))
                });
                let targets = options.and_then(|options| {
                    actions::plan_rename(order.iter().map(|&index| &entries[index].item), &options)
                });
                core.emit(AppEvent::RenamePreview {
                    token,
                    revision,
                    order,
                    targets,
                });
            });
        self.rename.preview_running = started.is_ok();
        if let Err(error) = started {
            app.set_rename_preview_pending(false);
            app.set_rename_error(format!("无法生成重命名预览：{error}").into());
        }
    }

    pub(super) fn rename_preview_ready(
        &mut self,
        token: u64,
        revision: u64,
        order: Vec<usize>,
        targets: Result<Vec<Option<String>>, String>,
    ) {
        if self.page != 6 || self.rename.session != token {
            return;
        }
        self.rename.preview_running = false;
        if self.rename.revision != revision {
            return;
        }
        let app = self.ui.global::<App>();
        app.set_rename_preview_pending(false);
        self.rename.preview.clear();
        let targets = match targets {
            Ok(targets) => {
                app.set_rename_error("".into());
                targets
            }
            Err(error) => {
                app.set_rename_error(error.into());
                vec![None; order.len()]
            }
        };
        let rows: Vec<_> = order
            .into_iter()
            .zip(targets)
            .map(|(index, target)| {
                let path = &self.rename.entries[index].item.path;
                let original = file_name_of(path);
                let changed = target.is_some();
                let result = if let Some(target) = target {
                    self.rename.preview.push((index, target.clone()));
                    target
                } else {
                    original.clone()
                };
                RenameRow {
                    original: original.into(),
                    result: result.into(),
                    changed,
                    path: display_path(path).into(),
                }
            })
            .collect();
        app.set_rename_changed_count(self.rename.preview.len() as i32);
        app.set_rename_rows(ModelRc::new(VecModel::from(rows)));
    }

    pub(super) fn leave_rename_if_open(&mut self) {
        if self.page != 6 {
            return;
        }
        self.rename = RenameState {
            session: self.rename.session + 1,
            ..Default::default()
        };
        let app = self.ui.global::<App>();
        app.set_rename_rows(ModelRc::default());
        app.set_rename_loading(false);
        app.set_rename_preview_pending(false);
        app.set_rename_error("".into());
        app.set_rename_target_count(0);
        app.set_rename_changed_count(0);
    }

    pub(super) fn confirm_rename_plan(&mut self) {
        let app = self.ui.global::<App>();
        if self.page != 6
            || self.pending_confirm.is_some()
            || self.rename.due.is_some()
            || app.get_rename_loading()
            || app.get_rename_preview_pending()
            || !app.get_rename_error().is_empty()
        {
            return;
        }
        let Ok(options) = self.rename_options() else {
            return;
        };
        let Some((preview_options, sort, descending)) = &self.rename.preview_config else {
            return;
        };
        if *preview_options != options
            || *sort != app.get_rename_sort()
            || *descending != app.get_rename_sort_descending()
        {
            return;
        }
        let items: Vec<_> = self
            .rename
            .preview
            .iter()
            .map(|(index, name)| {
                let mut item = self.rename.entries[*index].item.clone();
                item.rename_target = Some(name.clone());
                item
            })
            .collect();
        if items.is_empty() {
            return;
        }
        app.set_confirm_title(format!("重命名 {} 个文件？", items.len()).into());
        app.set_confirm_body("将按预览修改磁盘上的文件名。".into());
        app.set_confirm_label("开始重命名".into());
        app.set_confirm_danger(true);
        self.pending_confirm = Some(PendingConfirm::Rename(items, options));
        app.set_confirm_open(true);
    }
}
