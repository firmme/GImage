use super::*;
use crate::ConvertRow;
use gimage_core::{ResizeMode, Rotation};

const RESOLUTIONS: [(i32, i32); 20] = [
    (320, 240),
    (480, 320),
    (640, 360),
    (640, 480),
    (720, 480),
    (720, 576),
    (800, 600),
    (854, 480),
    (960, 540),
    (1024, 768),
    (1280, 720),
    (1280, 800),
    (1366, 768),
    (1440, 900),
    (1600, 900),
    (1920, 1080),
    (1920, 1200),
    (2560, 1440),
    (3840, 2160),
    (7680, 4320),
];

pub(super) fn resolution_presets() -> ModelRc<slint::SharedString> {
    let mut labels = vec!["常用分辨率…".into()];
    labels.extend(
        RESOLUTIONS
            .iter()
            .map(|(width, height)| format!("{width} × {height}").into()),
    );
    ModelRc::new(VecModel::from(labels))
}

#[derive(Default)]
pub(super) struct ConversionState {
    files: Vec<TaskItem>,
    token: u64,
    selected: Option<usize>,
    render_seq: u64,
    render_due: Option<Instant>,
    preview_key: Option<(
        usize,
        Rotation,
        ResizeMode,
        Option<gimage_core::ResizeSpec>,
        ExportFormat,
    )>,
    directory: Option<PathBuf>,
    pub(super) waiting_for_scan: bool,
    initial_loading: bool,
    drops_pending: usize,
}

impl UiState {
    pub(super) fn select_conversion_preset(&mut self, index: i32) {
        if index < 1 {
            return;
        }
        if let Some(&(width, height)) = RESOLUTIONS.get((index - 1) as usize) {
            let app = self.ui.global::<App>();
            app.set_convert_max_w(width);
            app.set_convert_max_h(height);
            if app.get_convert_resize_mode() == 0 {
                app.set_convert_resize_mode(1);
            }
            self.schedule_conversion_preview();
        }
    }

    pub(super) fn load_conversion(&mut self) {
        let app = self.ui.global::<App>();
        app.set_convert_error("".into());
        let token = self.conversion.token;
        if let Some(item) = self.workbench_file.take() {
            app.set_convert_selected_only(true);
            self.conversion_loaded(token, Ok(vec![item]));
            return;
        }
        let Some(mut session) = self.core.current_session() else {
            app.set_convert_loading(false);
            return;
        };
        if app.get_scanning() {
            self.conversion.waiting_for_scan = true;
            self.conversion.initial_loading = true;
            app.set_convert_loading(true);
            return;
        }
        self.conversion.waiting_for_scan = false;
        self.conversion.initial_loading = true;
        let marked = self.marked.clone();
        app.set_convert_selected_only(!marked.is_empty());
        app.set_convert_loading(true);
        let core = Arc::clone(&self.core);
        session.filter = ImageFilter {
            max_directory_depth: session.filter.max_directory_depth,
            ..ImageFilter::default()
        };
        let started = std::thread::Builder::new()
            .name("gimage-conversion-list".into())
            .spawn(move || {
                let items =
                    crate::core::collect_filtered_images(&core, &session, 0).map(|images| {
                        images
                            .into_iter()
                            .filter(|image| marked.is_empty() || marked.contains(&image.id))
                            .map(|image| TaskItem {
                                id: image.id,
                                path: image.path,
                                edits: None,
                                rename_target: None,
                            })
                            .collect()
                    });
                core.emit(AppEvent::ConversionLoaded { token, items });
            });
        if let Err(error) = started {
            self.conversion_loaded(token, Err(error.to_string()));
        }
    }

    pub(super) fn conversion_loaded(&mut self, token: u64, items: Result<Vec<TaskItem>, String>) {
        if self.page != 5 || self.conversion.token != token {
            return;
        }
        let app = self.ui.global::<App>();
        self.conversion.initial_loading = false;
        app.set_convert_loading(self.conversion.drops_pending > 0);
        let files = match items {
            Ok(files) => files,
            Err(error) => {
                app.set_convert_error(format!("无法读取文件列表：{error}").into());
                return;
            }
        };
        self.append_conversion_files(files);
    }

    pub(super) fn conversion_drop_paths(&mut self, paths: Vec<PathBuf>) {
        let token = self.conversion.token;
        let core = Arc::clone(&self.core);
        let settings = core.settings_snapshot();
        self.conversion.drops_pending += 1;
        self.ui.global::<App>().set_convert_loading(true);
        let started = std::thread::Builder::new()
            .name("gimage-drop-conversion".into())
            .spawn(move || {
                let items = super::drop_files::collect_paths(
                    paths,
                    settings.effective_scan_depth(),
                    settings.include_hidden,
                    true,
                )
                .map(|paths| {
                    let catalog = core.catalog.lock();
                    paths
                        .into_iter()
                        .map(|path| {
                            let id = catalog
                                .image_at_path(&path)
                                .ok()
                                .flatten()
                                .map_or(-1, |image| image.id);
                            TaskItem {
                                id,
                                path,
                                edits: None,
                                rename_target: None,
                            }
                        })
                        .collect()
                });
                core.emit(AppEvent::DroppedConversion { token, items });
            });
        if let Err(error) = started {
            self.conversion_dropped(token, Err(error.to_string()));
        }
    }

    pub(super) fn conversion_dropped(&mut self, token: u64, items: Result<Vec<TaskItem>, String>) {
        if self.page != 5 || self.conversion.token != token {
            return;
        }
        self.conversion.drops_pending = self.conversion.drops_pending.saturating_sub(1);
        self.ui.global::<App>().set_convert_loading(
            self.conversion.initial_loading || self.conversion.drops_pending > 0,
        );
        match items {
            Ok(files) => self.append_conversion_files(files),
            Err(error) => self.toast(error, true),
        }
    }

    fn append_conversion_files(&mut self, files: Vec<TaskItem>) {
        let mut seen: HashSet<_> = self
            .conversion
            .files
            .iter()
            .map(|item| item.path.clone())
            .collect();
        let mut files: Vec<_> = files
            .into_iter()
            .filter(|item| seen.insert(item.path.clone()))
            .collect();
        if files.is_empty() {
            return;
        }
        for item in &mut files {
            item.edits = self.draft_for(item.id).map(EditState::export_edits);
        }
        self.conversion.files.extend(files);
        let rows: Vec<_> = self
            .conversion
            .files
            .iter()
            .map(|item| ConvertRow {
                name: file_name_of(&item.path).into(),
                path: display_path(&item.path).into(),
            })
            .collect();
        let initial = self
            .selected
            .and_then(|id| self.conversion.files.iter().position(|item| item.id == id))
            .unwrap_or(0);
        let app = self.ui.global::<App>();
        app.set_convert_files(ModelRc::new(VecModel::from(rows)));
        app.set_convert_error("".into());
        if self.conversion.selected.is_none() {
            self.select_conversion(initial as i32);
        }
    }

    pub(super) fn leave_conversion_if_open(&mut self) {
        if self.page != 5 {
            return;
        }
        self.conversion.files.clear();
        self.conversion.token = self.conversion.token.wrapping_add(1);
        self.conversion.initial_loading = false;
        self.conversion.drops_pending = 0;
        self.conversion.selected = None;
        self.conversion.render_due = None;
        self.conversion.preview_key = None;
        self.conversion.render_seq = 0;
        self.conversion.waiting_for_scan = false;
        let app = self.ui.global::<App>();
        app.set_convert_loading(false);
        app.set_convert_files(ModelRc::default());
        app.set_convert_index(-1);
        app.set_convert_preview(slint::Image::default());
        app.set_convert_output_name("".into());
        app.set_convert_preview_status("".into());
    }

    pub(super) fn select_conversion(&mut self, index: i32) {
        if self.page != 5
            || index < 0
            || index as usize >= self.conversion.files.len()
            || self.conversion.selected == Some(index as usize)
        {
            return;
        }
        self.conversion.selected = Some(index as usize);
        let app = self.ui.global::<App>();
        app.set_convert_index(index);
        app.set_convert_preview(slint::Image::default());
        app.set_convert_preview_status("正在生成预览…".into());
        self.schedule_conversion_preview();
    }

    fn conversion_options(&self) -> ExportOptions {
        let app = self.ui.global::<App>();
        let format = app
            .get_export_formats()
            .row_data(app.get_export_format_index().max(0) as usize)
            .map_or(ExportFormat::Jpeg, |label| {
                actions::export_format_from_label(label.as_str())
            });
        let mode = match app.get_convert_resize_mode() {
            1 => ResizeMode::Contain,
            2 => ResizeMode::FitWidth,
            3 => ResizeMode::Pad,
            4 => ResizeMode::Crop,
            5 => ResizeMode::Stretch,
            _ => ResizeMode::Original,
        };
        let resize = mode != ResizeMode::Original;
        let mut options = actions::export_options_from_ui(
            format,
            (app.get_convert_quality() * 100.).round() as u8,
            if resize {
                app.get_convert_max_w().max(1) as u32
            } else {
                0
            },
            if resize {
                app.get_convert_max_h().max(1) as u32
            } else {
                0
            },
            actions::policy_from_index(app.get_convert_policy()),
        );
        options.resize_mode = mode;
        options.rotation = match app.get_convert_rotation() {
            1 => Rotation::Cw90,
            2 => Rotation::Cw180,
            3 => Rotation::Cw270,
            _ => Rotation::None,
        };
        options.directory = self.conversion.directory.clone();
        options.prefix = app.get_convert_prefix().trim().to_owned();
        options
    }

    pub(super) fn schedule_conversion_preview(&mut self) {
        let Some(index) = self.conversion.selected else {
            return;
        };
        let options = self.conversion_options();
        let destination = export_destination(&self.conversion.files[index].path, &options);
        let app = self.ui.global::<App>();
        app.set_convert_output_name(file_name_of(&destination).into());
        let key = (
            index,
            options.rotation,
            options.resize_mode,
            options.resize,
            options.format,
        );
        if self.conversion.preview_key != Some(key) {
            self.conversion.preview_key = Some(key);
            self.conversion.render_seq = 0;
            self.conversion.render_due = Some(Instant::now() + Duration::from_millis(120));
        }
    }

    pub(super) fn tick_conversion(&mut self, now: Instant) {
        if self.page != 5 || !self.conversion.render_due.is_some_and(|due| now >= due) {
            return;
        }
        self.conversion.render_due = None;
        let options = self.conversion_options();
        if let Some(item) = self
            .conversion
            .selected
            .and_then(|index| self.conversion.files.get(index))
        {
            let params = item
                .edits
                .as_ref()
                .map(|edits| edits.params.clone())
                .unwrap_or_else(|| actions::load_edit_params(&self.core, item.id));
            self.conversion.render_seq = actions::queue_render(
                &self.core,
                RenderKind::Conversion,
                item.id,
                item.path.clone(),
                params,
                Some(1280),
                Some(options),
            );
        }
    }

    pub(super) fn conversion_preview_ready(
        &mut self,
        seq: u64,
        image: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    ) {
        if self.page == 5 && self.conversion.render_seq == seq {
            let app = self.ui.global::<App>();
            app.set_convert_preview(slint::Image::from_rgba8(image));
            app.set_convert_preview_status("".into());
        }
    }

    pub(super) fn conversion_preview_failed(&mut self, seq: u64, error: String) {
        if self.page == 5 && self.conversion.render_seq == seq {
            let app = self.ui.global::<App>();
            app.set_convert_preview(slint::Image::default());
            app.set_convert_preview_status(error.into());
        }
    }

    pub(super) fn choose_conversion_directory(&mut self) {
        if self.page != 5 {
            return;
        }
        let mut dialog = rfd::FileDialog::new().set_title("选择输出目录");
        if let Some(path) = self
            .conversion
            .directory
            .clone()
            .or_else(|| self.core.current_session().map(|s| s.path))
        {
            dialog = dialog.set_directory(path);
        }
        if let Some(path) = dialog.pick_folder() {
            self.ui
                .global::<App>()
                .set_convert_directory(display_path(&path).into());
            self.conversion.directory = Some(path);
            self.schedule_conversion_preview();
        }
    }

    pub(super) fn reset_conversion_directory(&mut self) {
        self.conversion.directory = None;
        self.ui.global::<App>().set_convert_directory("".into());
        self.schedule_conversion_preview();
    }

    pub(super) fn open_conversion_directory(&mut self) {
        if let Some(path) = self
            .conversion
            .directory
            .clone()
            .or_else(|| self.core.current_session().map(|s| s.path))
        {
            open_folder_in_shell(&path);
        }
    }

    pub(super) fn confirm_conversion(&mut self) {
        if self.page != 5 || self.pending_confirm.is_some() || self.conversion.files.is_empty() {
            return;
        }
        let options = self.conversion_options();
        if options
            .prefix
            .chars()
            .any(|ch| ch.is_control() || "<>:\"/\\|?*".contains(ch))
        {
            self.toast("文件名前缀含有 Windows 文件名禁用字符".into(), true);
            return;
        }
        if let Some(dir) = &options.directory
            && !dir.is_dir()
        {
            self.toast("输出目录已不可用，请重新选择".into(), true);
            return;
        }
        if options.output == OutputPolicy::Overwrite {
            self.pending_confirm = Some(PendingConfirm::Conversion(options));
            let app = self.ui.global::<App>();
            app.set_confirm_title("覆盖输出目录中的同名文件？".into());
            app.set_confirm_body("同名的现有输出文件将被替换。此批次的源文件会保留。".into());
            app.set_confirm_label("覆盖并转换".into());
            app.set_confirm_danger(true);
            app.set_confirm_open(true);
        } else {
            self.start_conversion(options);
        }
    }

    pub(super) fn start_conversion(&mut self, options: ExportOptions) {
        if self.page != 5 || self.conversion.files.is_empty() {
            return;
        }
        let items = std::mem::take(&mut self.conversion.files);
        let title = format!("转换 {} 张图片", items.len());
        actions::start_export(&self.core, items, options, title);
        self.set_page(3);
    }
}
