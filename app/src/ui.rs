use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gimage_core::{
    Annotation, AnnotationKind, CropRect, EditParams, ExportFormat, ExportOptions, ImageFilter, ImageFormat, ImageMeta, MarkPoint, MarkTool,
    OutputPolicy, RenameOptions, Settings, SortSpec,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::actions;
use crate::core::{AppCore, ExportEdits, TaskItem};
use crate::events::{AppEvent, RenderKind, TaskSnapshot};
use crate::marks::{self, Draft};
use crate::preview_buffer::PREVIEW_EDGE;
use crate::updater;
use crate::util::*;
use crate::{App, FolderRow, FormatChip, GridRow, ImageCell, MainWindow, TaskRow, Theme};

#[path = "conversion.rs"]
mod conversion;
use conversion::ConversionState;
#[path = "drop_files.rs"]
pub(crate) mod drop_files;
#[path = "rename_page.rs"]
pub(crate) mod rename_page;
use rename_page::RenameState;

pub struct Entry {
    pub meta: ImageMeta,
    pub thumb: Option<slint::Image>,
    pub state: i32,
    pub error: Option<String>,
    pub row: usize,
    pub col: usize,
}

struct PreviewState {
    seq: u64,
    id: i64,
    width: u32,
    height: u32,
    detail_requested: bool,
}

#[derive(Clone)]
struct EditState {
    id: i64,
    params: EditParams,
    history: Vec<EditParams>,
    cursor: usize,
    seq: u64,
    base_width: u32,
    base_height: u32,
    last_push: Instant,
    path: PathBuf,
    version: Option<FileVersion>,
}

impl EditState {
    fn export_edits(&self) -> ExportEdits {
        ExportEdits { params: self.params.clone(), source_version: self.version }
    }

    fn empty() -> Self {
        Self {
            id: -1,
            params: EditParams::default(),
            history: vec![EditParams::default()],
            cursor: 0,
            seq: 0,
            base_width: 0,
            base_height: 0,
            last_push: Instant::now() - Duration::from_secs(5),
            path: PathBuf::new(),
            version: None,
        }
    }
}

enum PendingConfirm {
    Update(updater::Update),
    Export(Vec<TaskItem>, ExportOptions),
    Conversion(ExportOptions),
    Rename(Vec<TaskItem>, RenameOptions),
    SaveEdit,
    Exit,
    LeaveEditor(EditorDestination),
}

enum EditorDestination {
    Page(i32),
    Preview(i64),
    Editor(i64),
    Folder(PathBuf),
    Image(PathBuf),
    Drop(Vec<PathBuf>),
    Exit,
}

pub struct UiState {
    pub core: Arc<AppCore>,
    ui: MainWindow,
    rows: Rc<VecModel<GridRow>>,
    task_model: Rc<VecModel<TaskRow>>,
    folder_model: Rc<VecModel<FolderRow>>,
    format_model: Rc<VecModel<FormatChip>>,
    entries: Vec<Entry>,
    requested: HashSet<(i64, u32)>,
    total: u64,
    loaded: u32,
    loading_more: bool,
    columns: usize,
    visible_rows: Range<usize>,
    generation: u64,
    page: i32,
    selected: Option<i64>,
    marked: HashSet<i64>,
    mark_request: u64,
    drop_request: u64,
    selection_anchor: Option<i64>,
    conversion: ConversionState,
    rename: RenameState,
    workbench_file: Option<TaskItem>,
    sort: SortSpec,
    filter: ImageFilter,
    preview: PreviewState,
    edit: EditState,
    drafts: HashMap<i64, EditState>,
    saving: Option<i64>,
    save_destination: Option<EditorDestination>,
    tasks: Vec<TaskSnapshot>,
    pending_confirm: Option<PendingConfirm>,
    toast_until: Option<Instant>,
    pending_reload: Option<Instant>,
    pending_edit_render: Option<Instant>,
    pending_columns: Option<Instant>,
    clipboard: Option<arboard::Clipboard>,
    mark_draft: Draft,
}

impl UiState {
    pub fn new(core: Arc<AppCore>, ui: &MainWindow) -> Self {
        let settings = core.settings_snapshot();
        let mut state = Self {
            core,
            ui: ui.clone_strong(),
            rows: Rc::new(VecModel::from(Vec::<GridRow>::new())),
            task_model: Rc::new(VecModel::from(Vec::<TaskRow>::new())),
            folder_model: Rc::new(VecModel::from(Vec::<FolderRow>::new())),
            format_model: Rc::new(VecModel::from(Vec::<FormatChip>::new())),
            entries: Vec::new(),
            requested: HashSet::new(),
            total: 0,
            loaded: 0,
            loading_more: false,
            columns: 4,
            visible_rows: 0..0,
            generation: 0,
            page: 0,
            selected: None,
            marked: HashSet::new(),
            mark_request: 0,
            drop_request: 0,
            selection_anchor: None,
            conversion: ConversionState::default(),
            rename: RenameState::default(),
            workbench_file: None,
            sort: settings.sort.clone(),
            filter: ImageFilter::default(),
            preview: PreviewState { seq: 0, id: -1, width: 0, height: 0, detail_requested: false },
            edit: EditState::empty(),
            drafts: HashMap::new(),
            saving: None,
            save_destination: None,
            tasks: Vec::new(),
            pending_confirm: None,
            toast_until: None,
            pending_reload: None,
            pending_edit_render: None,
            pending_columns: None,
            clipboard: None,
            mark_draft: Draft::default(),
        };
        state.install(ui);
        state
    }

    fn app(&self) -> Option<App<'_>> {
        Some(self.ui.global::<App>())
    }

    fn install(&mut self, ui: &MainWindow) {
        let app = ui.global::<App>();
        let settings = self.core.settings_snapshot();

        app.set_grid_rows(ModelRc::new(self.rows.clone()));
        app.set_resolution_presets(conversion::resolution_presets());
        app.set_preview_base_edge(PREVIEW_EDGE as i32);
        app.set_tasks(ModelRc::new(self.task_model.clone()));
        app.set_recent_folders(ModelRc::new(self.folder_model.clone()));
        app.set_format_chips(ModelRc::new(self.format_model.clone()));

        let theme = ui.global::<Theme>();
        #[cfg(windows)]
        if std::env::var_os("WINDIR").is_some_and(|root| {
            PathBuf::from(root).join("Fonts/NotoSansSC-VF.ttf").is_file()
        }) {
            theme.set_font("Noto Sans SC".into());
        }
        theme.set_dark(matches!(settings.theme, gimage_core::Theme::Dark));
        theme.set_low_power(settings.low_power);

        app.set_page(0);
        app.set_sort_index(sort_index_from_key(settings.sort.key));
        app.set_sort_descending(matches!(
            settings.sort.direction,
            gimage_core::SortDirection::Descending
        ));
        app.set_settings_dark(matches!(settings.theme, gimage_core::Theme::Dark));
        app.set_settings_thumb_px(settings.thumbnail_px as i32);
        app.set_settings_concurrency(settings.concurrency as i32);
        app.set_settings_mem_mb(settings.memory_cache_mb as i32);
        app.set_settings_disk_mb(settings.disk_cache_mb as i32);
        app.set_settings_recursive(settings.recursive_scan);
        app.set_scan_depth_limit(i32::from(settings.effective_scan_depth()));
        let [browse, convert, rename] = settings.file_menu_positions.map(i32::from);
        app.set_context_browse(browse);
        app.set_context_convert(convert);
        app.set_context_rename(rename);
        app.set_settings_context_browse(browse);
        app.set_settings_context_convert(convert);
        app.set_settings_context_rename(rename);
        app.set_settings_explorer_menu(settings.explorer_menu_enabled);
        app.set_settings_hidden(settings.include_hidden);
        app.set_settings_low_power(settings.low_power);

        let caps = &self.core.capabilities;
        let formats: Vec<ExportFormat> = ExportFormat::all()
            .iter()
            .copied()
            .filter(|format| caps.can_write(&ImageFormat::from_extension(format.extension()).unwrap_or(ImageFormat::Other(String::new()))))
            .collect();
        let format_labels: Vec<slint::SharedString> = formats
            .iter()
            .map(|format| format.label().into())
            .collect();
        app.set_export_formats(ModelRc::new(VecModel::from(format_labels)));
        app.set_export_quality(0.9);
        app.set_export_policy(0);

        app.set_engine_info(
            format!(
                "{} {} · 可读 {} 种格式 · 可写 {} 种格式",
                caps.engine_name,
                caps.engine_version,
                caps.readable.len(),
                caps.writable.len()
            )
            .into(),
        );
        app.set_engine_detail(
            format!(
                "图像引擎：{} {}\n可读：{}\n可写：{}\n单帧显示：{}",
                caps.engine_name,
                caps.engine_version,
                caps.readable.iter().map(|f| f.label()).collect::<Vec<_>>().join(" / "),
                caps.writable.iter().map(|f| f.label()).collect::<Vec<_>>().join(" / "),
                caps.single_frame_only.iter().map(|f| f.label()).collect::<Vec<_>>().join(" / ")
            )
            .into(),
        );
        app.set_version_info(
            format!(
                "GImage 0.1.0 · Rust + Slint 原生 UI\n构建时间：{}",
                option_env!("GIMAGE_BUILD_DATE").unwrap_or("本地构建")
            )
            .into(),
        );
        app.set_library_stats("尚未建立索引".into());
        app.set_scan_status("选择一个文件夹开始浏览".into());
    }

    // -----------------------------------------------------------------------
    // Rows and thumbnails
    // -----------------------------------------------------------------------
    fn cell_for(&self, entry: &Entry) -> ImageCell {
        ImageCell {
            id: entry.meta.id as i32,
            name: file_name_of(&entry.meta.path).into(),
            marked: self.marked.contains(&entry.meta.id),
            thumb: entry.thumb.clone().unwrap_or_default(),
            state: entry.state,
            favorite: entry.meta.favorite,
        }
    }

    fn rebuild_rows(&mut self) {
        let columns = self.columns.max(1);
        for (index, entry) in self.entries.iter_mut().enumerate() {
            entry.row = index / columns;
            entry.col = index % columns;
            if !self.visible_rows.contains(&entry.row) && entry.thumb.take().is_some() {
                entry.state = 0;
            }
        }
        let mut rows = Vec::new();
        for chunk in self.entries.chunks(columns) {
            let cells: Vec<ImageCell> = chunk.iter().map(|entry| self.cell_for(entry)).collect();
            rows.push(GridRow {
                cells: ModelRc::new(VecModel::from(cells)),
            });
        }
        self.rows.set_vec(rows);
        if let Some(app) = self.app() {
            app.set_grid_rows(ModelRc::new(self.rows.clone()));
        }
        self.request_visible_thumbnails();
    }

    fn refresh_row(&mut self, entry_index: usize) {
        if let Some(entry) = self.entries.get(entry_index)
            && let Some(row) = self.rows.row_data(entry.row)
        {
            row.cells.set_row_data(entry.col, self.cell_for(entry));
        }
    }

    fn reset_thumbnails(&mut self) {
        self.requested.clear();
        for entry in &mut self.entries {
            entry.thumb = None;
            entry.state = 0;
            entry.error = None;
        }
        self.rebuild_rows();
        if let Some(app) = self.app() {
            app.set_sel_thumb(slint::Image::default());
        }
    }

    fn entry_index(&self, id: i64) -> Option<usize> {
        self.entries.iter().position(|entry| entry.meta.id == id)
    }

    fn request_thumb_at(&mut self, index: usize) {
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        if entry.state != 0 || entry.thumb.is_some() {
            return;
        }
        let px = self.core.settings_snapshot().thumbnail_px;
        let key = (entry.meta.id, px);
        if !self.requested.insert(key) {
            return;
        }
        let meta = entry.meta.clone();
        actions::request_thumbnail(&self.core, &meta, px, self.generation);
    }

    fn request_visible_thumbnails(&mut self) {
        if self.page != 0 { return; }
        let start = self.entries.partition_point(|entry| entry.row < self.visible_rows.start);
        let end = self.entries.partition_point(|entry| entry.row < self.visible_rows.end);
        for index in start..end {
            self.request_thumb_at(index);
        }
    }

    fn update_thumbnail_viewport(&mut self, rows: Range<usize>) {
        if self.visible_rows == rows { return; }
        let previous = std::mem::replace(&mut self.visible_rows, rows);
        let start = self.entries.partition_point(|entry| entry.row < previous.start);
        let end = self.entries.partition_point(|entry| entry.row < previous.end);
        for index in start..end {
            let entry = &mut self.entries[index];
            if !self.visible_rows.contains(&entry.row) && entry.thumb.take().is_some() {
                entry.state = 0;
                self.refresh_row(index);
            }
        }
        self.request_visible_thumbnails();
    }

    // -----------------------------------------------------------------------
    // Selection & pages
    // -----------------------------------------------------------------------
    fn select(&mut self, id: i64) {
        let meta = self
            .entries
            .iter()
            .find(|entry| entry.meta.id == id)
            .map(|entry| entry.meta.clone())
            .or_else(|| actions::image_meta_for(&self.core, id));
        let Some(meta) = meta else {
            return;
        };
        self.selected = Some(id);
        let thumb = self
            .entry_index(id)
            .and_then(|index| self.entries[index].thumb.clone());
        if meta.width.is_none() && !meta.missing {
            actions::request_probe(&self.core, id, meta.path.clone());
        }
        let Some(app) = self.app() else { return };
        app.set_selected_id(id as i32);
        app.set_sel_name(file_name_of(&meta.path).into());
        app.set_sel_dir(dir_display(&meta).into());
        app.set_sel_path(display_path(&meta.path).into());
        app.set_sel_format(meta.format.label().into());
        app.set_sel_dims(
            meta.dimension_text()
                .unwrap_or_else(|| "尺寸未知".to_string())
                .into(),
        );
        app.set_sel_size(size_text(meta.size_bytes).into());
        app.set_sel_modified(format_time_ns(meta.modified_ns).into());
        app.set_sel_favorite(meta.favorite);
        app.set_sel_missing(meta.missing);
        app.set_sel_status(actions::missing_text(&meta).into());
        if let Some(thumb) = thumb {
            app.set_sel_thumb(thumb);
        }
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        if let Some(app) = self.app() {
            app.set_selected_id(-1);
            app.set_sel_thumb(slint::Image::default());
        }
    }

    fn set_marked(&mut self, marked: HashSet<i64>) {
        self.mark_request = self.mark_request.wrapping_add(1);
        let changed: HashSet<_> = self.marked.symmetric_difference(&marked).copied().collect();
        self.marked = marked;
        for entry in &self.entries {
            if changed.contains(&entry.meta.id)
                && let Some(row) = self.rows.row_data(entry.row)
            {
                row.cells.set_row_data(entry.col, self.cell_for(entry));
            }
        }
        self.ui.global::<App>().set_marked_count(self.marked.len() as i32);
    }

    fn select_all_browser(&mut self) {
        if self.page != 0 || self.ui.global::<App>().get_scanning() {
            return;
        }
        let Some(session) = self.core.current_session().filter(|session| session.id > 0) else { return };
        self.mark_request = self.mark_request.wrapping_add(1);
        let request = self.mark_request;
        let generation = self.generation;
        let core = Arc::clone(&self.core);
        let started = std::thread::Builder::new().name("gimage-select-all".into()).spawn(move || {
            let ids = core.catalog_for(&session).image_ids(session.id, &session.filter).map_err(|error| error.to_string());
            core.emit(AppEvent::BrowserAllSelected { generation, request, ids });
        });
        if let Err(error) = started {
            self.toast(format!("无法全选：{error}"), true);
        }
    }

    fn select_browser_cell(&mut self, id: i64, mode: i32) {
        if self.page != 0 { return; }
        let mut marked = self.marked.clone();
        match mode {
            1 | 3 => {
                if mode == 1 && marked.is_empty() && self.selected != Some(id) {
                    marked.extend(self.selected);
                }
                if !marked.insert(id) { marked.remove(&id); }
                self.selection_anchor = Some(id);
            }
            2 => {
                let anchor = self.selection_anchor.or(self.selected).and_then(|anchor| self.entry_index(anchor));
                if let (Some(anchor), Some(current)) = (anchor, self.entry_index(id)) {
                    marked.extend(self.entries[anchor.min(current)..=anchor.max(current)]
                        .iter().map(|entry| entry.meta.id));
                } else {
                    marked.insert(id);
                    self.selection_anchor = Some(id);
                }
            }
            4 if self.selection_anchor.is_some() => {}
            _ => {
                self.selection_anchor = Some(id);
            }
        }
        self.set_marked(marked);
        self.select(id);
    }

    fn extend_browser_selection(&mut self, step: i32) {
        if self.page != 0 { return; }
        let Some(index) = self.selected.and_then(|id| self.entry_index(id)) else { return; };
        let target = (index as i64 + i64::from(step)).clamp(0, self.entries.len() as i64 - 1) as usize;
        self.select_browser_cell(self.entries[target].meta.id, 2);
    }

    fn set_preview_window(&self, id: i64, path: PathBuf) {
        let mut neighbors = Vec::new();
        if let Some(session) = self.core.current_session().filter(|session| session.id > 0) {
            let catalog = self.core.catalog_for(&session);
            for delta in [1, -1] {
                if let Ok(Some(meta)) =
                    catalog.neighbor(session.id, &session.sort, &session.filter, id, delta)
                {
                    neighbors.push(meta.path);
                }
            }
        }
        self.core.preview_buffer.set_window(path, neighbors);
    }

    fn clear_preview_buffer(&mut self) {
        self.core.preview_buffer.clear();
        if let Some(app) = self.app() {
            app.set_preview_image(slint::Image::default());
        }
    }

    fn open_preview(&mut self, id: i64) {
        if self.saving.is_some() || self.confirm_editor_leave(EditorDestination::Preview(id)) {
            return;
        }
        let Some(meta) = self
            .entries
            .iter()
            .find(|entry| entry.meta.id == id)
            .map(|entry| entry.meta.clone())
            .or_else(|| actions::image_meta_for(&self.core, id))
        else {
            return;
        };
        if self.page == 2 && self.saving.is_none() {
            self.park_editor();
        }
        self.leave_conversion_if_open();
        self.leave_rename_if_open();
        self.set_preview_window(id, meta.path.clone());
        let seq = actions::request_render(
            &self.core,
            RenderKind::Preview,
            id,
            meta.path.clone(),
            EditParams::default(),
            Some(PREVIEW_EDGE),
        );
        self.preview = PreviewState { seq, id, width: 0, height: 0, detail_requested: false };
        self.page = 1;
        self.update_thumbnail_viewport(0..0);
        self.select(id);
        let position = self.core.current_session().and_then(|session| {
            self.core.catalog_for(&session).image_position(session.id, &session.sort, &session.filter, id).ok().flatten()
        }).or_else(|| self.entry_index(id).map(|index| index as u64 + 1)).unwrap_or(1);
        let total = self.total.max(position).min(i32::MAX as u64) as i32;
        let title = file_name_of(&meta.path);
        let Some(app) = self.app() else { return };
        app.set_preview_w(0);
        app.set_preview_h(0);
        app.set_exif_open(false);
        app.set_exif_text("正在读取…".into());
        app.set_preview_id(id as i32);
        app.set_preview_loading(true);
        app.set_preview_title(title.into());
        let (w, h) = oriented_dimensions(&meta);
        app.set_preview_source_w(w as i32);
        app.set_preview_source_h(h as i32);
        app.set_preview_pos(position as i32);
        app.set_preview_total(total);
        app.set_page(1);
    }

    fn set_preview_detail(&mut self, detail: bool) {
        if self.page != 1 || self.preview.detail_requested == detail || self.preview.width == 0 {
            return;
        }
        if let Some(meta) = actions::image_meta_for(&self.core, self.preview.id) {
            self.preview.detail_requested = detail;
            self.preview.seq = actions::request_render(
                &self.core, RenderKind::Preview, meta.id, meta.path, EditParams::default(),
                if detail { None } else { Some(PREVIEW_EDGE) },
            );
        }
    }

    fn read_exif(&mut self) {
        let Some(meta) = actions::image_meta_for(&self.core, self.preview.id) else { return };
        let core = Arc::clone(&self.core);
        std::thread::spawn(move || {
            let text = crate::util::exif_text(&meta.path);
            core.emit(AppEvent::ExifReady { id: meta.id, text });
        });
    }

    fn navigate(&mut self, delta: i32) {
        let Some(session) = self.core.current_session() else {
            return;
        };
        let Some(current) = self.selected else {
            return;
        };
        let neighbor = {
            let catalog = self.core.catalog_for(&session);
            catalog
                .neighbor(session.id, &session.sort, &session.filter, current, delta)
                .ok()
                .flatten()
        };
        if let Some(meta) = neighbor {
            self.open_preview(meta.id);
        }
    }

    fn park_editor(&mut self) {
        if self.edit.id >= 0 {
            let _ = self.core.renders.send(None);
            if let Some(app) = self.app() {
                app.set_edit_image(slint::Image::default());
            }
        }
        if self.edit.id >= 0 && (self.edit.history.len() > 1 || !self.edit.params.is_identity()) {
            self.drafts.insert(self.edit.id, std::mem::replace(&mut self.edit, EditState::empty()));
        } else {
            self.edit = EditState::empty();
        }
        self.pending_edit_render = None;
        self.cancel_mark();
    }

    fn draft_for(&self, id: i64) -> Option<&EditState> {
        if self.edit.id == id {
            Some(&self.edit)
        } else {
            self.drafts.get(&id)
        }
    }

    fn open_editor(&mut self, id: i64) {
        if self.saving.is_some() || (self.edit.id == id && self.page == 2) {
            return;
        }
        if self.confirm_editor_leave(EditorDestination::Editor(id)) {
            return;
        }
        if self.ui.window().is_fullscreen() {
            self.ui.invoke_toggle_fullscreen();
        }
        let Some(meta) = self
            .entries
            .iter()
            .find(|entry| entry.meta.id == id)
            .map(|entry| entry.meta.clone())
            .or_else(|| actions::image_meta_for(&self.core, id))
        else {
            return;
        };
        if self.page == 1 {
            self.clear_preview_buffer();
        }
        self.leave_conversion_if_open();
        self.leave_rename_if_open();
        self.park_editor();
        self.edit = self.drafts.remove(&id).unwrap_or_else(|| {
            let params = actions::load_edit_params(&self.core, id);
            let (width, height) = oriented_dimensions(&meta);
            let cursor = usize::from(!params.is_identity());
            EditState {
                id,
                params: params.clone(),
                history: if params.is_identity() {
                    vec![params]
                } else {
                    vec![EditParams::default(), params]
                },
                cursor,
                seq: 0,
                base_width: width,
                base_height: height,
                last_push: Instant::now() - Duration::from_secs(5),
                version: file_version(&meta.path).ok(),
                path: meta.path.clone(),
            }
        });
        self.page = 2;
        self.update_thumbnail_viewport(0..0);
        self.cancel_mark();
        self.select(id);
        let Some(app) = self.app() else { return };
        app.set_edit_id(id as i32);
        app.set_edit_w(0);
        app.set_edit_h(0);
        app.set_edit_loading(true);
        app.set_edit_dirty(!self.edit.params.is_identity());
        app.set_rotation(self.edit.params.rotation.degrees());
        app.set_flip_h(self.edit.params.flip_horizontal);
        app.set_flip_v(self.edit.params.flip_vertical);
        app.set_exposure(self.edit.params.exposure);
        app.set_contrast(self.edit.params.contrast);
        app.set_saturation(self.edit.params.saturation);
        app.set_temperature(self.edit.params.temperature);
        app.set_has_crop(self.edit.params.crop.is_some());
        app.set_crop_mode(false);
        self.update_history_ui();
        app.set_page(2);
        app.set_sel_name(file_name_of(&meta.path).into());
        drop(app);
        self.edit_render_now();
    }

    fn edit_render_now(&mut self) {
        if self.edit.id < 0 {
            return;
        }
        let seq = actions::request_render(
            &self.core, RenderKind::Editor, self.edit.id, self.edit.path.clone(),
            self.edit.params.clone(), Some(1800),
        );
        self.edit.seq = seq;
        if let Some(app) = self.app()
            && app.get_edit_w() == 0
        {
            app.set_edit_loading(true);
        }
    }

    fn schedule_edit_render(&mut self) {
        self.pending_edit_render = Some(Instant::now() + Duration::from_millis(120));
    }

    fn push_history(&mut self) {
        self.edit.history.truncate(self.edit.cursor + 1);
        self.edit.history.push(self.edit.params.clone());
        if self.edit.history.len() > 80 {
            self.edit.history.drain(0..40);
        }
        self.edit.cursor = self.edit.history.len() - 1;
        self.update_history_ui();
    }

    fn update_history_ui(&self) {
        if let Some(app) = self.app() {
            app.set_can_undo(self.edit.cursor > 0);
            app.set_can_redo(self.edit.cursor + 1 < self.edit.history.len());
        }
    }

    fn apply_params(&mut self, mutate: impl FnOnce(&mut EditParams)) {
        if self.edit.id < 0 || self.saving.is_some() {
            return;
        }
        mutate(&mut self.edit.params);
        self.edit.params = self.edit.params.normalized();
        if self.edit.last_push.elapsed() > Duration::from_millis(500) {
            self.push_history();
            self.edit.last_push = Instant::now();
        }
        if let Some(app) = self.app() {
            app.set_rotation(self.edit.params.rotation.degrees());
            app.set_has_crop(self.edit.params.crop.is_some());
            app.set_exposure(self.edit.params.exposure);
            app.set_contrast(self.edit.params.contrast);
            app.set_saturation(self.edit.params.saturation);
            app.set_temperature(self.edit.params.temperature);
            app.set_edit_dirty(!self.edit.params.is_identity());
        }
        self.schedule_edit_render();
    }

    fn undo(&mut self) {
        if self.saving.is_some() { return; }
        if !self.mark_draft.points.is_empty() || self.mark_draft.pointer_down {
            self.cancel_mark();
            return;
        }
        self.flush_history();
        if self.edit.cursor == 0 {
            return;
        }
        self.edit.cursor -= 1;
        self.edit.params = self.edit.history[self.edit.cursor].clone();
        self.sync_params_ui();
        self.render_edit_immediately();
        self.update_history_ui();
    }

    fn redo(&mut self) {
        if self.saving.is_some() { return; }
        if self.edit.cursor + 1 >= self.edit.history.len() {
            return;
        }
        self.edit.cursor += 1;
        self.edit.params = self.edit.history[self.edit.cursor].clone();
        self.sync_params_ui();
        self.render_edit_immediately();
        self.update_history_ui();
    }

    fn sync_params_ui(&mut self) {
        if let Some(app) = self.app() {
            app.set_rotation(self.edit.params.rotation.degrees());
            app.set_flip_h(self.edit.params.flip_horizontal);
            app.set_flip_v(self.edit.params.flip_vertical);
            app.set_exposure(self.edit.params.exposure);
            app.set_contrast(self.edit.params.contrast);
            app.set_saturation(self.edit.params.saturation);
            app.set_temperature(self.edit.params.temperature);
            app.set_has_crop(self.edit.params.crop.is_some());
            app.set_edit_dirty(!self.edit.params.is_identity());
        }
    }

    fn flush_history(&mut self) {
        if self.edit.history.get(self.edit.cursor) != Some(&self.edit.params) {
            self.push_history();
        }
    }

    fn render_edit_immediately(&mut self) {
        self.pending_edit_render = None;
        self.edit_render_now();
    }

    fn cancel_mark(&mut self) {
        self.mark_draft = Draft::default();
        if let Some(app) = self.app() {
            app.set_mark_draft("".into());
            app.set_mark_can_finish(false);
        }
    }

    fn select_mark_tool(&mut self, tool: i32) {
        self.cancel_mark();
        let Some(app) = self.app() else { return };
        let tool = if app.get_mark_tool() == tool { 0 } else { tool };
        app.set_mark_tool(tool);
        app.set_mark_width(match tool {
            2 | 6 | 8 => 18,
            7 => 24,
            _ => 5,
        });
        app.set_crop_mode(false);
    }

    fn commit_mark(
        &mut self,
        kind: AnnotationKind,
        points: Vec<MarkPoint>,
        width: f32,
        text: String,
        color: [u8; 3],
        style: gimage_core::StrokeStyle,
    ) {
        let mark = Annotation {
            kind,
            points: points
                .into_iter()
                .map(|point| point.to_source(&self.edit.params))
                .collect(),
            width,
            text,
            color,
            style: if matches!(kind, AnnotationKind::Line | AnnotationKind::Arrow) {
                style
            } else {
                Default::default()
            },
        };
        self.mark_draft = Draft::default();
        if let Some(app) = self.app() { app.set_mark_can_finish(false); }
        if !mark.is_valid() {
            self.cancel_mark();
            return;
        }
        if self.edit.params.annotations.len() >= 256 {
            self.cancel_mark();
            self.toast("单张图片最多可添加 256 个标注".into(), true);
            return;
        }
        self.flush_history();
        self.edit.params.annotations.push(Arc::new(mark));
        self.push_history();
        self.sync_params_ui();
        self.render_edit_immediately();
    }

    fn mark_event(&mut self, event: i32, x: f32, y: f32, w: f32, h: f32, shift: bool) {
        if event == 3 {
            self.cancel_mark();
            return;
        }
        if self.page != 2 || self.edit.id < 0 || self.saving.is_some() || w.min(h) < 1.0 {
            return;
        }
        let app = self.ui.global::<App>();
        let Some(tool) = marks::tool(app.get_mark_tool()) else {
            return;
        };
        let px = app.get_mark_width() as f32;
        let width = (px / w.min(h)).clamp(0.0005, 0.2);
        let color = app.get_mark_ink();
        let color = [color.red(), color.green(), color.blue()];
        let style = marks::stroke_style(app.get_mark_style());
        let shape = app.get_mark_shape();
        let text = app.get_mark_text().to_string();
        let point = MarkPoint {
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
        };
        match tool {
            MarkTool::Eraser => {
                if event == 0 {
                    self.mark_draft = Draft::default();
                    self.mark_draft.pointer_down = true;
                }
                if !self.mark_draft.pointer_down {
                    return;
                }
                for (index, mark) in self.edit.params.annotations.iter().enumerate() {
                    if marks::hits(mark, &self.edit.params, point, (w, h), px) {
                        self.mark_draft.erased.insert(index);
                    }
                }
                if event == 2 {
                    let removed = std::mem::take(&mut self.mark_draft.erased);
                    self.cancel_mark();
                    if removed.is_empty() {
                        return;
                    }
                    self.flush_history();
                    let mut index = 0;
                    self.edit.params.annotations.retain(|_| {
                        let keep = !removed.contains(&index);
                        index += 1;
                        keep
                    });
                    self.push_history();
                    self.sync_params_ui();
                    self.render_edit_immediately();
                }
            }
            MarkTool::Draw(AnnotationKind::Text) if event == 0 => {
                if text.trim().is_empty() {
                    self.toast("请先输入标注文字，再点击图片放置".into(), false);
                    return;
                }
                if text.len() > 1024 {
                    self.toast("标注文字太长".into(), true);
                    return;
                }
                if !gimage_engine::annotation_text_available() {
                    self.toast("未找到可用的标注字体".into(), true);
                    return;
                }
                self.commit_mark(AnnotationKind::Text, vec![point], width, text, color, Default::default());
            }
            MarkTool::Draw(AnnotationKind::Text) => {}
            MarkTool::Draw(AnnotationKind::Polygon) if shape == 0 => {
                if event == 4 && self.mark_draft.points.len() >= 3 {
                    let points = std::mem::take(&mut self.mark_draft.points);
                    self.commit_mark(AnnotationKind::Polygon, points, width, String::new(), color, Default::default());
                } else if matches!(event, 0 | 1) {
                    if event == 0 {
                        marks::append_point(&mut self.mark_draft.points, point, (w, h), 256);
                    }
                    let mut preview = self.mark_draft.points.clone();
                    if !preview.is_empty() && event == 1 {
                        preview.push(point);
                    }
                    app.set_mark_can_finish(self.mark_draft.points.len() >= 3);
                    app.set_mark_draft(
                        marks::preview(AnnotationKind::Polygon, &preview, (w, h), px, Default::default(), false).into(),
                    );
                }
            }
            MarkTool::Draw(kind) => {
                let freehand = matches!(kind, AnnotationKind::Pen | AnnotationKind::Highlight);
                if event == 0 {
                    self.mark_draft = Draft::default();
                    self.mark_draft.pointer_down = true;
                    self.mark_draft.points.push(point);
                    if !freehand {
                        self.mark_draft.points.push(point);
                    }
                }
                if !self.mark_draft.pointer_down {
                    return;
                }
                if freehand {
                    marks::append_point(&mut self.mark_draft.points, point, (w, h), 2048);
                } else {
                    self.mark_draft.points[1] = point;
                }
                let preset = kind == AnnotationKind::Polygon;
                if event == 2 || preset {
                    let points = if preset {
                        marks::shape(shape, self.mark_draft.points[0], point, (w, h), shift)
                    } else {
                        self.mark_draft.points.clone()
                    };
                    if event == 2 {
                        if points.is_empty() || !freehand && !preset && points[0] == points[1] {
                            self.cancel_mark();
                        } else {
                            app.set_mark_draft(marks::preview(kind, &points, (w, h), px, style, preset).into());
                            self.commit_mark(kind, points, width, String::new(), color, style);
                        }
                    } else {
                        app.set_mark_draft(marks::preview(kind, &points, (w, h), px, style, true).into());
                    }
                } else {
                    app.set_mark_draft(
                        marks::preview(kind, &self.mark_draft.points, (w, h), px, style, false).into(),
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Library actions
    // -----------------------------------------------------------------------
    fn open_folder_dialog(&mut self) {
        if self.saving.is_some() || self.pending_confirm.is_some() {
            return;
        }
        let picked = rfd::FileDialog::new()
            .set_title("选择图片文件夹")
            .pick_folder();
        if let Some(path) = picked {
            self.open_folder(path);
        }
    }

    fn open_image_dialog(&mut self) {
        if self.saving.is_some() || self.pending_confirm.is_some() {
            return;
        }
        let mut dialog = rfd::FileDialog::new()
            .set_title("打开图片")
            .add_filter("图片", gimage_core::types::SCANNABLE_EXTENSIONS);
        if let Some(session) = self.core.current_session() {
            dialog = dialog.set_directory(session.path);
        }
        if let Some(path) = dialog.pick_file() {
            self.open_image_path(path);
        }
    }

    fn open_image_path(&mut self, path: PathBuf) {
        if self.saving.is_some() || self.confirm_editor_leave(EditorDestination::Image(path.clone())) {
            return;
        }
        self.open_indexed_image(path, None);
    }

    fn open_indexed_image(&mut self, path: PathBuf, workbench: Option<i32>) {
        let indexed = (|| -> anyhow::Result<(PathBuf, PathBuf, i64)> {
            let path = path.canonicalize()?;
            let folder = path.parent().ok_or_else(|| anyhow::anyhow!("无法定位文件目录"))?.to_owned();
            let format = ImageFormat::from_path(&path).ok_or_else(|| anyhow::anyhow!("无法识别图片格式"))?;
            let (size_bytes, modified) = file_version(&path)?;
            let mut catalog = self.core.catalog.lock();
            let folder_id = catalog.upsert_folder(&folder)?;
            catalog.sync_scan(folder_id, &gimage_catalog::ScanOutput {
                files: vec![gimage_catalog::ScannedFile {
                    path: path.clone(), size_bytes, format,
                    modified_ns: modified.duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_nanos().min(i64::MAX as u128) as i64),
                }],
                truncated: true,
                ..Default::default()
            })?;
            let id = catalog.image_at_path(&path)?.ok_or_else(|| anyhow::anyhow!("图片索引不可用"))?.id;
            Ok((folder, path, id))
        })();
        match indexed {
            Ok((folder, path, id)) => {
                self.open_folder(folder);
                if let Some(page) = workbench {
                    self.workbench_file = Some(TaskItem { id, path, edits: None, rename_target: None });
                    self.set_page(page);
                } else {
                    self.open_preview(id);
                }
            }
            Err(error) => self.toast(format!("无法打开图片：{error}"), true),
        }
    }

    fn open_folder(&mut self, path: PathBuf) {
        if self.saving.is_some() || self.confirm_editor_leave(EditorDestination::Folder(path.clone())) {
            return;
        }
        if self.ui.window().is_fullscreen() {
            self.ui.invoke_toggle_fullscreen();
        }
        if !path.is_dir() {
            if let Some(app) = self.app() {
                app.set_toast_message("不是有效的文件夹".into());
                app.set_toast_error(true);
            }
            return;
        }
        self.leave_conversion_if_open();
        self.leave_rename_if_open();
        self.set_marked(HashSet::new());
        self.selection_anchor = None;
        let mut settings = self.core.settings_snapshot();
        settings.last_folder = Some(path.clone());
        if let Ok(canonical) = path.canonicalize() {
            settings.hidden_recent_folders.retain(|hidden| hidden != &canonical);
        }
        if settings.sort.key != self.sort.key
            || settings.sort.direction != self.sort.direction
        {
            settings.sort = self.sort.clone();
        }
        self.core.update_settings(settings.clone());
        let _ = settings.save(&gimage_core::paths::settings_path());

        self.entries.clear();
        self.requested.clear();
        self.loaded = 0;
        self.total = 0;
        self.loading_more = false;
        self.rows.set_vec(Vec::new());
        self.clear_selection();
        self.clear_preview_buffer();
        self.preview = PreviewState { seq: 0, id: -1, width: 0, height: 0, detail_requested: false };
        self.park_editor();
        self.filter = ImageFilter::default();
        self.sort = settings.sort.clone();
        self.format_model.set_vec(Vec::new());

        if let Some(app) = self.app() {
            app.set_preview_id(-1);
            app.set_preview_w(0);
            app.set_preview_h(0);
            app.set_folder_name(
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned())
                    .into(),
            );
            app.set_folder_path(display_path(&path).into());
            app.set_temporary_browser(false);
            app.set_scanning(true);
            app.set_scan_status("正在扫描文件夹…".into());
            app.set_search_text("".into());
            app.set_favorites_only(false);
            app.set_selected_id(-1);
            app.set_loaded_images(0);
            app.set_total_images(0);
            app.set_can_load_more(false);
            app.set_grid_rows(ModelRc::new(self.rows.clone()));
        }
        self.generation = actions::begin_scan(&self.core, path, self.sort.clone(), self.filter.clone());
        self.page = 0;
        if let Some(app) = self.app() { app.set_page(0); }
    }

    fn refresh(&mut self) {
        self.reload_view();
    }

    fn reload_view(&mut self) {
        if let Some(mut session) = self.core.current_session() {
            if session.temporary_catalog.is_some() {
                self.generation = self.core.next_generation();
                session.generation = self.generation;
                session.sort = self.sort.clone();
                session.filter = self.filter.clone();
                self.core.set_session(Some(session));
                actions::load_page(&self.core, self.generation, 0);
                return;
            }
            self.generation = actions::begin_scan(
                &self.core,
                session.path.clone(),
                self.sort.clone(),
                self.filter.clone(),
            );
            self.entries.clear();
            self.requested.clear();
            self.loaded = 0;
            self.total = 0;
            self.loading_more = false;
            self.rows.set_vec(Vec::new());
            self.clear_selection();
            if let Some(app) = self.app() {
                app.set_scanning(true);
                app.set_scan_status("正在重新扫描…".into());
                app.set_grid_rows(ModelRc::new(self.rows.clone()));
                app.set_loaded_images(0);
                app.set_total_images(0);
            }
        }
    }

    fn load_more(&mut self) {
        if self.loading_more || self.loaded as u64 >= self.total {
            return;
        }
        self.loading_more = true;
        if let Some(app) = self.app() {
            app.set_loading_more(true);
        }
        actions::load_page(&self.core, self.generation, self.loaded);
    }

    fn update_sort(&mut self, index: i32, descending: bool) {
        self.sort = sort_spec(index, descending);
        let mut settings = self.core.settings_snapshot();
        settings.sort = self.sort.clone();
        self.core.update_settings(settings.clone());
        let _ = settings.save(&gimage_core::paths::settings_path());
        if let Some(app) = self.app() {
            app.set_sort_index(index);
            app.set_sort_descending(descending);
        }
        self.reload_view();
    }

    fn change_scan_depth(&mut self, depth: i32) {
        let depth = depth.clamp(0, 5) as u8;
        let mut settings = self.core.settings_snapshot();
        if settings.effective_scan_depth() == depth { return; }
        settings.recursive_scan = depth > 0;
        if depth > 0 { settings.scan_depth = depth; }
        if let Err(error) = settings.save(&gimage_core::paths::settings_path()) {
            self.toast(format!("无法保存扫描深度：{error}"), true);
            return;
        }
        self.core.update_settings(settings.clone());
        let app = self.ui.global::<App>();
        app.set_scan_depth_limit(i32::from(depth));
        app.set_settings_recursive(settings.recursive_scan);
        self.reload_view();
    }

    fn set_favorites_filter(&mut self, only: bool) {
        self.filter.favorites_only = only;
        if let Some(app) = self.app() {
            app.set_favorites_only(only);
        }
        self.reload_view();
    }

    fn toggle_format_filter(&mut self, label: &str) {
        let Some(format) = self.core.current_session().and_then(|session| {
            self.core.catalog_for(&session).folder_formats_at_depth(session.id, session.filter.max_directory_depth).ok()
        }).and_then(|formats| formats.into_iter().find(|format| format.label() == label)) else {
            return;
        };
        if let Some(position) = self.filter.formats.iter().position(|f| f == &format) {
            self.filter.formats.remove(position);
        } else {
            self.filter.formats.push(format);
        }
        self.rebuild_formats();
        self.reload_view();
    }

    fn rebuild_formats(&mut self) {
        let formats = self.core.current_session().and_then(|session| {
            self.core.catalog_for(&session).folder_formats_at_depth(session.id, session.filter.max_directory_depth).ok()
        }).unwrap_or_default();
        self.format_model.set_vec(formats.iter().map(|format| FormatChip {
            label: format.label().into(),
            active: self.filter.formats.contains(format),
        }).collect::<Vec<_>>());
    }

    fn toggle_favorite(&mut self, id: i64) {
        let Some(meta) = self
            .entries
            .iter()
            .find(|entry| entry.meta.id == id)
            .map(|entry| entry.meta.clone())
            .or_else(|| actions::image_meta_for(&self.core, id))
        else {
            return;
        };
        let favorite = !meta.favorite;
        if let Err(error) = actions::set_favorite(&self.core, id, favorite) {
            if let Some(app) = self.app() {
                app.set_toast_message(format!("收藏失败：{error}").into());
                app.set_toast_error(true);
            }
            return;
        }
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.meta.id == id) {
            entry.meta.favorite = favorite;
        }
        if let Some(index) = self.entry_index(id) {
            self.refresh_row(index);
        }
        if let Some(app) = self.app() {
            app.set_sel_favorite(favorite);
            app.set_toast_message(if favorite { "已加入收藏" } else { "已取消收藏" }.into());
            app.set_toast_error(false);
        }
        self.toast_until = Some(Instant::now() + Duration::from_secs(3));
        if self.filter.favorites_only {
            self.reload_view();
        }
    }

    // -----------------------------------------------------------------------
    // Event pump
    // -----------------------------------------------------------------------
    pub fn tick(&mut self) {
        self.drain_events();
        let now = Instant::now();
        self.tick_conversion(now);
        self.tick_rename(now);
        // Keep pending changes while the user is previewing or editing, or
        // while a scan is already reconciling the folder.
        if self.page == 0
            && !self.loading_more
            && self.pending_reload.is_none()
            && self.app().is_some_and(|app| {
                !app.get_scanning()
                    && !app.get_export_open()
                    && !app.get_confirm_open()
            })
        {
            let changed = self.core.folder_watch.try_lock().is_some_and(|watch| {
                watch.as_ref().is_some_and(|watch| watch.take_due(self.generation, now))
            });
            if changed {
                self.reload_view();
            }
        }
        if let Some(until) = self.toast_until
            && now >= until
        {
            self.toast_until = None;
            if let Some(app) = self.app() {
                app.set_toast_message("".into());
            }
        }
        if let Some(due) = self.pending_reload
            && now >= due
        {
            self.pending_reload = None;
            self.reload_view();
        }
        if let Some(due) = self.pending_edit_render
            && now >= due
        {
            self.pending_edit_render = None;
            self.edit_render_now();
        }
        if let Some(due) = self.pending_columns
            && now >= due
        {
            self.pending_columns = None;
            self.rebuild_rows();
        }
    }

    fn drain_events(&mut self) {
        for event in self.core.drain_events(128) {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::UpdateChecked(result) => self.update_checked(result),
            AppEvent::UpdateDownloaded(result) => self.update_downloaded(result),
            AppEvent::DroppedBrowser { request, generation, page, preview, result } => {
                self.dropped_browser_ready(request, generation, page, preview, result);
            }
            AppEvent::DroppedConversion { token, items } => self.conversion_dropped(token, items),
            AppEvent::BrowserAllSelected { generation, request, ids } => {
                if self.page == 0 && generation == self.generation && request == self.mark_request {
                    match ids {
                        Ok(ids) => self.set_marked(ids.into_iter().collect()),
                        Err(error) => self.toast(format!("无法全选：{error}"), true),
                    }
                }
            }
            AppEvent::RenameLoaded { token, entries } => self.rename_loaded(token, entries),
            AppEvent::RenamePreview { token, revision, order, targets } => self.rename_preview_ready(token, revision, order, targets),
            AppEvent::SettingsStats { cache, catalog, engine } => {
                if self.page == 4 && let Some(app) = self.app() {
                    app.set_cache_stats(cache.into());
                    app.set_db_stats(catalog.into());
                    app.set_engine_detail(engine.into());
                }
            }
            AppEvent::ConversionLoaded { token, items } => self.conversion_loaded(token, items),
            AppEvent::ExifReady { id, text } => {
                if self.preview.id == id && let Some(app) = self.app() {
                    app.set_exif_text(text.into());
                }
            }
            AppEvent::EditSaved { id, result } => {
                self.saving = None;
                let destination = self.save_destination.take();
                if let Some(app) = self.app() { app.set_edit_saving(false); }
                match result {
                    Ok(()) => {
                        self.drafts.remove(&id);
                        self.edit.params = EditParams::default();
                        self.edit.history = vec![EditParams::default()];
                        self.edit.cursor = 0;
                        self.edit.version = file_version(&self.edit.path).ok();
                        self.edit.base_width = 0;
                        self.edit.base_height = 0;
                        actions::request_probe(&self.core, id, self.edit.path.clone());
                        self.update_history_ui();
                        self.sync_params_ui();
                        self.toast("已保存".into(), false);
                        if let Some(destination) = destination {
                            self.complete_editor_leave(destination);
                        } else if self.page == 1 {
                            self.open_preview(id);
                        } else if self.page == 2 {
                            self.render_edit_immediately();
                        }
                    }
                    Err(error) => self.toast(format!("保存失败：{error}"), true),
                }
            }
            AppEvent::CopyImage(result) => match result {
                Ok(image) => self.copy_to_clipboard("已复制图片", |clipboard| clipboard.set_image(arboard::ImageData {
                    width: image.width as usize,
                    height: image.height as usize,
                    bytes: std::borrow::Cow::Owned(image.pixels),
                })),
                Err(error) => self.toast(format!("无法复制图片：{error}"), true),
            },
            AppEvent::ScanProgress { generation, scanned, found, current } => {
                if generation != self.generation {
                    return;
                }
                if let Some(app) = self.app() {
                    app.set_scanning(true);
                    app.set_scan_status(
                        format!("扫描 {scanned} 个文件 · 找到 {found} 张 · {current}").into(),
                    );
                }
            }
            AppEvent::ScanFinished { generation, report } => {
                if generation != self.generation {
                    return;
                }
                self.rebuild_formats();
                if let Some(app) = self.app() {
                    app.set_scanning(false);
                    match &report {
                        Ok(report) => {
                            app.set_scan_depth_found(report.max_image_depth as i32);
                            app.set_scan_status(actions::scan_summary_text(report).into());
                        }
                        Err(error) => {
                            app.set_scan_depth_found(-1);
                            app.set_scan_status(error.clone().into());
                            app.set_toast_message(error.clone().into());
                            app.set_toast_error(true);
                            self.toast_until = Some(Instant::now() + Duration::from_secs(5));
                        }
                    }
                }
                if self.page == 5 && self.conversion.waiting_for_scan {
                    self.load_conversion();
                }
            }
            AppEvent::PageReady { generation, offset, total, items } => {
                if generation != self.generation {
                    return;
                }
                self.loading_more = false;
                if offset == 0 {
                    self.entries.clear();
                    self.requested.clear();
                } else if offset < self.loaded {
                    return;
                }
                self.total = total;
                self.loaded = offset.saturating_add(items.len() as u32);
                for item in items {
                    self.entries.push(Entry {
                        meta: item,
                        thumb: None,
                        state: 0,
                        error: None,
                        row: 0,
                        col: 0,
                    });
                }
                self.rebuild_rows();
                if offset == 0 && self.page == 1 && self.preview.id >= 0 {
                    let position = self.core.current_session().and_then(|session| {
                        self.core.catalog_for(&session).image_position(
                            session.id, &session.sort, &session.filter, self.preview.id
                        ).ok().flatten()
                    }).unwrap_or(1);
                    if let Some(app) = self.app() {
                        app.set_preview_pos(position as i32);
                        app.set_preview_total(self.total.max(position) as i32);
                    }
                    if let Some(meta) = actions::image_meta_for(&self.core, self.preview.id) {
                        self.set_preview_window(meta.id, meta.path);
                        if self.app().is_some_and(|app| !app.get_preview_loading()) {
                            self.core.preview_buffer.prefetch();
                        }
                    }
                }
                if let Some(app) = self.app() {
                    app.set_total_images(total.min(i32::MAX as u64) as i32);
                    app.set_loaded_images(self.loaded as i32);
                    app.set_can_load_more((self.loaded as u64) < total);
                    app.set_loading_more(false);
                }
            }
            AppEvent::PageFailed { generation, error } => {
                if generation != self.generation {
                    return;
                }
                self.loading_more = false;
                if let Some(app) = self.app() {
                    app.set_loading_more(false);
                    app.set_toast_message(format!("加载图片失败：{error}").into());
                    app.set_toast_error(true);
                }
            }
            AppEvent::ThumbReady { generation, cache_epoch, id, px, image } => {
                if generation != self.generation || cache_epoch != self.core.cache.epoch() {
                    return;
                }
                self.requested.remove(&(id, px));
                if px != self.core.settings.read().thumbnail_px {
                    return;
                }
                let Some(index) = self.entry_index(id) else {
                    return;
                };
                if self.page != 0 || !self.visible_rows.contains(&self.entries[index].row) {
                    return;
                }
                let image = to_slint_image(&image);
                self.entries[index].thumb = Some(image.clone());
                self.entries[index].state = 1;
                self.entries[index].error = None;
                self.refresh_row(index);
                if self.selected == Some(id)
                    && let Some(app) = self.app()
                {
                    app.set_sel_thumb(image);
                }
            }
            AppEvent::ThumbFailed { generation, cache_epoch, id, px, error } => {
                if generation != self.generation || cache_epoch != self.core.cache.epoch() {
                    return;
                }
                self.requested.remove(&(id, px));
                if px != self.core.settings.read().thumbnail_px {
                    return;
                }
                let Some(index) = self.entry_index(id) else {
                    return;
                };
                self.entries[index].state = 2;
                self.entries[index].error = Some(error);
                self.refresh_row(index);
            }
            AppEvent::ProbeReady { id, info } => {
                let Ok(info) = info else { return };
                if let Some(entry) = self.entries.iter_mut().find(|entry| entry.meta.id == id) {
                    entry.meta.width = Some(info.width);
                    entry.meta.height = Some(info.height);
                    entry.meta.orientation = info.orientation;
                }
                if let Some(index) = self.entry_index(id) {
                    self.refresh_row(index);
                }
                if self.selected == Some(id) {
                    self.select(id);
                }
                if self.preview.id == id && let Some(app) = self.app() {
                    let (w, h) = if (5..=8).contains(&info.orientation) {
                        (info.height, info.width)
                    } else {
                        (info.width, info.height)
                    };
                    app.set_preview_source_w(w as i32);
                    app.set_preview_source_h(h as i32);
                }
                if self.edit.id == id {
                    (self.edit.base_width, self.edit.base_height) = if (5..=8).contains(&info.orientation) {
                        (info.height, info.width)
                    } else {
                        (info.width, info.height)
                    };
                }
            }
            AppEvent::RenderReady { seq, kind, id, image } => {
                let (width, height) = (image.width(), image.height());
                match kind {
                    RenderKind::Conversion => self.conversion_preview_ready(seq, image),
                    RenderKind::Preview => {
                        if self.page != 1 || seq != self.preview.seq {
                            return;
                        }
                        self.preview.width = width;
                        self.preview.height = height;
                        if let Some(app) = self.app() {
                            app.set_preview_image(slint::Image::from_rgba8(image));
                            app.set_preview_w(width as i32);
                            app.set_preview_h(height as i32);
                            app.set_preview_loading(false);
                            app.set_preview_id(id as i32);
                        }
                        self.core.preview_buffer.prefetch();
                    }
                    RenderKind::Editor => {
                        if self.page != 2 || seq != self.edit.seq {
                            return;
                        }
                        if let Some(app) = self.app() {
                            if !self.mark_draft.pointer_down && self.mark_draft.points.is_empty() {
                                app.set_mark_draft("".into());
                            }
                            app.set_edit_image(slint::Image::from_rgba8(image));
                            app.set_edit_w(width as i32);
                            app.set_edit_h(height as i32);
                            app.set_edit_loading(false);
                            app.set_edit_info(format!("{width} × {height}").into());
                        }
                    }
                }
            }
            AppEvent::RenderFailed { seq, kind, id, error } => {
                let current = match kind {
                    RenderKind::Conversion => {
                        self.conversion_preview_failed(seq, error);
                        return;
                    }
                    RenderKind::Preview => self.page == 1 && seq == self.preview.seq,
                    RenderKind::Editor => seq == self.edit.seq,
                };
                if !current {
                    return;
                }
                if kind == RenderKind::Preview {
                    self.core.preview_buffer.prefetch();
                }
                if let Some(app) = self.app() {
                    match kind {
                        RenderKind::Preview => app.set_preview_loading(false),
                        RenderKind::Editor => app.set_edit_loading(false),
                        RenderKind::Conversion => {}
                    }
                    app.set_toast_message(format!("渲染失败（#{id}）：{error}").into());
                    app.set_toast_error(true);
                }
                self.toast_until = Some(Instant::now() + Duration::from_secs(5));
            }
            AppEvent::Task(snapshot) => {
                if let Some(existing) = self
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == snapshot.id)
                {
                    *existing = snapshot;
                } else {
                    self.tasks.push(snapshot);
                }
                self.rebuild_task_model();
            }
            AppEvent::Toast { message, error } => {
                if let Some(app) = self.app() {
                    app.set_toast_message(message.into());
                    app.set_toast_error(error);
                }
                self.toast_until = Some(Instant::now() + Duration::from_secs(4));
            }
            AppEvent::LibraryUpdated { folders, stats } => {
                let hidden = self.core.settings_snapshot().hidden_recent_folders;
                let rows: Vec<FolderRow> = folders
                    .iter()
                    .filter(|folder| !hidden.contains(&PathBuf::from(&folder.path)))
                    .map(|folder| FolderRow {
                        id: folder.id as i32,
                        name: folder.name.clone().into(),
                        path: folder.path.clone().into(),
                        count: folder.count as i32,
                    })
                    .collect();
                self.folder_model.set_vec(rows);
                if let Some(app) = self.app() {
                    app.set_recent_folders(ModelRc::new(self.folder_model.clone()));
                    app.set_library_stats(stats.clone().into());
                    app.set_db_stats(stats.into());
                }
            }
            AppEvent::CacheStats(text) => {
                if let Some(app) = self.app() {
                    app.set_cache_stats(text.into());
                }
            }
            AppEvent::RefreshRequested => {
                self.reload_view();
            }
        }
    }

    fn rebuild_task_model(&mut self) {
        let rows: Vec<TaskRow> = self
            .tasks
            .iter()
            .map(|task| TaskRow {
                id: task.id as i32,
                title: task.title.clone().into(),
                detail: task.detail.clone().into(),
                status: task.status.clone().into(),
                progress: task.progress,
                done: task.done as i32,
                total: task.total as i32,
                failed: task.failed as i32,
                state: task.state,
                cancelable: task.cancelable,
                retryable: task.retryable,
            })
            .collect();
        let active = self
            .tasks
            .iter()
            .filter(|task| task.state == 0 || task.state == 1)
            .count();
        self.task_model.set_vec(rows);
        if let Some(app) = self.app() {
            app.set_tasks(ModelRc::new(self.task_model.clone()));
            app.set_active_tasks(active as i32);
        }
    }

    // -----------------------------------------------------------------------
    // Editing callbacks
    // -----------------------------------------------------------------------
    fn jump_to_editor_for_selected(&mut self) {
        if let Some(id) = self.selected {
            self.open_editor(id);
        }
    }

    fn crop_preset(&mut self, preset: i32) {
        let (mut width, mut height) = (self.edit.base_width, self.edit.base_height);
        if self.edit.params.rotation.swaps_axes() {
            std::mem::swap(&mut width, &mut height);
        }
        if width == 0 || height == 0 {
            return;
        }
        let target = match preset {
            0 => 1.0f32,
            1 => 16.0 / 9.0,
            2 => 4.0 / 3.0,
            _ => return,
        };
        let image_aspect = width as f32 / height as f32;
        let (cw, ch) = if image_aspect > target {
            (target / image_aspect, 1.0)
        } else {
            (1.0, image_aspect / target)
        };
        let crop = CropRect {
            x: (1.0 - cw) / 2.0,
            y: (1.0 - ch) / 2.0,
            width: cw,
            height: ch,
        };
        self.apply_params(|params| params.crop = Some(crop));
        if let Some(app) = self.app() {
            app.set_crop_mode(false);
        }
    }

    // -----------------------------------------------------------------------
    // Export / rename
    // -----------------------------------------------------------------------
    fn open_export_copy(&mut self) {
        if self.saving.is_some() {
            return;
        }
        if self.selected.is_none() {
            self.set_page(5);
            return;
        }
        let index = self.app().map(|app| app.get_export_format_index()).unwrap_or(0);
        if let Some(app) = self.app() {
            app.set_export_target_count(1);
            app.set_export_open(true);
        }
        self.export_hint_for(index);
    }

    fn export_confirm(&mut self, format_label: &str, quality: i32, max_w: i32, max_h: i32, policy: i32) {
        let options = actions::export_options_from_ui(
            actions::export_format_from_label(format_label),
            quality as u8,
            max_w.max(0) as u32,
            max_h.max(0) as u32,
            actions::policy_from_index(policy),
        );
        if let Some(app) = self.app() {
            app.set_export_open(false);
        }
        let target_items = self.selected
            .and_then(|id| actions::image_meta_for(&self.core, id)
                .map(|meta| TaskItem { id, path: meta.path, edits: self.draft_for(id).map(EditState::export_edits), rename_target: None }))
            .into_iter()
            .collect();
        if matches!(options.output, OutputPolicy::Overwrite) {
            self.pending_confirm = Some(PendingConfirm::Export(target_items, options));
            if let Some(app) = self.app() {
                app.set_confirm_title("覆盖已存在的文件？".into());
                app.set_confirm_body(
                    "导出将替换目标位置同名文件，源图不会被修改。该操作无法撤销。".into(),
                );
                app.set_confirm_label("覆盖并导出".into());
                app.set_confirm_danger(true);
                app.set_confirm_open(true);
            }
            return;
        }
        actions::start_export(&self.core, target_items, options, "导出 1 张图片".into());
    }

    fn confirm_accepted(&mut self) {
        if let Some(app) = self.app() {
            app.set_confirm_open(false);
            app.set_confirm_discard(false);
        }
        let Some(pending) = self.pending_confirm.take() else {
            return;
        };
        match pending {
            PendingConfirm::Update(update) => self.download_update(update),
            PendingConfirm::SaveEdit => self.save_edit(),
            PendingConfirm::Conversion(options) => self.start_conversion(options),
            PendingConfirm::Rename(items, options) => {
                actions::start_rename(&self.core, items, options);
                self.set_page(3);
            }
            PendingConfirm::LeaveEditor(destination) => {
                self.save_destination = Some(destination);
                self.save_edit();
            }
            PendingConfirm::Exit => { let _ = self.ui.hide(); }
            PendingConfirm::Export(items, options) => {
                actions::start_export(&self.core, items, options, "导出 1 张图片".into());
            }
        }
    }

    fn confirm_editor_leave(&mut self, destination: EditorDestination) -> bool {
        if self.page != 2 || self.edit.params.is_identity() {
            return false;
        }
        if self.pending_confirm.is_none() {
            self.pending_confirm = Some(PendingConfirm::LeaveEditor(destination));
            let app = self.ui.global::<App>();
            app.set_confirm_title("保存编辑后离开？".into());
            app.set_confirm_body("保存会覆盖原图，重新编码可能移除原有元数据。".into());
            app.set_confirm_label("保存并离开".into());
            app.set_confirm_danger(false);
            app.set_confirm_discard(true);
            app.set_confirm_open(true);
        }
        true
    }

    fn complete_editor_leave(&mut self, destination: EditorDestination) {
        match destination {
            EditorDestination::Page(page) => self.set_page(page),
            EditorDestination::Preview(id) => self.open_preview(id),
            EditorDestination::Editor(id) => self.open_editor(id),
            EditorDestination::Folder(path) => self.open_folder(path),
            EditorDestination::Image(path) => self.open_image_path(path),
            EditorDestination::Drop(paths) => self.drop_paths(paths),
            EditorDestination::Exit => self.request_exit(),
        }
    }

    fn discard_edit_and_leave(&mut self) {
        let Some(PendingConfirm::LeaveEditor(destination)) = self.pending_confirm.take() else {
            return;
        };
        self.cancel_confirm();
        let cleared = self.core.with_image_catalog(self.edit.id, |catalog| catalog.clear_edit_session(self.edit.id));
        if let Some(Err(error)) = cleared {
            self.toast(format!("无法丢弃编辑：{error}"), true);
            return;
        }
        self.drafts.remove(&self.edit.id);
        self.edit.params = EditParams::default();
        self.edit.history = vec![EditParams::default()];
        self.edit.cursor = 0;
        self.sync_params_ui();
        self.update_history_ui();
        self.complete_editor_leave(destination);
        if self.page == 2 && self.edit.params.is_identity() {
            self.render_edit_immediately();
        }
    }

    fn confirm_save_edit(&mut self) {
        if self.edit.params.is_identity() || self.saving.is_some() || self.page != 2
            || self.pending_confirm.is_some()
        {
            return;
        }
        self.pending_confirm = Some(PendingConfirm::SaveEdit);
        let Some(app) = self.app() else { return };
        app.set_confirm_title("保存到原文件？".into());
        app.set_confirm_body("保存会覆盖原文件并清空撤销历史。重新编码可能移除原有元数据。".into());
        app.set_confirm_label("保存并覆盖".into());
        app.set_confirm_danger(true);
        app.set_confirm_open(true);
    }

    fn save_edit(&mut self) {
        let id = self.edit.id;
        let path = self.edit.path.clone();
        let params = self.edit.params.clone();
        let version = self.edit.version;
        let core = Arc::clone(&self.core);
        self.saving = Some(id);
        if let Some(app) = self.app() { app.set_edit_saving(true); }
        std::thread::spawn(move || {
            let result = (|| -> Result<(), gimage_core::EngineError> {
                let _editing = core.image_writes.write();
                let check_version = || {
                    let current = file_version(&path)
                        .map_err(|error| gimage_core::EngineError::io(&path, error))?;
                    if Some(current) == version {
                        Ok(())
                    } else {
                        Err(gimage_core::EngineError::encode(&path, "文件已被其它程序修改，请导出副本"))
                    }
                };
                check_version()?;
                let format = gimage_engine::replacement_format(&path)?;
                let image = core.engine.render(&path, &params, None)?;
                gimage_engine::write_atomic_checked(core.engine.as_ref(), &image, format, 95, &path, check_version)?;
                if let Some(Err(error)) = core.with_image_catalog(id, |catalog| catalog.clear_edit_session(id)) {
                    tracing::warn!(%error, id, "failed to clear legacy edit session");
                }
                Ok(())
            })().map_err(|error| error.user_message());
            core.emit(AppEvent::EditSaved { id, result });
        });
    }

    pub fn can_exit(&mut self) -> bool {
        if self.ui.global::<App>().get_update_busy() {
            self.toast("正在处理更新，请稍候".into(), false);
            return false;
        }
        if self.saving.is_some() {
            self.toast("正在保存，完成后即可关闭".into(), false);
            return false;
        }
        if self.confirm_editor_leave(EditorDestination::Exit) {
            return false;
        }
        let pending = usize::from(!self.edit.params.is_identity())
            + self.drafts.values().filter(|draft| !draft.params.is_identity()).count();
        if pending == 0 {
            return true;
        }
        self.pending_confirm = Some(PendingConfirm::Exit);
        if let Some(app) = self.app() {
            app.set_confirm_title("关闭并丢弃未保存的编辑？".into());
            app.set_confirm_body(format!("有 {pending} 张图片的草稿尚未保存。").into());
            app.set_confirm_label("丢弃并关闭".into());
            app.set_confirm_danger(true);
            app.set_confirm_open(true);
        }
        false
    }

    fn request_exit(&mut self) {
        if self.can_exit() { let _ = self.ui.hide(); }
    }

    fn open_rename(&mut self) {
        self.set_page(6);
    }

    fn check_updates(&mut self) {
        let app = self.ui.global::<App>();
        if app.get_update_busy() || self.pending_confirm.is_some() {
            return;
        }
        app.set_update_busy(true);
        app.set_update_status("正在检查…".into());
        let core = Arc::clone(&self.core);
        let started = std::thread::Builder::new().name("gimage-update-check".into()).spawn(move || {
            core.emit(AppEvent::UpdateChecked(updater::check()));
        });
        if let Err(error) = started {
            self.update_checked(Err(error.to_string()));
        }
    }

    fn update_checked(&mut self, result: Result<Option<updater::Update>, String>) {
        let app = self.ui.global::<App>();
        app.set_update_busy(false);
        app.set_update_status("".into());
        match result {
            Ok(Some(update)) if self.pending_confirm.is_none() && !app.get_export_open() => {
                app.set_confirm_title(format!("发现 GImage {}", update.version).into());
                app.set_confirm_body(format!("当前版本 {}。确认后将下载并校验新版，完成后重启 GImage。", env!("CARGO_PKG_VERSION")).into());
                app.set_confirm_label("下载并更新".into());
                app.set_confirm_danger(false);
                app.set_confirm_discard(false);
                self.pending_confirm = Some(PendingConfirm::Update(update));
                app.set_confirm_open(true);
            }
            Ok(Some(update)) => self.toast(format!("发现新版 {}，请关闭当前对话框后重新检查。", update.version), false),
            Ok(None) => self.toast("已是最新版本".into(), false),
            Err(error) => self.toast(error, true),
        }
    }

    fn ready_to_update(&self) -> bool {
        self.saving.is_none()
            && self.pending_confirm.is_none()
            && self.edit.params.is_identity()
            && self.drafts.values().all(|draft| draft.params.is_identity())
            && !self.tasks.iter().any(|task| matches!(task.state, 0 | 1))
            && !self.ui.global::<App>().get_export_open()
    }

    fn download_update(&mut self, update: updater::Update) {
        if !self.ready_to_update() {
            self.toast("请先保存编辑并完成当前任务，再进行更新。".into(), false);
            return;
        }
        let app = self.ui.global::<App>();
        app.set_update_busy(true);
        app.set_update_status("正在下载并校验…".into());
        let core = Arc::clone(&self.core);
        let started = std::thread::Builder::new().name("gimage-update-download".into()).spawn(move || {
            core.emit(AppEvent::UpdateDownloaded(updater::download(update)));
        });
        if let Err(error) = started {
            self.update_downloaded(Err(error.to_string()));
        }
    }

    fn update_downloaded(&mut self, result: Result<updater::StagedUpdate, String>) {
        let app = self.ui.global::<App>();
        app.set_update_busy(false);
        app.set_update_status("".into());
        let staged = match result {
            Ok(staged) => staged,
            Err(error) => { self.toast(error, true); return; }
        };
        if !self.ready_to_update() {
            self.toast("有编辑或任务正在进行。请完成后重新更新。".into(), false);
            return;
        }
        if let Err(error) = staged.launch() {
            self.toast(error, true);
        } else {
            let _ = self.ui.hide();
        }
    }

    // -----------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------
    fn settings_apply(&mut self) {
        let Some(app) = self.app() else { return };
        let previous = self.core.settings_snapshot();
        let mut settings = previous.clone();
        settings.theme = if app.get_settings_dark() {
            gimage_core::Theme::Dark
        } else {
            gimage_core::Theme::Light
        };
        settings.thumbnail_px = app.get_settings_thumb_px().max(1) as u32;
        if !Settings::THUMBNAIL_SIZES.contains(&settings.thumbnail_px) {
            settings.thumbnail_px = 320;
        }
        settings.concurrency = app.get_settings_concurrency().max(1) as u32;
        settings.memory_cache_mb = app.get_settings_mem_mb().max(16) as u32;
        settings.disk_cache_mb = app.get_settings_disk_mb().max(32) as u32;
        settings.recursive_scan = app.get_settings_recursive();
        settings.include_hidden = app.get_settings_hidden();
        settings.low_power = app.get_settings_low_power();
        settings.file_menu_positions = [
            app.get_settings_context_browse(), app.get_settings_context_convert(), app.get_settings_context_rename(),
        ].map(|position| position.clamp(0, 2) as u8);
        settings.explorer_menu_enabled = app.get_settings_explorer_menu();
        let settings = settings.normalized();
        let update_explorer_menu = settings.explorer_menu_enabled || previous.explorer_menu_enabled;
        if update_explorer_menu {
            if let Err(error) = crate::native::explorer_menu(settings.explorer_menu_enabled, settings.file_menu_positions) {
                tracing::warn!(%error, "could not update Explorer context menu");
                let _ = crate::native::explorer_menu(previous.explorer_menu_enabled, previous.file_menu_positions);
                self.toast(format!("无法更新资源管理器菜单：{error}"), true);
                return;
            }
        }
        if let Err(error) = settings.save(&gimage_core::paths::settings_path()) {
            if update_explorer_menu {
                let _ = crate::native::explorer_menu(previous.explorer_menu_enabled, previous.file_menu_positions);
            }
            app.set_toast_message(format!("保存设置失败：{error}").into());
            app.set_toast_error(true);
            self.toast_until = Some(Instant::now() + Duration::from_secs(5));
            return;
        }
        self.core.update_settings(settings.clone());

        let theme = self.ui.global::<Theme>();
        let [browse, convert, rename] = settings.file_menu_positions.map(i32::from);
        app.set_context_browse(browse);
        app.set_context_convert(convert);
        app.set_context_rename(rename);
        app.set_scan_depth_limit(i32::from(settings.effective_scan_depth()));
        theme.set_dark(matches!(settings.theme, gimage_core::Theme::Dark));
        theme.set_low_power(settings.low_power);
        self.core.cache.set_memory_budget(settings.thumbnail_memory_bytes());
        let freed = self
            .core
            .cache
            .set_disk_budget(settings.disk_cache_bytes())
            .unwrap_or(0);
        let cache_text = actions::cache_stats_text(&self.core);
        app.set_cache_stats(cache_text.into());
        app.set_toast_message(
            format!(
                "设置已保存{}",
                if freed > 0 {
                    format!("，清理了 {}", size_text(freed))
                } else {
                    String::new()
                }
            )
            .into(),
        );
        app.set_toast_error(false);
        self.toast_until = Some(Instant::now() + Duration::from_secs(4));
        if settings.recursive_scan != previous.recursive_scan || settings.include_hidden != previous.include_hidden {
            self.reload_view();
        } else if settings.thumbnail_px != previous.thumbnail_px {
            self.reset_thumbnails();
        }
    }

    fn refresh_settings_stats(&mut self) {
        let core = Arc::clone(&self.core);
        let _ = std::thread::Builder::new().name("gimage-settings-stats".into()).spawn(move || {
            let cache = actions::cache_stats_text(&core);
            let stats = core.catalog.lock()
                .db_stats()
                .map(|stats| {
                    format!(
                        "{} 张图片 · {} 个文件夹 · {} 收藏\n数据库 {}（含索引）",
                        stats.images,
                        stats.folders,
                        stats.favorites,
                        size_text(stats.db_bytes)
                    )
                })
                .unwrap_or_else(|error| format!("索引不可用：{error}"));
            let caps = &core.capabilities;
            let engine = format!(
                "图像引擎：{} {}\n可读：{}\n可写：{}\n单帧显示：{}",
                caps.engine_name,
                caps.engine_version,
                caps.readable.iter().map(|f| f.label()).collect::<Vec<_>>().join(" / "),
                caps.writable.iter().map(|f| f.label()).collect::<Vec<_>>().join(" / "),
                caps.single_frame_only.iter().map(|f| f.label()).collect::<Vec<_>>().join(" / ")
            );
            core.emit(AppEvent::SettingsStats { cache, catalog: stats, engine });
        });
    }

    // -----------------------------------------------------------------------
    // Helpers used by wiring
    // -----------------------------------------------------------------------
    fn set_page(&mut self, page: i32) {
        if self.saving.is_some() {
            return;
        }
        if page != 2 && self.confirm_editor_leave(EditorDestination::Page(page)) {
            return;
        }
        if self.page == page && matches!(page, 5 | 6) { return; }
        if page != 5 { self.leave_conversion_if_open(); }
        if page != 6 { self.leave_rename_if_open(); }
        if page != 1 && self.ui.window().is_fullscreen() {
            self.ui.invoke_toggle_fullscreen();
        }
        if page != 2 && self.page == 2 && self.saving.is_none() {
            self.park_editor();
        }
        if page == 2 {
            self.jump_to_editor_for_selected();
            return;
        }
        if self.page == 1 && page != 1 {
            self.clear_preview_buffer();
        }
        self.page = page;
        if page != 0 {
            self.update_thumbnail_viewport(0..0);
        }
        if let Some(app) = self.app() {
            app.set_page(page);
        }
        match page {
            1 => {
                if let Some(id) = self.selected {
                    self.open_preview(id);
                }
            }
            3 => self.rebuild_task_model(),
            4 => self.refresh_settings_stats(),
            5 => self.load_conversion(),
            6 => self.load_rename(),
            _ => {}
        }
    }

    fn toast(&mut self, message: String, error: bool) {
        if let Some(app) = self.app() {
            app.set_toast_message(message.into());
            app.set_toast_error(error);
        }
        self.toast_until = Some(Instant::now() + Duration::from_secs(4));
    }

    fn remove_recent_folder(&mut self, path: PathBuf) {
        let mut settings = self.core.settings_snapshot();
        if settings.last_folder.as_ref().and_then(|folder| folder.canonicalize().ok()).as_ref() == Some(&path) {
            settings.last_folder = None;
        }
        if !settings.hidden_recent_folders.contains(&path) {
            settings.hidden_recent_folders.push(path);
        }
        if let Err(error) = settings.save(&gimage_core::paths::settings_path()) {
            self.toast(format!("无法移除最近文件夹：{error}"), true);
            return;
        }
        self.core.update_settings(settings);
        let core = Arc::clone(&self.core);
        std::thread::spawn(move || actions::publish_library(&core));
    }

    fn export_hint_for(&mut self, index: i32) {
        let label = self
            .app()
            .map(|app| {
                let formats = app.get_export_formats();
                formats
                    .row_data(index as usize)
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let hint = match label.as_str() {
            "WebP" => "WebP 使用无损编码，质量参数不生效",
            "PNG" | "BMP" | "TIFF" => "该格式为无损编码，质量参数不生效",
            _ => "",
        };
        if let Some(app) = self.app() {
            app.set_export_hint(hint.into());
        }
    }

    fn clear_cache(&mut self) {
        match self.core.cache.clear() {
            Ok(()) => {
                self.reset_thumbnails();
                let text = actions::cache_stats_text(&self.core);
                if let Some(app) = self.app() {
                    app.set_cache_stats(text.into());
                }
                self.toast("缩略图缓存已清理".into(), false);
            }
            Err(error) => self.toast(format!("清理缓存失败：{error}"), true),
        }
    }
}

fn actions_sort(settings: &Settings) -> SortSpec {
    settings.sort.clone()
}


// ---------------------------------------------------------------------------
// Callback wiring
// ---------------------------------------------------------------------------
//
// Slint may invoke callbacks synchronously while a model update is in flight
// (row `init`, `changed columns`, ...). Calling back into `RefCell<UiState>`
// there would re-enter the borrow. Callbacks therefore only enqueue an action;
// the UI timer executes queued actions one at a time.

pub type ActionQueue = Rc<RefCell<std::collections::VecDeque<Box<dyn FnOnce(&mut UiState)>>>>;

fn push(queue: &ActionQueue, action: impl FnOnce(&mut UiState) + 'static) {
    if let Ok(mut queue) = queue.try_borrow_mut() {
        queue.push_back(Box::new(action));
    }
}

impl UiState {
    fn set_search(&mut self, text: String) {
        if text == self.filter.query {
            return;
        }
        self.filter.query = text;
        self.pending_reload = Some(Instant::now() + Duration::from_millis(320));
    }

    fn on_columns(&mut self, columns: i32) {
        let columns = (columns.max(1) as usize).min(12);
        if columns != self.columns {
            self.columns = columns;
            self.pending_columns = Some(Instant::now() + Duration::from_millis(120));
        }
    }

    fn reveal(&mut self, id: i64) {
        if let Some(meta) = actions::image_meta_for(&self.core, id) {
            reveal_in_explorer(&meta.path);
        }
    }

    fn copy_to_clipboard(
        &mut self,
        message: &str,
        copy: impl FnOnce(&mut arboard::Clipboard) -> Result<(), arboard::Error>,
    ) {
        let result = (|| {
            if self.clipboard.is_none() {
                self.clipboard = Some(arboard::Clipboard::new()?);
            }
            copy(self.clipboard.as_mut().unwrap())
        })();
        match result {
            Ok(()) => self.toast(message.to_owned(), false),
            Err(error) => self.toast(format!("无法写入剪贴板：{error}"), true),
        }
    }

    fn copy_text(&mut self, text: String) {
        self.copy_to_clipboard("已复制", |clipboard| clipboard.set_text(text));
    }

    fn file_action(&mut self, id: i64, action: &str) {
        let Some(meta) = actions::image_meta_for(&self.core, id) else {
            return;
        };
        match action {
            "context-convert" | "context-rename" => {
                if !self.marked.contains(&id) {
                    self.workbench_file = Some(TaskItem { id, path: meta.path, edits: None, rename_target: None });
                }
                self.set_page(if action == "context-convert" { 5 } else { 6 });
                self.workbench_file = None;
            }
            "preview" => self.open_preview(id),
            "edit" => self.open_editor(id),
            "open" => open_folder_in_shell(&meta.path),
            "open-with" => {
                if let Err(error) = crate::native::open_with(&meta.path) {
                    self.toast(error, true);
                }
            }
            "print" | "wallpaper" => {
                let wallpaper = action == "wallpaper";
                let owner = crate::native::owner();
                let core = Arc::clone(&self.core);
                std::thread::spawn(move || {
                    let result = core.engine.render(&meta.path, &EditParams::default(), None)
                        .map_err(|error| error.user_message())
                        .and_then(|image| if wallpaper {
                            crate::native::wallpaper(&core, &image)
                        } else {
                            crate::native::print(&image, &file_name_of(&meta.path), owner)
                        });
                    match result {
                        Ok(true) => core.toast(if wallpaper { "已设置桌面背景" } else { "已提交打印" }, false),
                        Ok(false) => {}
                        Err(error) => core.toast(error, true),
                    }
                });
            }
            "reveal" => reveal_in_explorer(&meta.path),
            "copy-path" => self.copy_text(display_path(&meta.path)),
            "copy-name" => self.copy_text(file_name_of(&meta.path)),
            "copy-file" => self.copy_to_clipboard("已复制文件", |clipboard| {
                clipboard.set().file_list(&[meta.path])
            }),
            "copy-image" => {
                let core = Arc::clone(&self.core);
                let params = if self.page == 2 {
                    self.draft_for(id).map(|draft| draft.params.clone()).unwrap_or_default()
                } else {
                    EditParams::default()
                };
                std::thread::spawn(move || {
                    let result = core
                        .engine
                        .render(&meta.path, &params, None)
                        .map_err(|error| error.user_message());
                    core.emit(AppEvent::CopyImage(result));
                });
            }
            "export" => {
                self.select(id);
                self.open_export_copy();
            }
            _ => {}
        }
    }

    fn edit_param(&mut self, field: &str, value: f32) {
        match field {
            "open-editor" => {
                if let Some(id) = self.selected {
                    self.open_editor(id);
                }
            }
            "exposure" => self.apply_params(|params| params.exposure = value),
            "contrast" => self.apply_params(|params| params.contrast = value),
            "saturation" => self.apply_params(|params| params.saturation = value),
            "temperature" => self.apply_params(|params| params.temperature = value),
            "crop-mode" => {
                if let Some(app) = self.app() {
                    app.set_crop_mode(value > 0.5);
                }
            }
            _ => {}
        }
    }

    fn cancel_task(&mut self, id: i64) {
        if self.core.task_cancel(id as u64) {
            self.toast("已请求取消，正在等待当前图片处理完成".into(), false);
        }
    }

    fn retry_task(&mut self, id: i64) {
        let core = Arc::clone(&self.core);
        actions::retry_task(&core, id as u64);
    }

    fn clear_finished_tasks(&mut self) {
        let core = &self.core;
        self.tasks.retain(|task| {
            matches!(task.state, 0 | 1 | 3 | 4)
                || (task.state == 6 && !core.clear_interrupted_task(task.id))
        });
        self.rebuild_task_model();
    }

    fn set_theme(&mut self, dark: bool) {
        self.ui.global::<Theme>().set_dark(dark);
        let mut settings = self.core.settings_snapshot();
        settings.theme = if dark {
            gimage_core::Theme::Dark
        } else {
            gimage_core::Theme::Light
        };
        self.core.update_settings(settings.clone());
        let _ = settings.save(&gimage_core::paths::settings_path());
    }

    fn open_data_folder(&mut self) {
        let dir = self.core.data_dir().to_path_buf();
        let _ = gimage_core::paths::ensure_dir(&dir);
        reveal_in_explorer(&dir);
    }

    fn cancel_confirm(&mut self) {
        self.pending_confirm = None;
        if let Some(app) = self.app() {
            app.set_confirm_open(false);
            app.set_confirm_discard(false);
        }
    }

    fn apply_crop_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let a = MarkPoint { x, y }.to_source(&self.edit.params);
        let b = MarkPoint { x: x + w, y: y + h }.to_source(&self.edit.params);
        self.apply_params(|params| {
            params.crop = Some(CropRect {
                x: a.x.min(b.x),
                y: a.y.min(b.y),
                width: (a.x - b.x).abs(),
                height: (a.y - b.y).abs(),
            });
        });
        if let Some(app) = self.app() {
            app.set_crop_mode(false);
        }
    }

    fn toggle_crop_mode(&mut self) {
        self.cancel_mark();
        if let Some(app) = self.app() { app.set_mark_tool(0); }
        let next = !self.app().map(|app| app.get_crop_mode()).unwrap_or(false);
        if next {
            let had_crop = self.edit.params.crop.is_some();
            self.apply_params(|params| params.crop = None);
            if had_crop {
                self.edit_render_now();
            }
        }
        if let Some(app) = self.app() {
            app.set_crop_mode(next);
        }
    }
}

pub fn wire(ui: &MainWindow, queue: ActionQueue) {
    let app = ui.global::<App>();
    {
        let queue = Rc::clone(&queue);
        app.on_mark_tool_changed(move |tool| push(&queue, move |s| s.select_mark_tool(tool)));
    }
    {
        let queue = Rc::clone(&queue);
        app.on_mark_event(move |event, x, y, w, h, shift| {
            push(&queue, move |s| s.mark_event(event, x, y, w, h, shift))
        });
    }

    {
        let queue = Rc::clone(&queue);
        app.on_file_action(move |id, action| {
            push(&queue, move |s| {
                s.file_action(i64::from(id), action.as_str())
            })
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_folder_action(move |path, action| {
            push(&queue, move |s| {
                let path = PathBuf::from(path.as_str());
                if action == "copy-path" {
                    s.copy_text(display_path(&path));
                }
                if action == "open" {
                    open_folder_in_shell(&path);
                }
                if action == "remove" {
                    s.remove_recent_folder(path);
                }
            })
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_copy_text(move |value| push(&queue, move |s| s.copy_text(value.to_string())));
    }

    macro_rules! simple {
        ($method:ident, |$state:ident| $body:expr) => {{
            let queue = Rc::clone(&queue);
            app.$method(move || {
                let queue = Rc::clone(&queue);
                push(&queue, |$state: &mut UiState| $body);
            });
        }};
    }

    simple!(on_open_folder, |s| s.open_folder_dialog());
    simple!(on_open_image, |s| s.open_image_dialog());
    simple!(on_save_edit, |s| s.confirm_save_edit());
    simple!(on_exit_requested, |s| s.request_exit());
    simple!(on_exif_requested, |s| s.read_exif());
    {
        let queue = Rc::clone(&queue);
        app.on_preview_detail_changed(move |detail| {
            push(&queue, move |s| s.set_preview_detail(detail));
        });
    }
    simple!(on_choose_mark_color, |s| {
        let app = s.ui.global::<App>();
        if let Some(color) = crate::native::choose_color(app.get_mark_ink()) {
            app.set_mark_ink(color);
            app.set_mark_color(-1);
        }
    });
    simple!(on_refresh, |s| s.refresh());
    simple!(on_load_more, |s| s.load_more());
    simple!(on_undo, |s| s.undo());
    simple!(on_redo, |s| s.redo());
    simple!(on_prev_image, |s| s.navigate(-1));
    simple!(on_next_image, |s| s.navigate(1));
    simple!(on_export_current, |s| s.open_export_copy());
    simple!(on_open_export, |s| s.open_export_copy());
    simple!(on_open_batch_export, |s| s.set_page(5));
    simple!(on_select_all, |s| s.select_all_browser());
    simple!(on_clear_marked, |s| {
        s.set_marked(HashSet::new());
        s.selection_anchor = None;
        s.clear_selection();
    });
    {
        let queue = Rc::clone(&queue);
        app.on_scan_depth_changed(move |depth| push(&queue, move |s| s.change_scan_depth(depth)));
    }
    {
        let queue = Rc::clone(&queue);
        app.on_extend_marked(move |step| push(&queue, move |s| s.extend_browser_selection(step)));
    }
    simple!(on_convert_pick_folder, |s| s.choose_conversion_directory());
    simple!(on_convert_open_folder, |s| s.open_conversion_directory());
    simple!(on_convert_reset_folder, |s| s.reset_conversion_directory());
    simple!(on_convert_start, |s| s.confirm_conversion());
    simple!(on_convert_changed, |s| s.schedule_conversion_preview());
    {
        let queue = Rc::clone(&queue);
        app.on_convert_preset(move |index| push(&queue, move |s| s.select_conversion_preset(index)));
    }
    {
        let queue = Rc::clone(&queue);
        app.on_convert_select(move |index| {
            push(&queue, move |s| s.select_conversion(index));
        });
    }
    simple!(on_open_rename, |s| s.open_rename());
    simple!(on_rename_changed, |s| s.schedule_rename_preview());
    simple!(on_rename_add_files, |s| s.rename_add_files());
    simple!(on_rename_apply, |s| s.confirm_rename_plan());
    simple!(on_settings_apply, |s| s.settings_apply());
    simple!(on_check_updates, |s| s.check_updates());
    simple!(on_cache_clear, |s| s.clear_cache());
    simple!(on_open_data_folder, |s| s.open_data_folder());
    simple!(on_file_associations, |s| {
        if let Err(error) = crate::native::file_associations() {
            s.toast(error, true);
        }
    });
    simple!(on_confirm_accepted, |s| s.confirm_accepted());
    simple!(on_confirm_discarded, |s| s.discard_edit_and_leave());
    simple!(on_confirm_canceled, |s| s.cancel_confirm());
    simple!(on_tasks_clear_finished, |s| s.clear_finished_tasks());
    simple!(on_rotate_cw, |s| s.apply_params(|params| params.rotation = params.rotation.rotate_cw()));
    simple!(on_rotate_ccw, |s| s.apply_params(|params| params.rotation = params.rotation.rotate_ccw()));
    simple!(on_flip_horizontal, |s| s.apply_params(|params| params.flip_horizontal = !params.flip_horizontal));
    simple!(on_flip_vertical, |s| s.apply_params(|params| params.flip_vertical = !params.flip_vertical));
    simple!(on_clear_crop, |s| s.apply_params(|params| params.crop = None));
    simple!(on_toggle_crop_mode, |s| s.toggle_crop_mode());
    simple!(on_reset_edits, |s| {
        if s.edit.id >= 0 && s.saving.is_none() {
            s.cancel_mark();
            s.edit.params = EditParams::default();
            s.push_history();
            s.sync_params_ui();
            s.schedule_edit_render();
        }
    });

    {
        let queue = Rc::clone(&queue);
        app.on_page_requested(move |page| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.set_page(page));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_open_folder_path(move |path| {
            let queue = Rc::clone(&queue);
            let path = PathBuf::from(path.to_string());
            push(&queue, move |s| s.open_folder(path));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_search_changed(move |text| {
            let queue = Rc::clone(&queue);
            let text = text.to_string();
            push(&queue, move |s| s.set_search(text));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_sort_changed(move |index, descending| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.update_sort(index, descending));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_favorites_filter_changed(move |only| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.set_favorites_filter(only));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_columns_changed(move |columns| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.on_columns(columns));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_format_chip_clicked(move |label| {
            let queue = Rc::clone(&queue);
            let label = label.to_string();
            push(&queue, move |s| s.toggle_format_filter(&label));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_browser_viewport(move |first, end| {
            push(&queue, move |s| {
                if s.page == 0 {
                    s.update_thumbnail_viewport(first.max(0) as usize..end.max(first).max(0) as usize);
                }
            });
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_cell_clicked(move |id, mode| {
            push(&queue, move |s| s.select_browser_cell(id as i64, mode));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_cell_double_clicked(move |id| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.open_preview(id as i64));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_star_clicked(move |id| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.toggle_favorite(id as i64));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_reveal_in_explorer(move |id| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.reveal(id as i64));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_edit_param_changed(move |field, value| {
            let queue = Rc::clone(&queue);
            let field = field.to_string();
            push(&queue, move |s| s.edit_param(&field, value as f32));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_apply_crop(move |x, y, w, h| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.apply_crop_rect(x, y, w, h));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_crop_preset(move |preset| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.crop_preset(preset));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_export_format_changed(move |index| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.export_hint_for(index));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_export_confirm(move |format, quality, max_w, max_h, policy| {
            let queue = Rc::clone(&queue);
            let format = format.to_string();
            push(&queue, move |s| {
                s.export_confirm(&format, quality, max_w, max_h, policy)
            });
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_task_cancel(move |id| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.cancel_task(id as i64));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_task_retry(move |id| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.retry_task(id as i64));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_theme_toggled(move |dark| {
            let queue = Rc::clone(&queue);
            push(&queue, move |s| s.set_theme(dark));
        });
    }
    {
        let queue = Rc::clone(&queue);
        app.on_toast_shown(move |message, error| {
            let queue = Rc::clone(&queue);
            let message = message.to_string();
            push(&queue, move |s| s.toast(message, error));
        });
    }
}

pub fn setup_initial_library(state: &mut UiState, startup_path: Option<PathBuf>, workbench: Option<i32>) {
    actions::publish_library(&state.core);
    if workbench == Some(6) {
        state.set_page(6);
        if let Some(path) = startup_path {
            state.rename_open_path(path);
        }
        return;
    }
    if let Some(path) = startup_path.as_ref().filter(|path| path.is_file()) {
        state.open_indexed_image(path.clone(), workbench);
        return;
    }
    if let (Some(path), Some(page)) = (&startup_path, workbench) {
        if path.is_dir() {
            state.open_folder(path.clone());
            state.set_page(page);
        } else {
            state.toast("找不到所选文件或文件夹".into(), true);
        }
        return;
    }
    let settings = state.core.settings_snapshot();
    let folder = startup_path
        .filter(|folder| folder.is_dir())
        .or_else(|| settings.last_folder.clone().filter(|folder| folder.is_dir()));
    if let Some(folder) = folder {
        state.open_folder(folder);
    }
}
