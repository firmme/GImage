use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use gimage_core::{ImageFormat, ImageMeta, RgbaImage, SortDirection, SortKey, SortSpec};

/// Convert straight-alpha RGBA into a Slint image.
pub fn to_slint_image(image: &RgbaImage) -> slint::Image {
    if image.is_empty() {
        return slint::Image::default();
    }
    slint::Image::from_rgba8(pixel_buffer(image))
}

pub fn pixel_buffer(image: &RgbaImage) -> slint::SharedPixelBuffer<slint::Rgba8Pixel> {
    slint::SharedPixelBuffer::clone_from_slice(
        &image.pixels,
        image.width,
        image.height,
    )
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Format a UNIX-nanosecond timestamp in local time.
pub fn format_time_ns(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let Ok(dt) = time::OffsetDateTime::from_unix_timestamp(secs) else {
        return "—".to_string();
    };
    let local = time::UtcOffset::current_local_offset()
        .map(|offset| dt.to_offset(offset))
        .unwrap_or(dt);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.year(),
        local.month() as u8,
        local.day(),
        local.hour(),
        local.minute()
    )
}

pub fn format_time_ms(ms: i64) -> String {
    format_time_ns(ms.saturating_mul(1_000_000))
}

pub fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

pub fn dir_display(meta: &ImageMeta) -> String {
    display_path(&meta.dir)
}

pub fn oriented_dimensions(meta: &ImageMeta) -> (u32, u32) {
    let (w, h) = (meta.width.unwrap_or(0), meta.height.unwrap_or(0));
    if (5..=8).contains(&meta.orientation) { (h, w) } else { (w, h) }
}

pub type FileVersion = (u64, std::time::SystemTime);

pub fn file_version(path: &Path) -> std::io::Result<FileVersion> {
    let meta = std::fs::metadata(path)?;
    Ok((meta.len(), meta.modified()?))
}

pub fn exif_text(path: &Path) -> String {
    use exif::{In, Tag};
    let parsed = std::fs::File::open(path).ok().and_then(|file| {
        exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file)).ok()
    });
    let Some(data) = parsed else { return "未找到 EXIF 信息".to_owned(); };
    let fields = [
        ("厂商", Tag::Make),
        ("相机", Tag::Model),
        ("镜头", Tag::LensModel),
        ("拍摄时间", Tag::DateTimeOriginal),
        ("曝光时间", Tag::ExposureTime),
        ("光圈", Tag::FNumber),
        ("ISO", Tag::PhotographicSensitivity),
        ("焦距", Tag::FocalLength),
        ("曝光补偿", Tag::ExposureBiasValue),
        ("方向", Tag::Orientation),
        ("软件", Tag::Software),
    ];
    let lines: Vec<_> = fields.iter().filter_map(|(label, tag)| {
        let value = data.get_field(*tag, In::PRIMARY)?.display_value().with_unit(&data).to_string();
        Some(format!("{label}  {}", value.trim_matches('"')))
    }).collect();
    if lines.is_empty() { "未找到常用 EXIF 字段".to_owned() } else { lines.join("\n") }
}

pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(share) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{share}")
    } else if let Some(drive) = text.strip_prefix(r"\\?\").filter(|path| path.as_bytes().get(1) == Some(&b':')) {
        drive.to_owned()
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_dos_and_unc_paths_without_changing_device_paths() {
        assert_eq!(display_path(Path::new(r"\\?\E:\相册\示例.png")), r"E:\相册\示例.png");
        assert_eq!(display_path(Path::new(r"\\?\UNC\server\share\a.png")), r"\\server\share\a.png");
        assert_eq!(display_path(Path::new(r"\\?\Volume{id}\a.png")), r"\\?\Volume{id}\a.png");
    }
}

pub fn size_text(bytes: u64) -> String {
    gimage_core::paths::format_bytes(bytes)
}

/// Small caption under a thumbnail: "1920×1080 · 2.4 MB".
pub fn meta_line(meta: &ImageMeta) -> String {
    let mut parts = Vec::new();
    match (meta.width, meta.height) {
        (Some(w), Some(h)) => parts.push(format!("{w}×{h}")),
        _ => parts.push(meta.format.label().to_string()),
    }
    parts.push(size_text(meta.size_bytes));
    parts.join(" · ")
}

pub fn sort_key_from_index(index: i32) -> SortKey {
    match index {
        1 => SortKey::Modified,
        2 => SortKey::Size,
        _ => SortKey::Name,
    }
}

pub fn sort_index_from_key(key: SortKey) -> i32 {
    match key {
        SortKey::Modified => 1,
        SortKey::Size => 2,
        _ => 0,
    }
}

pub fn sort_spec(index: i32, descending: bool) -> SortSpec {
    SortSpec {
        key: sort_key_from_index(index),
        direction: if descending {
            SortDirection::Descending
        } else {
            SortDirection::Ascending
        },
    }
}

pub fn format_label(format: &ImageFormat) -> String {
    format.label().to_string()
}

/// Compose the export destination for a source image: same folder, `-export`
/// suffix, target extension. The engine handles collision policies.
pub fn export_destination(source: &Path, options: &gimage_core::ExportOptions) -> std::path::PathBuf {
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_string());
    let extension = options.format.extension();
    let name = if options.prefix.is_empty() {
        format!("{stem}-export.{extension}")
    } else {
        format!("{}{stem}.{extension}", options.prefix)
    };
    match options.directory.as_deref().or_else(|| source.parent()) {
        Some(parent) => parent.join(name),
        None => std::path::PathBuf::from(name),
    }
}

pub fn reveal_in_explorer(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let arg = format!("/select,{}", display_path(path));
        let _ = std::process::Command::new("explorer.exe").arg(arg).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(parent) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

pub fn open_folder_in_shell(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer.exe").arg(display_path(path)).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}
