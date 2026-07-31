//! Annotations drawn on top of a capture.
//!
//! Two things render these: the live preview on the overlay canvas, and the
//! rasteriser that bakes them into the saved PNG. Rather than writing the
//! shapes twice and watching the two drift apart, both draw from [`outline`],
//! which reduces every tool to a list of polylines. The preview strokes them
//! with the toolkit of the day; the output strokes them with tiny-skia.

use crate::geom::{Point, Rect, Size};

/// Segments used to approximate an ellipse. Enough that the curve reads as
/// smooth at any size a screenshot annotation is likely to be.
const ELLIPSE_STEPS: usize = 64;
/// Length of an arrow head, as a fraction of the shaft.
const HEAD_FRACTION: f64 = 0.22;
/// Longest an arrow head may get, in points, so a long arrow does not grow a
/// comically large head.
const HEAD_MAX: f64 = 34.0;
/// Half-angle of the arrow head, in radians (~28°).
const HEAD_SPREAD: f64 = 0.48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Arrow,
    Rectangle,
    Ellipse,
    /// Freehand, but thick and translucent so the text underneath stays legible.
    Highlight,
    /// Pixelates its rectangle. The one tool that edits pixels rather than
    /// drawing over them, because a drawn-on black box can be undone by anyone
    /// with the file, and redaction that can be undone is not redaction.
    Blur,
}

impl Tool {
    pub fn label(self) -> &'static str {
        match self {
            Tool::Pen => "Pen  (p)",
            Tool::Arrow => "Arrow  (a)",
            Tool::Rectangle => "Box  (r)",
            Tool::Ellipse => "Ellipse  (o)",
            Tool::Highlight => "Highlight  (h)",
            Tool::Blur => "Blur  (b)",
        }
    }

    /// Highlighter ink is translucent, and thick enough to cover a line of text.
    pub fn ink(self, color: [f64; 4], width: f64) -> ([f64; 4], f64) {
        match self {
            Tool::Highlight => ([color[0], color[1], color[2], 0.35], width * 5.0),
            _ => (color, width),
        }
    }

    /// Whether a stray click should be discarded rather than kept.
    fn needs_a_drag(self) -> bool {
        !matches!(self, Tool::Pen)
    }
}

#[derive(Debug, Clone)]
pub struct Annotation {
    pub tool: Tool,
    /// Pen keeps every sample; the other tools keep only start and end.
    pub points: Vec<Point>,
    /// Straight rgba, 0..1, so both renderers can take it unchanged.
    pub color: [f64; 4],
    pub width: f64,
}

impl Annotation {
    pub fn new(tool: Tool, from: Point, color: [f64; 4], width: f64) -> Self {
        Self {
            tool,
            points: vec![from],
            color,
            width,
        }
    }

    /// Extend with the pointer's latest position.
    ///
    /// Freehand accumulates; every other tool is defined by two corners, so it
    /// keeps replacing the second one as you drag.
    pub fn extend(&mut self, to: Point) {
        match self.tool {
            Tool::Pen | Tool::Highlight => self.points.push(to),
            _ => {
                self.points.truncate(1);
                self.points.push(to);
            }
        }
    }

    /// Whether this is worth keeping, or just a click that drew nothing.
    pub fn is_usable(&self) -> bool {
        if self.tool.needs_a_drag() {
            self.points.len() == 2 && distance(self.points[0], self.points[1]) > 3.0
        } else {
            self.points.len() > 1
        }
    }

    /// The rectangle a two-corner tool covers, in whatever space its points are.
    pub fn bounds(&self) -> Option<Rect> {
        let [a, b] = self.points.as_slice() else {
            return None;
        };

        Some(Rect::new(
            Point::new(a.x.min(b.x), a.y.min(b.y)),
            Size::new((a.x - b.x).abs(), (a.y - b.y).abs()),
        ))
    }
}

/// Every polyline making up an annotation, in the same space its points are in.
pub fn outline(annotation: &Annotation) -> Vec<Vec<Point>> {
    match annotation.tool {
        // Blur has no outline: it replaces pixels rather than drawing on them.
        Tool::Blur => Vec::new(),

        Tool::Pen | Tool::Highlight => vec![annotation.points.clone()],

        Tool::Arrow => match annotation.points.as_slice() {
            [from, to] => vec![vec![*from, *to], arrow_head(*from, *to)],
            _ => Vec::new(),
        },

        Tool::Rectangle => match annotation.points.as_slice() {
            [a, b] => {
                let (left, right) = (a.x.min(b.x), a.x.max(b.x));
                let (top, bottom) = (a.y.min(b.y), a.y.max(b.y));

                vec![vec![
                    Point::new(left, top),
                    Point::new(right, top),
                    Point::new(right, bottom),
                    Point::new(left, bottom),
                    Point::new(left, top),
                ]]
            }
            _ => Vec::new(),
        },

        Tool::Ellipse => match annotation.points.as_slice() {
            [a, b] => vec![ellipse(*a, *b)],
            _ => Vec::new(),
        },
    }
}

/// Colours offered for annotations, in the order they appear and are numbered.
pub const PALETTE: [(&str, [f64; 4]); 6] = [
    ("red", [0.92, 0.19, 0.21, 1.0]),
    ("orange", [0.96, 0.55, 0.13, 1.0]),
    ("yellow", [0.98, 0.83, 0.20, 1.0]),
    ("green", [0.30, 0.76, 0.38, 1.0]),
    ("blue", [0.26, 0.52, 0.96, 1.0]),
    ("white", [1.0, 1.0, 1.0, 1.0]),
];

/// Stroke widths, cycled with the thickness control.
pub const WIDTHS: [f64; 4] = [2.0, 4.0, 7.0, 12.0];

/// The two barbs, as one polyline that runs through the tip.
fn arrow_head(from: Point, to: Point) -> Vec<Point> {
    let shaft = distance(from, to);

    if shaft <= f64::EPSILON {
        return Vec::new();
    }

    let length = (shaft * HEAD_FRACTION).min(HEAD_MAX);
    let angle = (to.y - from.y).atan2(to.x - from.x);

    let barb = |offset: f64| {
        let direction = angle + offset;
        Point::new(
            to.x - length * direction.cos(),
            to.y - length * direction.sin(),
        )
    };

    vec![barb(-HEAD_SPREAD), to, barb(HEAD_SPREAD)]
}

/// An ellipse inscribed in the box defined by two opposite corners.
fn ellipse(a: Point, b: Point) -> Vec<Point> {
    let centre = Point::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
    let radius = Size::new((a.x - b.x).abs() / 2.0, (a.y - b.y).abs() / 2.0);

    (0..=ELLIPSE_STEPS)
        .map(|step| {
            let angle = step as f64 / ELLIPSE_STEPS as f64 * std::f64::consts::TAU;
            Point::new(
                centre.x + radius.width * angle.cos(),
                centre.y + radius.height * angle.sin(),
            )
        })
        .collect()
}

fn distance(a: Point, b: Point) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// Bake annotations into a captured region.
///
/// `origin` is the selection's top-left in the same space the annotations were
/// recorded in, and `scale` converts that space into image pixels - so the
/// drawing lands exactly where it appeared on screen.
pub fn rasterize(
    annotations: &[Annotation],
    png: &[u8],
    origin: Point,
    scale: f64,
) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;
    use tiny_skia::{LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

    if annotations.is_empty() {
        return Ok(png.to_vec());
    }

    let mut pixmap = Pixmap::decode_png(png).context("could not decode the capture")?;

    // Redaction first, so a box drawn to point at something is not itself
    // pixelated by a later blur.
    for annotation in annotations.iter().filter(|a| a.tool == Tool::Blur) {
        if let Some(area) = annotation.bounds() {
            pixelate(
                &mut pixmap,
                (area.x - origin.x) * scale,
                (area.y - origin.y) * scale,
                area.width * scale,
                area.height * scale,
                (annotation.width * scale * 2.5).max(6.0),
            );
        }
    }

    for annotation in annotations.iter().filter(|a| a.tool != Tool::Blur) {
        let mut paint = Paint::default();
        let [red, green, blue, alpha] = annotation.color;
        // tiny-skia works in f32; our geometry is f64, so cast at this boundary.
        paint.set_color(tiny_skia::Color::from_rgba(red as f32, green as f32, blue as f32, alpha as f32).unwrap_or(
            tiny_skia::Color::from_rgba8(255, 0, 0, 255),
        ));
        paint.anti_alias = true;

        let stroke = Stroke {
            width: (annotation.width * scale).max(1.0) as f32,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };

        for line in outline(annotation) {
            let mut builder = PathBuilder::new();

            for (index, point) in line.iter().enumerate() {
                let x = (point.x - origin.x) * scale;
                let y = (point.y - origin.y) * scale;

                if index == 0 {
                    builder.move_to(x as f32, y as f32);
                } else {
                    builder.line_to(x as f32, y as f32);
                }
            }

            if let Some(path) = builder.finish() {
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
    }

    pixmap.encode_png().context("could not encode the annotated capture")
}

/// Average square blocks of pixels in place.
///
/// Deliberately destructive: the original pixels are gone from the output, so
/// unlike a drawn-on black box there is nothing underneath to recover.
fn pixelate(
    pixmap: &mut tiny_skia::Pixmap,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    block: f64,
) {
    let (image_width, image_height) = (pixmap.width() as i64, pixmap.height() as i64);

    let left = (x.round() as i64).clamp(0, image_width);
    let top = (y.round() as i64).clamp(0, image_height);
    let right = ((x + width).round() as i64).clamp(0, image_width);
    let bottom = ((y + height).round() as i64).clamp(0, image_height);

    let block = (block.round() as i64).max(2);
    let data = pixmap.data_mut();

    let mut block_top = top;
    while block_top < bottom {
        let block_bottom = (block_top + block).min(bottom);

        let mut block_left = left;
        while block_left < right {
            let block_right = (block_left + block).min(right);

            let (mut red, mut green, mut blue, mut alpha, mut count) = (0u32, 0u32, 0u32, 0u32, 0u32);

            for row in block_top..block_bottom {
                for column in block_left..block_right {
                    let index = ((row * image_width + column) * 4) as usize;
                    red += data[index] as u32;
                    green += data[index + 1] as u32;
                    blue += data[index + 2] as u32;
                    alpha += data[index + 3] as u32;
                    count += 1;
                }
            }

            if count > 0 {
                let average = [
                    (red / count) as u8,
                    (green / count) as u8,
                    (blue / count) as u8,
                    (alpha / count) as u8,
                ];

                for row in block_top..block_bottom {
                    for column in block_left..block_right {
                        let index = ((row * image_width + column) * 4) as usize;
                        data[index..index + 4].copy_from_slice(&average);
                    }
                }
            }

            block_left = block_right;
        }

        block_top = block_bottom;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drag(tool: Tool, from: Point, to: Point) -> Annotation {
        let mut annotation = Annotation::new(tool, from, [1.0, 0.0, 0.0, 1.0], 3.0);
        annotation.extend(to);
        annotation
    }

    #[test]
    fn pixelating_flattens_a_region_and_leaves_the_rest_alone() {
        use tiny_skia::{Pixmap, PremultipliedColorU8};

        let mut pixmap = Pixmap::new(8, 8).unwrap();

        // A gradient, so flattening is visible as neighbouring pixels agreeing.
        for (index, pixel) in pixmap.pixels_mut().iter_mut().enumerate() {
            let value = (index * 3) as u8;
            *pixel = PremultipliedColorU8::from_rgba(value, value, value, 255).unwrap();
        }

        let before_outside = pixmap.pixels()[63];
        pixelate(&mut pixmap, 0.0, 0.0, 4.0, 4.0, 4.0);

        let flattened = &pixmap.pixels()[..4];
        assert!(flattened.iter().all(|p| p.red() == flattened[0].red()));

        // The far corner was outside the region and must be untouched.
        assert_eq!(pixmap.pixels()[63].red(), before_outside.red());
    }

    #[test]
    fn the_highlighter_is_translucent_and_fat() {
        let (ink, width) = Tool::Highlight.ink([1.0, 0.0, 0.0, 1.0], 3.0);

        assert!(ink[3] < 0.5, "highlighter must not hide what it marks");
        assert!(width > 3.0);

        // Every other tool leaves ink alone.
        assert_eq!(Tool::Pen.ink([1.0, 0.0, 0.0, 1.0], 3.0), ([1.0, 0.0, 0.0, 1.0], 3.0));
    }

    #[test]
    fn freehand_accumulates_but_the_others_replace_their_end() {
        let mut pen = Annotation::new(Tool::Pen, Point::ORIGIN, [1.0; 4], 3.0);
        pen.extend(Point::new(1.0, 1.0));
        pen.extend(Point::new(2.0, 2.0));
        assert_eq!(pen.points.len(), 3);

        let mut arrow = Annotation::new(Tool::Arrow, Point::ORIGIN, [1.0; 4], 3.0);
        arrow.extend(Point::new(1.0, 1.0));
        arrow.extend(Point::new(2.0, 2.0));
        assert_eq!(arrow.points.len(), 2);
        assert_eq!(arrow.points[1], Point::new(2.0, 2.0));
    }

    #[test]
    fn a_click_that_drew_nothing_is_discarded() {
        let click = drag(Tool::Arrow, Point::ORIGIN, Point::new(1.0, 1.0));
        assert!(!click.is_usable());

        let real = drag(Tool::Arrow, Point::ORIGIN, Point::new(90.0, 40.0));
        assert!(real.is_usable());
    }

    #[test]
    fn a_box_closes_back_on_itself() {
        let outline = outline(&drag(
            Tool::Rectangle,
            Point::new(10.0, 10.0),
            Point::new(50.0, 30.0),
        ));

        let line = &outline[0];
        assert_eq!(line.len(), 5);
        assert_eq!(line.first(), line.last());
    }

    #[test]
    fn a_box_is_the_same_whichever_corner_it_is_dragged_from() {
        let downhill = outline(&drag(Tool::Rectangle, Point::new(10.0, 10.0), Point::new(50.0, 30.0)));
        let uphill = outline(&drag(Tool::Rectangle, Point::new(50.0, 30.0), Point::new(10.0, 10.0)));

        assert_eq!(downhill, uphill);
    }

    #[test]
    fn the_arrow_head_sits_at_the_pointy_end() {
        let arrow = drag(Tool::Arrow, Point::new(0.0, 0.0), Point::new(100.0, 0.0));
        let head = &outline(&arrow)[1];

        // Middle of the barb polyline is the tip itself.
        assert_eq!(head[1], Point::new(100.0, 0.0));
        // Both barbs trail behind it.
        assert!(head[0].x < 100.0 && head[2].x < 100.0);
    }

    #[test]
    fn a_long_arrow_does_not_grow_an_absurd_head() {
        let long = drag(Tool::Arrow, Point::ORIGIN, Point::new(4000.0, 0.0));
        let head = &outline(&long)[1];

        assert!(4000.0 - head[0].x <= HEAD_MAX + 1.0);
    }

    #[test]
    fn an_ellipse_closes_and_stays_inside_its_box() {
        let line = &outline(&drag(
            Tool::Ellipse,
            Point::new(0.0, 0.0),
            Point::new(100.0, 50.0),
        ))[0];

        // Closes on itself, within floating point: the last point comes from
        // cos(TAU), which is not exactly 1.0.
        let (first, last) = (line.first().unwrap(), line.last().unwrap());
        assert!((first.x - last.x).abs() < 0.001 && (first.y - last.y).abs() < 0.001);

        assert!(line
            .iter()
            .all(|p| p.x >= -0.01 && p.x <= 100.01 && p.y >= -0.01 && p.y <= 50.01));
    }
}
