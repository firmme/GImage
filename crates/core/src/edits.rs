use serde::{Deserialize, Serialize};
use crate::Annotation;
use std::sync::Arc;

pub const EDIT_PARAMS_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rotation {
    #[default]
    None,
    Cw90,
    Cw180,
    Cw270,
}

impl Rotation {
    pub fn degrees(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Cw90 => 90,
            Self::Cw180 => 180,
            Self::Cw270 => 270,
        }
    }

    pub fn rotate_cw(self) -> Self {
        match self {
            Self::None => Self::Cw90,
            Self::Cw90 => Self::Cw180,
            Self::Cw180 => Self::Cw270,
            Self::Cw270 => Self::None,
        }
    }

    pub fn rotate_ccw(self) -> Self {
        match self {
            Self::None => Self::Cw270,
            Self::Cw90 => Self::None,
            Self::Cw180 => Self::Cw90,
            Self::Cw270 => Self::Cw180,
        }
    }

    pub fn swaps_axes(self) -> bool {
        matches!(self, Self::Cw90 | Self::Cw270)
    }
}

/// Crop rectangle in normalised image coordinates (0..=1), applied *after*
/// rotation and flipping so that the on-screen selection maps directly.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CropRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Default for CropRect {
    fn default() -> Self {
        Self::FULL
    }
}

impl CropRect {
    pub const FULL: Self = Self { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };

    pub fn is_full(&self) -> bool {
        self.x <= 0.0001 && self.y <= 0.0001 && self.width >= 0.9999 && self.height >= 0.9999
    }

    /// Clamp into 0..=1 while keeping a positive, non-degenerate rectangle.
    pub fn clamped(&self) -> Self {
        let x = self.x.clamp(0.0, 1.0);
        let y = self.y.clamp(0.0, 1.0);
        let width = self.width.clamp(0.0, 1.0 - x);
        let height = self.height.clamp(0.0, 1.0 - y);
        Self { x, y, width: width.max(0.0), height: height.max(0.0) }
    }

    pub fn is_usable(&self) -> bool {
        self.clamped().width >= 0.005 && self.clamped().height >= 0.005
    }

    /// Pixel rectangle inside an image of `width` x `height`; at least 1 px.
    pub fn to_pixels(&self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        let c = self.clamped();
        let x = (c.x * width as f32).round() as u32;
        let y = (c.y * height as f32).round() as u32;
        let w = ((c.width * width as f32).round() as u32).max(1);
        let h = ((c.height * height as f32).round() as u32).max(1);
        let w = w.min(width.saturating_sub(x)).max(1);
        let h = h.min(height.saturating_sub(y)).max(1);
        (x, y, w, h)
    }
}

/// Downscale bound. Aspect ratio is always preserved; images that already fit
/// are never upscaled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeSpec {
    pub max_width: u32,
    pub max_height: u32,
}

impl ResizeSpec {
    pub fn new(max_width: u32, max_height: u32) -> Self {
        Self { max_width, max_height }
    }

    /// Target size for `width` x `height`, never larger than the source.
    pub fn fit(&self, width: u32, height: u32) -> (u32, u32) {
        if width == 0 || height == 0 {
            return (width, height);
        }
        let max_w = self.max_width.max(1) as f64;
        let max_h = self.max_height.max(1) as f64;
        let scale = (max_w / width as f64).min(max_h / height as f64).min(1.0);
        if scale >= 1.0 {
            return (width, height);
        }
        let w = ((width as f64 * scale).round() as u32).max(1);
        let h = ((height as f64 * scale).round() as u32).max(1);
        (w, h)
    }

    pub fn is_noop(&self, width: u32, height: u32) -> bool {
        self.fit(width, height) == (width, height)
    }
}

/// Non-destructive edit parameters. Order of application is fixed by the
/// export pipeline: orientation -> crop -> rotate -> flip -> tone -> resize.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditParams {
    /// Free-form for forward compatibility; checked by `from_json`.
    pub version: u32,
    pub rotation: Rotation,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
    pub crop: Option<CropRect>,
    /// Exposure in EV, -3..=3.
    pub exposure: f32,
    /// -1..=1, 0 = unchanged.
    pub contrast: f32,
    /// -1..=1, 0 = unchanged.
    pub saturation: f32,
    /// -1..=1, 0 = unchanged; positive is warmer.
    pub temperature: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<Arc<Annotation>>,
}

impl Default for EditParams {
    fn default() -> Self {
        Self {
            version: EDIT_PARAMS_VERSION,
            rotation: Rotation::None,
            flip_horizontal: false,
            flip_vertical: false,
            crop: None,
            exposure: 0.0,
            contrast: 0.0,
            saturation: 0.0,
            temperature: 0.0,
            annotations: Vec::new(),
        }
    }
}

impl EditParams {
    pub fn is_identity(&self) -> bool {
        self.annotations.is_empty()
            && self.rotation == Rotation::None
            && !self.flip_horizontal
            && !self.flip_vertical
            && self.crop.map(|c| c.is_full()).unwrap_or(true)
            && self.exposure.abs() < f32::EPSILON
            && self.contrast.abs() < f32::EPSILON
            && self.saturation.abs() < f32::EPSILON
            && self.temperature.abs() < f32::EPSILON
    }

    /// Clamp every field into its documented range. Always call before use.
    pub fn normalized(&self) -> Self {
        let mut out = self.clone();
        out.version = EDIT_PARAMS_VERSION;
        out.exposure = out.exposure.clamp(-3.0, 3.0);
        out.contrast = out.contrast.clamp(-1.0, 1.0);
        out.saturation = out.saturation.clamp(-1.0, 1.0);
        out.temperature = out.temperature.clamp(-1.0, 1.0);
        out.annotations.retain(|annotation| annotation.is_valid());
        out.annotations.truncate(256);
        if let Some(crop) = out.crop {
            let crop = crop.clamped();
            out.crop = if crop.is_usable() && !crop.is_full() { Some(crop) } else { None };
        }
        out
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Unknown future fields are ignored; invalid JSON degrades to defaults
    /// instead of failing the whole session load.
    pub fn from_json(json: &str) -> Self {
        serde_json::from_str::<Self>(json).map(|p| p.normalized()).unwrap_or_default()
    }
}

/// Persisted editing state for one image.
#[derive(Clone, Debug, PartialEq)]
pub struct EditSession {
    pub params: EditParams,
    pub revision: u32,
    pub updated_at_ms: i64,
}

impl EditSession {
    pub fn new() -> Self {
        Self { params: EditParams::default(), revision: 0, updated_at_ms: 0 }
    }
}

impl Default for EditSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_cycles() {
        assert_eq!(Rotation::None.rotate_cw().rotate_ccw(), Rotation::None);
        assert_eq!(Rotation::None.rotate_ccw().degrees(), 270);
        assert_eq!(Rotation::Cw270.rotate_cw(), Rotation::None);
    }

    #[test]
    fn crop_pixels_respect_bounds() {
        let crop = CropRect { x: 0.5, y: 0.5, width: 0.9, height: 0.9 }.clamped();
        let (x, y, w, h) = crop.to_pixels(100, 200);
        assert_eq!((x, y), (50, 100));
        assert!(x + w <= 100 && y + h <= 200);
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn resize_never_upscales() {
        let spec = ResizeSpec::new(4000, 4000);
        assert_eq!(spec.fit(800, 600), (800, 600));
        let spec = ResizeSpec::new(400, 400);
        assert_eq!(spec.fit(800, 600), (400, 300));
    }

    #[test]
    fn invalid_json_falls_back_to_default() {
        assert_eq!(EditParams::from_json("not json").is_identity(), true);
    }

    #[test]
    fn normalization_clears_full_crop() {
        let p = EditParams { crop: Some(CropRect::FULL), exposure: 9.0, ..Default::default() }
            .normalized();
        assert!(p.crop.is_none());
        assert_eq!(p.exposure, 3.0);
    }
}
