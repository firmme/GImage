use serde::{Deserialize, Serialize};

use crate::{CropRect, EditParams, Rotation};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MarkPoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotationKind {
    Pen,
    Highlight,
    Line,
    Arrow,
    Polygon,
    Mosaic,
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkTool {
    Draw(AnnotationKind),
    Eraser,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrokeStyle {
    #[default]
    Solid,
    Dashed,
    Dotted,
    Wavy,
}

impl StrokeStyle {
    pub fn is_solid(&self) -> bool {
        *self == Self::Solid
    }

    pub fn line_segments(self, a: (f32, f32), b: (f32, f32), width: f32) -> Vec<Vec<(f32, f32)>> {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let length = dx.hypot(dy);
        if self == Self::Solid || length < 1.0 {
            return vec![vec![a, b]];
        }
        let (ux, uy) = (dx / length, dy / length);
        let at = |t: f32, offset: f32| (a.0 + ux * t - uy * offset, a.1 + uy * t + ux * offset);
        if self == Self::Wavy {
            let period = (width * 5.0).max(8.0);
            let steps = ((length / period * 12.0).ceil() as usize).clamp(2, 4096);
            return vec![
                (0..=steps)
                    .map(|index| {
                        let t = length * index as f32 / steps as f32;
                        let offset = (t / period * std::f32::consts::TAU).sin()
                            * (width * 0.9).max(2.0)
                            * ((length - t) / (period * 0.25)).min(1.0);
                        at(t, offset)
                    })
                    .collect(),
            ];
        }
        let on = if self == Self::Dashed {
            width * 3.0
        } else {
            width * 0.12
        }
        .max(0.25);
        let cycle = (on + width * 2.2).max(3.0).max(length / 4096.0);
        (0..(length / cycle).ceil() as usize)
            .map(|index| {
                let start = index as f32 * cycle;
                vec![at(start, 0.0), at((start + on).min(length), 0.0)]
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub kind: AnnotationKind,
    /// Positions in the EXIF-oriented source, before any crop or rotation.
    pub points: Vec<MarkPoint>,
    pub color: [u8; 3],
    /// Fraction of the rendered image's shorter edge.
    pub width: f32,
    #[serde(default, skip_serializing_if = "StrokeStyle::is_solid")]
    pub style: StrokeStyle,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

impl Annotation {
    pub fn is_valid(&self) -> bool {
        let count = self.points.len();
        let proper_count = match self.kind {
            AnnotationKind::Pen | AnnotationKind::Highlight => (1..=2048).contains(&count),
            AnnotationKind::Line | AnnotationKind::Arrow | AnnotationKind::Mosaic => count == 2,
            AnnotationKind::Polygon => (3..=256).contains(&count),
            AnnotationKind::Text => count == 1 && !self.text.trim().is_empty(),
        };
        proper_count
            && (0.0005..=0.2).contains(&self.width)
            && self.text.len() <= 1024
            && self
                .points
                .iter()
                .all(|p| (0.0..=1.0).contains(&p.x) && (0.0..=1.0).contains(&p.y))
    }
}

fn rotate(p: MarkPoint, rotation: Rotation) -> MarkPoint {
    match rotation {
        Rotation::None => p,
        Rotation::Cw90 => MarkPoint {
            x: 1.0 - p.y,
            y: p.x,
        },
        Rotation::Cw180 => MarkPoint {
            x: 1.0 - p.x,
            y: 1.0 - p.y,
        },
        Rotation::Cw270 => MarkPoint {
            x: p.y,
            y: 1.0 - p.x,
        },
    }
}

fn flip(p: MarkPoint, params: &EditParams) -> MarkPoint {
    MarkPoint {
        x: if params.flip_horizontal {
            1.0 - p.x
        } else {
            p.x
        },
        y: if params.flip_vertical { 1.0 - p.y } else { p.y },
    }
}

impl MarkPoint {
    pub fn to_canvas(self, params: &EditParams) -> Self {
        let crop = params.crop.unwrap_or(CropRect::FULL);
        let p = Self {
            x: (self.x - crop.x) / crop.width,
            y: (self.y - crop.y) / crop.height,
        };
        flip(rotate(p, params.rotation), params)
    }

    pub fn to_source(self, params: &EditParams) -> Self {
        let crop = params.crop.unwrap_or(CropRect::FULL);
        let inverse = match params.rotation {
            Rotation::Cw90 => Rotation::Cw270,
            Rotation::Cw270 => Rotation::Cw90,
            rotation => rotation,
        };
        let p = rotate(flip(self, params), inverse);
        Self {
            x: (crop.x + p.x * crop.width).clamp(0.0, 1.0),
            y: (crop.y + p.y * crop.height).clamp(0.0, 1.0),
        }
    }
}
