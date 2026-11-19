use std::{cell::RefCell, rc::Rc, sync::Arc};

use gimage_cache::ThumbnailCache;
use gimage_catalog::{Catalog, ScanOptions, scan_folder};
use gimage_core::{
    AnnotationKind, CancellationToken, EditParams, EngineOptions, ExportFormat, ImageEngine,
    RgbaImage, Settings, StrokeStyle,
};
use gimage_engine::RustImageEngine;
use i_slint_backend_testing::ElementHandle;
use slint::platform::{Key, PointerEventButton as Button, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, PhysicalSize, VecModel};

use crate::{App, GridRow, ImageCell, MainWindow, actions, core::AppCore, ui};

fn window() -> MainWindow {
    i_slint_backend_testing::init_no_event_loop();
    let window = MainWindow::new().unwrap();
    window.window().set_size(PhysicalSize::new(1360, 860));
    window.show().unwrap();
    window
}

fn pointer(window: &MainWindow, button: Button, from: LogicalPosition, to: LogicalPosition) {
    let window = window.window();
    window.dispatch_event(WindowEvent::PointerMoved { position: from });
    window.dispatch_event(WindowEvent::PointerPressed {
        position: from,
        button,
    });
    window.dispatch_event(WindowEvent::PointerMoved { position: to });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: to,
        button,
    });
}

#[test]
fn right_click_opens_the_clicked_file_menu() {
    let window = window();
    let app = window.global::<App>();
    app.set_folder_path("E:\\Pictures".into());
    let cell = ImageCell {
        id: 42,
        name: "test.png".into(),
        ..Default::default()
    };
    let row = GridRow {
        cells: ModelRc::new(Rc::new(VecModel::from(vec![cell]))),
    };
    app.set_grid_rows(ModelRc::new(Rc::new(VecModel::from(vec![row]))));
    let received = Rc::new(RefCell::new(None));
    let capture = Rc::clone(&received);
    app.on_file_action(move |id, action| *capture.borrow_mut() = Some((id, action)));
    let at = LogicalPosition::new(284.0, 165.0);
    pointer(&window, Button::Right, at, at);
    let copy = ElementHandle::find_by_accessible_label(&window, "复制完整路径")
        .next()
        .expect("right click should show file actions");
    copy.mock_single_click(Button::Left);
    assert_eq!(*received.borrow(), Some((42, "copy-path".into())));
}

#[test]
fn editor_keeps_drafts_until_confirmed_save() {
    let temp = tempfile::tempdir().unwrap();
    let gallery = temp.path().join("gallery");
    std::fs::create_dir(&gallery).unwrap();
    let source = gallery.join("source.png");
    let engine = Arc::new(RustImageEngine::new(EngineOptions::default()));
    engine
        .encode(
            &RgbaImage::new(120, 120, [35, 35, 35, 255].repeat(120 * 120)),
            ExportFormat::Png,
            90,
            &source,
        )
        .unwrap();
    let original = std::fs::read(&source).unwrap();
    let mut catalog = Catalog::open_in_memory().unwrap();
    let folder = catalog.upsert_folder(&gallery).unwrap();
    let scan = scan_folder(
        &gallery,
        &ScanOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .unwrap();
    catalog.sync_scan(folder, &scan).unwrap();
    let image = catalog
        .images_page(folder, &Default::default(), &Default::default(), 0, 10)
        .unwrap()
        .items[0]
        .id;

    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let (thumb_tx, _thumb_rx) = crossbeam_channel::unbounded();
    let (render_tx, render_rx) = crossbeam_channel::unbounded();
    let (probe_tx, _probe_rx) = crossbeam_channel::unbounded();
    let cache_dir = temp.path().join("thumbs");
    let core = Arc::new(AppCore::new(
        engine,
        Arc::new(ThumbnailCache::new(cache_dir.clone(), 1024 * 1024).unwrap()),
        catalog,
        Settings::default(),
        event_tx,
        event_rx,
        thumb_tx,
        render_tx,
        probe_tx,
        temp.path().to_path_buf(),
        cache_dir,
        temp.path().join("catalog.sqlite3"),
    ));
    let last_render = || {
        render_rx
            .try_iter()
            .flatten()
            .last()
            .expect("editor render requested")
            .params
    };
    let window = window();
    let mut state = ui::UiState::new(Arc::clone(&core), &window);
    let queue: ui::ActionQueue = Rc::new(RefCell::new(Default::default()));
    ui::wire(&window, Rc::clone(&queue));
    let app = window.global::<App>();
    let pump = |state: &mut ui::UiState| loop {
        let action = queue.borrow_mut().pop_front();
        let Some(action) = action else { break };
        action(state);
    };
    app.invoke_file_action(image as i32, "edit".into());
    pump(&mut state);
    app.set_edit_w(120);
    app.set_edit_h(120);
    let position = |x: f32, y: f32| {
        let canvas = ElementHandle::find_by_element_id(&window, "EditorPage::drawing")
            .next()
            .expect("active drawing surface");
        let start = canvas.absolute_position();
        let size = canvas.size();
        LogicalPosition::new(start.x + x * size.width, start.y + y * size.height)
    };
    app.set_mark_ink(slint::Color::from_rgb_u8(112, 181, 220));
    app.set_mark_style(1);
    for (index, kind, y) in [
        (1, AnnotationKind::Pen, 0.12),
        (2, AnnotationKind::Highlight, 0.22),
        (3, AnnotationKind::Line, 0.32),
        (4, AnnotationKind::Arrow, 0.42),
        (6, AnnotationKind::Mosaic, 0.52),
    ] {
        app.invoke_mark_tool_changed(index);
        pump(&mut state);
        pointer(
            &window,
            Button::Left,
            position(0.2, y),
            position(0.6, y + 0.035),
        );
        pump(&mut state);
        let params = last_render();
        let mark = params.annotations.last().unwrap();
        assert_eq!(mark.kind, kind);
        assert_eq!(mark.color, [112, 181, 220]);
        if matches!(kind, AnnotationKind::Line | AnnotationKind::Arrow) {
            assert_eq!(mark.style, StrokeStyle::Dashed);
        }
        assert!(actions::load_edit_params(&core, image).is_identity());
    }
    app.invoke_mark_tool_changed(5);
    app.set_mark_shape(0);
    pump(&mut state);
    for (x, y) in [(0.22, 0.65), (0.46, 0.64), (0.46, 0.78)] {
        let at = position(x, y);
        pointer(&window, Button::Left, at, at);
        pump(&mut state);
    }
    let at = position(0.44, 0.75);
    pointer(&window, Button::Right, at, at);
    pump(&mut state);
    assert_eq!(
        last_render().annotations.last().unwrap().kind,
        AnnotationKind::Polygon
    );

    app.invoke_mark_tool_changed(7);
    pump(&mut state);
    app.set_mark_text("测试".into());
    let at = position(0.2, 0.86);
    pointer(&window, Button::Left, at, at);
    pump(&mut state);
    let saved = last_render();
    assert_eq!(saved.annotations.len(), 7);
    assert_eq!(saved.annotations.last().unwrap().text, "测试");

    app.invoke_mark_tool_changed(8);
    pump(&mut state);
    let at = position(0.22, 0.12);
    pointer(&window, Button::Left, at, at);
    pump(&mut state);
    assert_eq!(last_render().annotations.len(), 6);
    app.invoke_undo();
    pump(&mut state);
    assert_eq!(last_render(), saved);
    app.invoke_redo();
    pump(&mut state);
    assert_eq!(last_render().annotations.len(), 6);
    app.invoke_mark_tool_changed(5);
    app.set_mark_shape(1);
    pump(&mut state);
    window.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Shift.into(),
    });
    pointer(
        &window,
        Button::Left,
        position(0.64, 0.12),
        position(0.94, 0.32),
    );
    window.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Shift.into(),
    });
    pump(&mut state);
    let draft = last_render();
    let square = draft.annotations.last().unwrap();
    assert_eq!(square.points.len(), 4);
    let dx = square.points[1].x - square.points[0].x;
    let dy = square.points[2].y - square.points[1].y;
    assert!((dx - dy).abs() < 0.002, "{dx}, {dy}");
    assert_eq!(std::fs::read(&source).unwrap(), original);

    app.invoke_page_requested(1);
    pump(&mut state);
    assert_eq!(last_render(), EditParams::default());
    app.invoke_file_action(image as i32, "edit".into());
    pump(&mut state);
    assert_eq!(last_render(), draft);
    app.invoke_save_edit();
    pump(&mut state);
    assert!(app.get_confirm_open());
    app.invoke_confirm_canceled();
    pump(&mut state);
    assert_eq!(std::fs::read(&source).unwrap(), original);

    app.invoke_save_edit();
    pump(&mut state);
    app.invoke_confirm_accepted();
    pump(&mut state);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.get_edit_saving() && std::time::Instant::now() < deadline {
        state.tick();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!app.get_edit_saving());
    assert!(!app.get_edit_dirty());
    assert_ne!(std::fs::read(source).unwrap(), original);
    assert!(actions::load_edit_params(&core, image).is_identity());
}

#[test]
fn preview_zoom_field_accepts_manual_input_wheel_and_fit_menu() {
    let window = window();
    let app = window.global::<App>();
    app.set_preview_id(42);
    app.set_preview_w(1000);
    app.set_preview_h(800);
    app.set_preview_source_w(1000);
    app.set_preview_source_h(800);
    app.set_page(1);
    let find = |label| {
        ElementHandle::find_by_accessible_label(&window, label)
            .next()
            .expect("preview control is visible")
    };
    let field = find("缩放百分比");
    field.mock_single_click(Button::Left);
    let w = window.window();
    w.dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    w.dispatch_event(WindowEvent::KeyPressed { text: "a".into() });
    w.dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    for text in ["1", "2", "5"] {
        w.dispatch_event(WindowEvent::KeyPressed { text: text.into() });
    }
    w.dispatch_event(WindowEvent::KeyPressed {
        text: Key::Return.into(),
    });
    assert_eq!(field.accessible_value().as_deref(), Some("125"));
    let location = field.absolute_position();
    w.dispatch_event(WindowEvent::PointerScrolled {
        position: LogicalPosition::new(location.x + 3.0, location.y + 10.0),
        delta_x: 0.0,
        delta_y: 30.0,
    });
    assert_eq!(field.accessible_value().as_deref(), Some("130"));
    let zoom = find("缩放选项");
    zoom.mock_single_click(Button::Left);
    find("100% · 实际像素").mock_single_click(Button::Left);
    assert_eq!(field.accessible_value().as_deref(), Some("100"));
    find("适应窗口").mock_single_click(Button::Left);
    find("切换到 100% 实际像素");
    find("EXIF 信息").mock_single_click(Button::Left);
    assert!(app.get_exif_open());
    find("关闭 EXIF 信息").mock_single_click(Button::Left);
    assert!(!app.get_exif_open());

    let weak = window.as_weak();
    window.on_toggle_fullscreen(move || {
        let window = weak.unwrap();
        let app = window.global::<App>();
        app.set_fullscreen(!app.get_fullscreen());
        app.set_preview_chrome(true);
    });
    find("全屏 · F11").mock_single_click(Button::Left);
    assert!(app.get_fullscreen());
    let center = LogicalPosition::new(670.0, 410.0);
    w.dispatch_event(WindowEvent::PointerMoved { position: center });
    for _ in 0..11 {
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(200));
    }
    assert!(!app.get_preview_chrome());
    w.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(680.0, 3.0),
    });
    assert!(app.get_preview_chrome());
    pointer(&window, Button::Left, center, center);
    w.dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    assert!(!app.get_fullscreen());
}
