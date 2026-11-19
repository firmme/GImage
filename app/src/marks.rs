use gimage_core::{Annotation, AnnotationKind, EditParams, MarkPoint, MarkTool, StrokeStyle};
use std::{collections::HashSet, fmt::Write};

#[derive(Default)]
pub struct Draft {
    pub points: Vec<MarkPoint>,
    pub erased: HashSet<usize>,
    pub pointer_down: bool,
}

pub fn tool(index: i32) -> Option<MarkTool> {
    use AnnotationKind::*;
    Some(match index {
        1 => MarkTool::Draw(Pen),
        2 => MarkTool::Draw(Highlight),
        3 => MarkTool::Draw(Line),
        4 => MarkTool::Draw(Arrow),
        5 => MarkTool::Draw(Polygon),
        6 => MarkTool::Draw(Mosaic),
        7 => MarkTool::Draw(Text),
        8 => MarkTool::Eraser,
        _ => return None,
    })
}

pub fn append_point(
    points: &mut Vec<MarkPoint>,
    point: MarkPoint,
    dimensions: (f32, f32),
    limit: usize,
) {
    let distinct = points.last().is_none_or(|last| {
        ((point.x - last.x) * dimensions.0).hypot((point.y - last.y) * dimensions.1) >= 1.5
    });
    if distinct && points.len() < limit {
        points.push(point);
    }
}

pub fn preview(
    kind: AnnotationKind,
    points: &[MarkPoint],
    dimensions: (f32, f32),
    width: f32,
    style: StrokeStyle,
    closed: bool,
) -> String {
    let Some(first) = points.first() else {
        return String::new();
    };
    let (w, h) = dimensions;
    let (x, y) = (first.x * w, first.y * h);
    let mut path = format!("M{x:.2} {y:.2}");
    if kind == AnnotationKind::Mosaic {
        if let Some(last) = points.get(1) {
            let _ = write!(path, " H{:.2} V{:.2} H{x:.2} Z", last.x * w, last.y * h);
        }
        return path;
    }
    if points.len() == 1 {
        let _ = write!(path, " L{:.2} {y:.2}", x + 0.1);
    }
    if matches!(kind, AnnotationKind::Line | AnnotationKind::Arrow) && points.len() == 2 {
        let last = points[1];
        path.clear();
        for segment in style.line_segments((x, y), (last.x * w, last.y * h), width) {
            let _ = write!(path, " M{:.2} {:.2}", segment[0].0, segment[0].1);
            for (x, y) in &segment[1..] {
                let _ = write!(path, " L{x:.2} {y:.2}");
            }
        }
    } else {
        for point in &points[1..] {
            let _ = write!(path, " L{:.2} {:.2}", point.x * w, point.y * h);
        }
    }
    if closed {
        path.push_str(" Z");
    }
    if kind == AnnotationKind::Arrow
        && let Some(last) = points.get(1)
    {
        let (bx, by) = (last.x * w, last.y * h);
        let length = (bx - x).hypot(by - y);
        if length > 0.0 {
            let head = (width * 4.0).max(6.0).min(length * 0.65);
            let (dx, dy) = ((bx - x) / length, (by - y) / length);
            let _ = write!(
                path,
                " M{:.2} {:.2} L{bx:.2} {by:.2} L{:.2} {:.2}",
                bx - dx * head - dy * head * 0.5,
                by - dy * head + dx * head * 0.5,
                bx - dx * head + dy * head * 0.5,
                by - dy * head - dx * head * 0.5
            );
        }
    }
    path
}

pub fn stroke_style(index: i32) -> StrokeStyle {
    match index {
        1 => StrokeStyle::Dashed,
        2 => StrokeStyle::Dotted,
        3 => StrokeStyle::Wavy,
        _ => StrokeStyle::Solid,
    }
}

pub fn shape(
    index: i32,
    start: MarkPoint,
    end: MarkPoint,
    (w, h): (f32, f32),
    regular: bool,
) -> Vec<MarkPoint> {
    let (mut dx, mut dy) = ((end.x - start.x) * w, (end.y - start.y) * h);
    if regular {
        let side = dx.abs().min(dy.abs());
        dx = side.copysign(dx);
        dy = side.copysign(dy);
    }
    if dx.abs().min(dy.abs()) < 2.0 {
        return Vec::new();
    }
    let at = |x, y| MarkPoint {
        x: start.x + dx * x / w,
        y: start.y + dy * y / h,
    };
    if index == 1 {
        return vec![at(0.0, 0.0), at(1.0, 0.0), at(1.0, 1.0), at(0.0, 1.0)];
    }
    let count = match index {
        2 => 72,
        3 => 3,
        4 => 5,
        _ => 6,
    };
    (0..count)
        .map(|vertex| {
            let angle = std::f32::consts::TAU * vertex as f32 / count as f32;
            at(0.5 + angle.sin() * 0.5, 0.5 - angle.cos() * 0.5)
        })
        .collect()
}

pub fn hits(
    mark: &Annotation,
    params: &EditParams,
    point: MarkPoint,
    dimensions: (f32, f32),
    eraser: f32,
) -> bool {
    let (w, h) = dimensions;
    let (x, y) = (point.x * w, point.y * h);
    let width = mark.width * w.min(h);
    let radius = (eraser + width) * 0.5;
    let points: Vec<_> = mark
        .points
        .iter()
        .map(|point| {
            let point = point.to_canvas(params);
            (point.x * w, point.y * h)
        })
        .collect();
    let (ax, ay) = points[0];
    match mark.kind {
        AnnotationKind::Mosaic => {
            let (bx, by) = points[1];
            x >= ax.min(bx) - eraser
                && x <= ax.max(bx) + eraser
                && y >= ay.min(by) - eraser
                && y <= ay.max(by) + eraser
        }
        AnnotationKind::Text => {
            let columns = mark
                .text
                .lines()
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(1);
            let rows = mark.text.lines().count().max(1);
            x >= ax - eraser
                && x <= ax + width * columns as f32 + eraser
                && y >= ay - eraser
                && y <= ay + width * rows as f32 * 1.3 + eraser
        }
        _ => {
            let near = |(ax, ay): (f32, f32), (bx, by): (f32, f32)| {
                let (dx, dy) = (bx - ax, by - ay);
                let length_squared = dx * dx + dy * dy;
                let t = if length_squared > 0.0 {
                    (((x - ax) * dx + (y - ay) * dy) / length_squared).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                (x - ax - t * dx).hypot(y - ay - t * dy) <= radius
            };
            let on_stroke = if matches!(mark.kind, AnnotationKind::Line | AnnotationKind::Arrow)
                && mark.style != StrokeStyle::Solid
            {
                mark.style
                    .line_segments(points[0], points[1], width)
                    .iter()
                    .any(|segment| segment.windows(2).any(|pair| near(pair[0], pair[1])))
            } else {
                points.windows(2).any(|pair| near(pair[0], pair[1]))
            };
            on_stroke
                || points.len() == 1 && near(points[0], points[0])
                || mark.kind == AnnotationKind::Polygon && near(*points.last().unwrap(), points[0])
                || mark.kind == AnnotationKind::Arrow
                    && (x - points[1].0).hypot(y - points[1].1) <= width * 4.0 + eraser
        }
    }
}
