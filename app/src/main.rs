#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod core;
mod events;
mod folder_watch;
mod marks;
mod native;
mod preview_buffer;
mod task_journal;
mod thumbnails;
mod updater;
mod ui;
mod util;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gimage_cache::ThumbnailCache;
use gimage_catalog::Catalog;
use gimage_core::{EngineOptions, Settings, paths};
use gimage_engine::RustImageEngine;
use slint::ComponentHandle;

use crate::core::AppCore;

slint::include_modules!();

#[cfg(all(test, debug_assertions))]
mod ui_tests;

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
    tracing::info!("starting GImage");
    #[cfg(windows)]
    let _apartment = native::UiApartment::new()?;
    #[cfg(all(windows, target_env = "msvc"))]
    if std::env::var_os("SLINT_BACKEND").is_none() {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("skia".into())
            .select()?;
    }

    let settings_path = paths::settings_path();
    let (settings, settings_warning) = Settings::load_or_default(&settings_path);
    paths::ensure_dir(&paths::app_data_dir())?;
    paths::ensure_dir(&paths::thumbnail_cache_dir())?;
    paths::ensure_dir(&paths::log_dir())?;

    let db_path = paths::catalog_db_path();
    let catalog = match Catalog::open(&db_path) {
        Ok(catalog) => catalog,
        Err(error) => {
            tracing::error!(%error, "failed to open catalog, using in-memory index");
            Catalog::open_in_memory()?
        }
    };

    let engine = Arc::new(RustImageEngine::new(EngineOptions {
        max_decode_pixels: settings.max_decode_pixels,
    }));
    let cache = Arc::new(ThumbnailCache::new(
        paths::thumbnail_cache_dir(),
        settings.thumbnail_memory_bytes(),
    )?);
    if let Ok(freed) = cache.set_disk_budget(settings.disk_cache_bytes()) {
        if freed > 0 {
            tracing::info!(freed, "pruned thumbnail cache");
        }
    }

    let (event_tx, event_rx) = crossbeam_channel::unbounded::<events::AppEvent>();
    let (thumb_tx, thumb_rx) = crossbeam_channel::unbounded::<core::ThumbRequest>();
    let (render_tx, render_rx) = crossbeam_channel::unbounded();
    let (probe_tx, probe_rx) = crossbeam_channel::unbounded::<(i64, std::path::PathBuf)>();

    let core = Arc::new(AppCore::new(
        engine,
        cache,
        catalog,
        settings.clone(),
        event_tx,
        event_rx,
        thumb_tx,
        render_tx,
        probe_tx,
        paths::app_data_dir(),
        paths::thumbnail_cache_dir(),
        db_path,
    ));
    core.restore_interrupted_tasks();

    let ui = MainWindow::new()?;
    {
        let weak = ui.as_weak();
        ui.on_minimize_window(move || {
            if let Some(ui) = weak.upgrade() {
                ui.window().set_minimized(true);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_toggle_fullscreen(move || {
            if let Some(ui) = weak.upgrade() {
                let fullscreen = !ui.window().is_fullscreen();
                ui.window().set_fullscreen(fullscreen);
                let app = ui.global::<App>();
                app.set_fullscreen(fullscreen);
                app.set_preview_chrome(true);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_toggle_maximize(move || {
            if let Some(ui) = weak.upgrade() {
                let maximized = !ui.window().is_maximized();
                ui.window().set_maximized(maximized);
                ui.set_window_maximized(maximized);
            }
        });
    }
    let state = Rc::new(RefCell::new(ui::UiState::new(Arc::clone(&core), &ui)));
    {
        let state = Rc::downgrade(&state);
        ui.window().on_close_requested(move || {
            use slint::CloseRequestResponse::*;
            if let Some(state) = state.upgrade()
                && let Ok(mut state) = state.try_borrow_mut()
                && state.can_exit()
            {
                return HideWindow;
            }
            KeepWindowShown
        });
    }
    let queue: ui::ActionQueue = Rc::new(RefCell::new(std::collections::VecDeque::new()));
    ui::wire(&ui, Rc::clone(&queue));
    ui::drop_files::wire(&ui, Rc::clone(&queue));
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    let (startup_path, workbench) = match first.as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("--gimage-browse") => (arguments.next(), None),
        Some("--gimage-convert") => (arguments.next(), Some(5)),
        Some("--gimage-rename") => (arguments.next(), Some(6)),
        _ => (first, None),
    };
    {
        let mut state = state.borrow_mut();
        ui::setup_initial_library(&mut state, startup_path.map(std::path::PathBuf::from), workbench);
        state.tick();
    }

    thumbnails::spawn_thumbnail_workers(Arc::clone(&core), thumb_rx, 2);
    thumbnails::spawn_render_worker(Arc::clone(&core), render_rx);
    preview_buffer::spawn_prefetch_worker(Arc::clone(&core));
    thumbnails::spawn_probe_worker(Arc::clone(&core), probe_rx);

    // Callbacks enqueue actions (Slint can invoke them re-entrantly during model
    // updates); the timer drains actions and then applies queued background
    // events, all on the UI thread.
    let timer = slint::Timer::default();
    {
        let state = Rc::clone(&state);
        let queue = Rc::clone(&queue);
        let window = ui.as_weak();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                loop {
                    let action = queue.borrow_mut().pop_front();
                    let Some(action) = action else { break };
                    action(&mut state.borrow_mut());
                }
                state.borrow_mut().tick();
                if let Some(ui) = window.upgrade() {
                    let app = ui.global::<App>();
                    let scale = ui.window().scale_factor();
                    if app.get_dpi_scale() != scale {
                        app.set_dpi_scale(scale);
                    }
                    let maximized = ui.window().is_maximized();
                    if ui.get_window_maximized() != maximized {
                        ui.set_window_maximized(maximized);
                    }
                }
            },
        );
    }

    if let Some(warning) = settings_warning {
        core.toast(format!("设置读取失败，已使用默认值：{warning}"), true);
    }

    ui.run()?;

    // Persist window-independent settings on exit.
    let settings = state.borrow().core.settings_snapshot();
    if let Err(error) = settings.save(&settings_path) {
        tracing::warn!(%error, "failed to save settings on exit");
    }
    tracing::info!("GImage closed");
    Ok(())
}
