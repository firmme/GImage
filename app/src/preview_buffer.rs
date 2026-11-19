use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, bounded};
use gimage_core::EditParams;
use parking_lot::Mutex;
use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::core::AppCore;
use crate::util::{FileVersion, file_version, pixel_buffer};

pub const PREVIEW_EDGE: u32 = 1600;

struct Frame {
    version: FileVersion,
    image: SharedPixelBuffer<Rgba8Pixel>,
}

#[derive(Default)]
struct State {
    current: Option<PathBuf>,
    neighbors: Vec<PathBuf>,
    frames: HashMap<PathBuf, Frame>,
    plan: u64,
    ready_plan: Option<u64>,
}

pub struct PreviewBuffer {
    state: Mutex<State>,
    wake: Sender<()>,
    work: Receiver<()>,
}

impl PreviewBuffer {
    pub fn new() -> Self {
        let (wake, work) = bounded(1);
        Self {
            state: Mutex::new(State::default()),
            wake,
            work,
        }
    }

    pub fn set_window(&self, current: PathBuf, neighbors: Vec<PathBuf>) {
        let mut state = self.state.lock();
        if state.current.as_ref() == Some(&current) && state.neighbors == neighbors {
            return;
        }
        state.plan += 1;
        state
            .frames
            .retain(|path, _| path == &current || neighbors.contains(path));
        state.current = Some(current);
        state.neighbors = neighbors;
    }

    pub fn clear(&self) {
        let mut state = self.state.lock();
        *state = State {
            plan: state.plan + 1,
            ..State::default()
        };
    }

    pub fn prefetch(&self) {
        let mut state = self.state.lock();
        state.ready_plan = Some(state.plan);
        let _ = self.wake.try_send(());
    }

    pub fn get(&self, path: &Path, version: FileVersion) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        self.state
            .lock()
            .frames
            .get(path)
            .filter(|frame| frame.version == version)
            .map(|frame| frame.image.clone())
    }

    pub fn remember(
        &self,
        path: PathBuf,
        version: FileVersion,
        image: SharedPixelBuffer<Rgba8Pixel>,
    ) {
        let mut state = self.state.lock();
        if state.current.as_ref() == Some(&path) || state.neighbors.contains(&path) {
            state.frames.insert(path, Frame { version, image });
        }
    }

    fn neighborhood(&self) -> Option<(u64, Vec<PathBuf>)> {
        let state = self.state.lock();
        (state.ready_plan == Some(state.plan)).then(|| (state.plan, state.neighbors.clone()))
    }
}

pub fn spawn_prefetch_worker(core: Arc<AppCore>) {
    let _ = std::thread::Builder::new()
        .name("gimage-preview-prefetch".into())
        .spawn(move || {
            let buffer = &core.preview_buffer;
            while buffer.work.recv().is_ok() {
                let Some((plan, paths)) = buffer.neighborhood() else {
                    continue;
                };
                for path in paths {
                    if buffer.state.lock().plan != plan {
                        break;
                    }
                    let Ok(version) = file_version(&path) else {
                        continue;
                    };
                    if buffer.get(&path, version).is_some() {
                        continue;
                    }
                    if let Ok(image) =
                        core.engine
                            .render(&path, &EditParams::default(), Some(PREVIEW_EDGE))
                        && file_version(&path).ok() == Some(version)
                    {
                        buffer.remember(path, version, pixel_buffer(&image));
                    }
                }
            }
        });
}
