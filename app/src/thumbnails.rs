use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, bounded};
use gimage_cache::ThumbKey;
use gimage_core::{EditParams, RgbaImage};
use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::core::{AppCore, RenderRequest, ThumbRequest};
use crate::events::{AppEvent, RenderKind};
use crate::preview_buffer::PREVIEW_EDGE;
use crate::util::{FileVersion, file_version, pixel_buffer};

/// Thumbnail workers are intentionally few: decoding is CPU/memory heavy and
/// the grid only needs visible rows. Requests are deduplicated by the UI.
pub fn spawn_thumbnail_workers(core: Arc<AppCore>, rx: Receiver<ThumbRequest>, count: usize) {
    let (disk_tx, disk_rx) = bounded::<CacheWrite>(12);
    let cache = Arc::clone(&core.cache);
    let _ = std::thread::Builder::new()
        .name("gimage-thumb-cache".into())
        .spawn(move || {
            while let Ok(job) = disk_rx.recv() {
                cache.persist(&job.key, &job.image, job.epoch);
            }
        });
    for index in 0..count.max(1) {
        let core = Arc::clone(&core);
        let rx = rx.clone();
        let disk_tx = disk_tx.clone();
        let _ = std::thread::Builder::new()
            .name(format!("gimage-thumb-{index}"))
            .spawn(move || thumb_worker(core, rx, disk_tx));
    }
}

struct CacheWrite {
    key: ThumbKey,
    image: Arc<RgbaImage>,
    epoch: u64,
}

fn thumb_worker(core: Arc<AppCore>, rx: Receiver<ThumbRequest>, disk: Sender<CacheWrite>) {
    while let Ok(request) = rx.recv() {
        if !core.is_current(request.generation)
            || request.cache_epoch != core.cache.epoch()
            || request.px != core.settings.read().thumbnail_px
        {
            continue;
        }
        let key = ThumbKey::for_path(
            &request.path,
            request.size_bytes,
            request.mtime_ns,
            request.px,
        );
        let (image, generated) = match core.cache.get(&key) {
            Some(image) => (image, false),
            None => match core.engine.thumbnail(&request.path, request.px) {
                Ok(image) => {
                    let image = Arc::new(image);
                    core.cache
                        .put_memory(&key, Arc::clone(&image), request.cache_epoch);
                    (image, true)
                }
                Err(error) => {
                    core.emit(AppEvent::ThumbFailed {
                        generation: request.generation,
                        cache_epoch: request.cache_epoch,
                        id: request.id,
                        px: request.px,
                        error: error.user_message(),
                    });
                    continue;
                }
            },
        };
        core.emit(AppEvent::ThumbReady {
            generation: request.generation,
            cache_epoch: request.cache_epoch,
            id: request.id,
            px: request.px,
            image: Arc::clone(&image),
        });
        if generated {
            let _ = disk.try_send(CacheWrite {
                key,
                image,
                epoch: request.cache_epoch,
            });
        }
    }
}

/// One render thread for preview/editor. Stale requests are dropped by sequence
/// number so dragging a slider never queues a long backlog.
pub fn spawn_render_worker(core: Arc<AppCore>, rx: Receiver<Option<RenderRequest>>) {
    let _ = std::thread::Builder::new()
        .name("gimage-render".into())
        .spawn(move || {
            let mut editor = None;
            while let Ok(request) = rx.recv() {
                let Some(request) = request else {
                    editor = None;
                    continue;
                };
                if request.seq < core.render_seq.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                let result = if request.kind == RenderKind::Editor {
                    render_editor(&request, &mut editor, |adjustments| {
                        core.engine
                            .render(&request.path, adjustments, request.max_edge)
                    })
                    .map(pixel_buffer)
                } else if let Some(options) = request.conversion.as_ref() {
                    core.engine
                        .render(&request.path, &request.params, request.max_edge)
                        .and_then(|image| {
                            gimage_engine::transform_export(image, options, request.max_edge)
                                .map(|image| pixel_buffer(&image))
                                .map_err(|message| {
                                    gimage_core::EngineError::decode(&request.path, message)
                                })
                        })
                } else {
                    render_preview(&core, &request)
                };
                if request.seq != core.render_seq.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                match result {
                    Ok(image) => {
                        core.emit(AppEvent::RenderReady {
                            seq: request.seq,
                            kind: request.kind,
                            id: request.id,
                            image,
                        });
                    }
                    Err(error) => {
                        core.emit(AppEvent::RenderFailed {
                            seq: request.seq,
                            kind: request.kind,
                            id: request.id,
                            error: error.user_message(),
                        });
                    }
                }
            }
        });
}

fn render_preview(
    core: &AppCore,
    request: &RenderRequest,
) -> Result<SharedPixelBuffer<Rgba8Pixel>, gimage_core::EngineError> {
    let version = if request.max_edge == Some(PREVIEW_EDGE) {
        file_version(&request.path).ok()
    } else {
        None
    };
    if let Some(version) = version
        && let Some(image) = core.preview_buffer.get(&request.path, version)
    {
        return Ok(image);
    }
    let image = pixel_buffer(&core.engine.render(
        &request.path,
        &request.params,
        request.max_edge,
    )?);
    if let Some(version) = version
        && file_version(&request.path).ok() == Some(version)
    {
        core.preview_buffer
            .remember(request.path.clone(), version, image.clone());
    }
    Ok(image)
}

struct EditorCache {
    source: std::path::PathBuf,
    version: Option<FileVersion>,
    adjustments: EditParams,
    base: RgbaImage,
    composed: RgbaImage,
    annotations: Vec<Arc<gimage_core::Annotation>>,
}

fn render_editor<'a>(
    request: &RenderRequest,
    cache: &'a mut Option<EditorCache>,
    render_base: impl FnOnce(&EditParams) -> Result<RgbaImage, gimage_core::EngineError>,
) -> Result<&'a RgbaImage, gimage_core::EngineError> {
    let version = file_version(&request.path).ok();
    let params = request.params.normalized();
    let mut adjustments = params.clone();
    adjustments.annotations.clear();
    if !cache.as_ref().is_some_and(|cached| {
        cached.source == request.path
            && cached.version == version
            && cached.adjustments == adjustments
            && version.is_some()
    }) {
        let base = render_base(&adjustments)?;
        *cache = Some(EditorCache {
            source: request.path.clone(),
            version,
            adjustments,
            composed: base.clone(),
            base,
            annotations: Vec::new(),
        });
    }
    let cache = cache.as_mut().unwrap();
    let appended = cache.annotations.len() <= params.annotations.len()
        && cache
            .annotations
            .iter()
            .zip(&params.annotations)
            .all(|(old, current)| Arc::ptr_eq(old, current));
    if !appended {
        cache.composed.pixels.copy_from_slice(&cache.base.pixels);
        cache.annotations.clear();
    }
    gimage_engine::draw_annotations_from(&mut cache.composed, &params, cache.annotations.len());
    cache.annotations = params.annotations;
    Ok(&cache.composed)
}

#[cfg(test)]
mod editor_tests {
    use super::*;
    use gimage_core::{Annotation, AnnotationKind, MarkPoint};
    use std::cell::Cell;

    #[test]
    fn reuses_the_base_for_committed_strokes_and_undo() {
        let source = tempfile::NamedTempFile::new().unwrap();
        let mut request = RenderRequest {
            conversion: None,
            seq: 0,
            kind: RenderKind::Editor,
            id: 0,
            path: source.path().to_owned(),
            params: EditParams::default(),
            max_edge: Some(128),
        };
        let decodes = Cell::new(0);
        let base = |_: &EditParams| {
            decodes.set(decodes.get() + 1);
            Ok(RgbaImage::new(128, 128, vec![255; 128 * 128 * 4]))
        };
        let mut cache = None;
        let clean_pixels = render_editor(&request, &mut cache, base)
            .unwrap()
            .pixels
            .clone();
        request.params.annotations.push(Arc::new(Annotation {
            kind: AnnotationKind::Pen,
            points: vec![MarkPoint { x: 0.5, y: 0.5 }],
            color: [0, 0, 0],
            width: 0.03,
            style: Default::default(),
            text: String::new(),
        }));
        let marked = render_editor(&request, &mut cache, base).unwrap();
        assert_ne!(marked.pixels, clean_pixels);
        request.params.annotations.clear();
        assert_eq!(
            render_editor(&request, &mut cache, base).unwrap().pixels,
            clean_pixels
        );
        assert_eq!(decodes.get(), 1);
        request.params.flip_vertical = true;
        render_editor(&request, &mut cache, base).unwrap();
        assert_eq!(decodes.get(), 2);
    }
}

pub fn spawn_probe_worker(
    core: Arc<AppCore>,
    rx: crossbeam_channel::Receiver<(i64, std::path::PathBuf)>,
) {
    let _ = std::thread::Builder::new()
        .name("gimage-probe".into())
        .spawn(move || {
            while let Ok((id, path)) = rx.recv() {
                let result = core.engine.probe(&path);
                match result {
                    Ok(info) => {
                        {
                            let _ = core.with_image_catalog(id, |catalog| catalog.update_probe(id, &info));
                        }
                        core.emit(AppEvent::ProbeReady { id, info: Ok(info) });
                    }
                    Err(error) => core.emit(AppEvent::ProbeReady {
                        id,
                        info: Err(error.user_message()),
                    }),
                }
            }
        });
}

pub fn render_kind_is(kind: RenderKind, other: RenderKind) -> bool {
    kind == other
}
