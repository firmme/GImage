use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use gimage_core::{EngineError, ImageFormat, ImageInfo, RgbaImage};
use resvg::{tiny_skia, usvg};

use crate::RustImageEngine;
use crate::codec::Decoded;
use crate::pipeline::fit_edge;

mod psd;
#[cfg(windows)]
pub(crate) mod windows;

pub fn handles(format: &ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Svg
            | ImageFormat::Psd
            | ImageFormat::Ai
            | ImageFormat::Avif
            | ImageFormat::Heic
            | ImageFormat::Raw
    )
}

pub fn probe(
    engine: &RustImageEngine,
    path: &Path,
    format: ImageFormat,
) -> Result<ImageInfo, EngineError> {
    let (width, height, orientation, has_alpha) = match format {
        ImageFormat::Svg => {
            let size = svg_tree(path)?.size().to_int_size();
            (size.width(), size.height(), 1, true)
        }
        ImageFormat::Psd => psd::probe(path)?,
        #[cfg(windows)]
        ImageFormat::Ai => windows::ai_probe(path)?,
        #[cfg(windows)]
        ImageFormat::Heic | ImageFormat::Avif | ImageFormat::Raw => {
            windows::wic_probe(path, &format)?
        }
        _ => return Err(EngineError::Unsupported(format.label().into())),
    };
    engine.guard_pixels(path, width, height)?;
    Ok(ImageInfo {
        width,
        height,
        orientation,
        has_alpha,
        format,
        animated: false,
        frames: 1,
    })
}

pub fn decode(
    engine: &RustImageEngine,
    path: &Path,
    format: ImageFormat,
    edge: Option<u32>,
    thumbnail: bool,
) -> Result<Decoded, EngineError> {
    let image = match format {
        ImageFormat::Svg => svg_decode(engine, path, edge)?,
        ImageFormat::Psd => psd::decode(engine, path, edge)?,
        #[cfg(windows)]
        ImageFormat::Ai => windows::ai_decode(engine, path, edge)?,
        #[cfg(windows)]
        ImageFormat::Heic | ImageFormat::Avif | ImageFormat::Raw => {
            return windows::wic_decode(engine, path, &format, edge, thumbnail);
        }
        _ => return Err(EngineError::Unsupported(format.label().into())),
    };
    Ok(Decoded {
        image,
        orientation: 1,
    })
}

fn svg_tree(path: &Path) -> Result<usvg::Tree, EngineError> {
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    let mut file = std::io::BufReader::new(File::open(path).map_err(|e| EngineError::io(path, e))?);
    let mut magic = [0; 2];
    file.read_exact(&mut magic)
        .map_err(|e| EngineError::io(path, e))?;
    let input = magic.as_slice().chain(file);
    let mut bytes = Vec::new();
    if magic == [0x1f, 0x8b] {
        flate2::read::GzDecoder::new(input)
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
    } else {
        input.take(MAX_BYTES + 1).read_to_end(&mut bytes)
    }
    .map_err(|e| EngineError::io(path, e))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(EngineError::decode(path, "SVG 文档超过 16 MiB"));
    }

    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    let mut options = usvg::Options::default();
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    if bytes.windows(5).any(|part| part == b"<text") {
        options.fontdb = FONTS
            .get_or_init(|| {
                let mut fonts = usvg::fontdb::Database::new();
                fonts.load_system_fonts();
                Arc::new(fonts)
            })
            .clone();
    }
    usvg::Tree::from_data(&bytes, &options).map_err(|e| EngineError::decode(path, e.to_string()))
}

fn svg_decode(
    engine: &RustImageEngine,
    path: &Path,
    edge: Option<u32>,
) -> Result<RgbaImage, EngineError> {
    let tree = svg_tree(path)?;
    let size = tree.size();
    let source = size.to_int_size();
    engine.guard_pixels(path, source.width(), source.height())?;
    let (width, height) = fit_edge(source.width(), source.height(), edge.unwrap_or(u32::MAX));
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| EngineError::decode(path, "无法分配 SVG 像素缓冲"))?;
    let transform = tiny_skia::Transform::from_scale(
        width as f32 / size.width(),
        height as f32 / size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut pixels = pixmap.take();
    for rgba in pixels.chunks_exact_mut(4) {
        let alpha = u16::from(rgba[3]);
        if alpha > 0 && alpha < 255 {
            for channel in &mut rgba[..3] {
                *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    Ok(RgbaImage::new(width, height, pixels))
}
