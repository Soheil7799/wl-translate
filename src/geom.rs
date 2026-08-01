//! Geometry for the selection overlay, independent of any toolkit.
//!
//! Kept free of widget types on purpose, which is what let all of it survive a
//! change of toolkit unchanged:
//! the rules for what a drag means, where the toolbars can sit and how a
//! selection maps onto image pixels have nothing to do with which library draws
//! them, and they are the parts with tests worth keeping.

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ORIGIN: Point = Point { x: 0.0, y: 0.0 };

    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Size {
    pub fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(origin: Point, size: Size) -> Self {
        Self {
            x: origin.x,
            y: origin.y,
            width: size.width,
            height: size.height,
        }
    }

    pub fn position(self) -> Point {
        Point::new(self.x, self.y)
    }

    pub fn size(self) -> Size {
        Size::new(self.width, self.height)
    }

    pub fn right(self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(self) -> f64 {
        self.y + self.height
    }

    pub fn contains(self, point: Point) -> bool {
        point.x >= self.x && point.x <= self.right() && point.y >= self.y && point.y <= self.bottom()
    }
}

/// How close to a corner counts as grabbing it.
pub const HANDLE_GRAB: f64 = 18.0;
/// Drawn size of a corner handle.
pub const HANDLE_DRAW: f64 = 8.0;
/// Selections smaller than this in either axis are a stray click, not a drag.
pub const MIN_SELECTION: f64 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// The edge under the pointer, if any.
///
/// Corners are checked first by the caller and win: within the grab radius of a
/// corner, two edges are both in range, and resizing one axis when you aimed at
/// a corner feels broken.
pub fn nearest_edge(rect: Rect, position: Point) -> Option<Edge> {
    let within_x = position.x >= rect.x - HANDLE_GRAB && position.x <= rect.right() + HANDLE_GRAB;
    let within_y = position.y >= rect.y - HANDLE_GRAB && position.y <= rect.bottom() + HANDLE_GRAB;

    if within_x && (position.y - rect.y).abs() <= HANDLE_GRAB {
        return Some(Edge::Top);
    }
    if within_x && (position.y - rect.bottom()).abs() <= HANDLE_GRAB {
        return Some(Edge::Bottom);
    }
    if within_y && (position.x - rect.x).abs() <= HANDLE_GRAB {
        return Some(Edge::Left);
    }
    if within_y && (position.x - rect.right()).abs() <= HANDLE_GRAB {
        return Some(Edge::Right);
    }

    None
}

/// The rectangle you get by dragging one edge to `position`.
pub fn resize_edge(rect: Rect, edge: Edge, position: Point) -> Rect {
    let (mut left, mut top, mut right, mut bottom) =
        (rect.x, rect.y, rect.right(), rect.bottom());

    match edge {
        Edge::Top => top = position.y,
        Edge::Bottom => bottom = position.y,
        Edge::Left => left = position.x,
        Edge::Right => right = position.x,
    }

    // Through from_corners so dragging an edge past its opposite flips rather
    // than producing a negative size.
    from_corners(Point::new(left, top), Point::new(right, bottom))
}

/// A rectangle from two opposite corners, in any order.
pub fn from_corners(a: Point, b: Point) -> Rect {
    Rect::new(
        Point::new(a.x.min(b.x), a.y.min(b.y)),
        Size::new((a.x - b.x).abs(), (a.y - b.y).abs()),
    )
}

pub fn corners(rect: Rect) -> [Point; 4] {
    [
        Point::new(rect.x, rect.y),
        Point::new(rect.right(), rect.y),
        Point::new(rect.x, rect.bottom()),
        Point::new(rect.right(), rect.bottom()),
    ]
}

pub fn nearest_corner(rect: Rect, position: Point) -> Option<Corner> {
    [
        (Corner::TopLeft, Point::new(rect.x, rect.y)),
        (Corner::TopRight, Point::new(rect.right(), rect.y)),
        (Corner::BottomLeft, Point::new(rect.x, rect.bottom())),
        (Corner::BottomRight, Point::new(rect.right(), rect.bottom())),
    ]
    .into_iter()
    .find(|(_, corner)| {
        (corner.x - position.x).abs() <= HANDLE_GRAB && (corner.y - position.y).abs() <= HANDLE_GRAB
    })
    .map(|(corner, _)| corner)
}

/// The corner that stays put while `corner` is dragged.
pub fn opposite(rect: Rect, corner: Corner) -> Point {
    match corner {
        Corner::TopLeft => Point::new(rect.right(), rect.bottom()),
        Corner::TopRight => Point::new(rect.x, rect.bottom()),
        Corner::BottomLeft => Point::new(rect.right(), rect.y),
        Corner::BottomRight => Point::new(rect.x, rect.y),
    }
}

/// The four rectangles covering `full` except for `hole`.
pub fn surround(full: Rect, hole: Rect) -> Vec<Rect> {
    [
        Rect::new(full.position(), Size::new(full.width, hole.y.max(0.0))),
        Rect::new(
            Point::new(full.x, hole.bottom()),
            Size::new(full.width, (full.bottom() - hole.bottom()).max(0.0)),
        ),
        Rect::new(
            Point::new(full.x, hole.y),
            Size::new(hole.x.max(0.0), hole.height),
        ),
        Rect::new(
            Point::new(hole.right(), hole.y),
            Size::new((full.right() - hole.right()).max(0.0), hole.height),
        ),
    ]
    .into_iter()
    .filter(|rect| rect.width > 0.0 && rect.height > 0.0)
    .collect()
}

/// Area shared by two rectangles.
pub fn overlap(a: Rect, b: Rect) -> f64 {
    let width = a.right().min(b.right()) - a.x.max(b.x);
    let height = a.bottom().min(b.bottom()) - a.y.max(b.y);

    if width <= 0.0 || height <= 0.0 {
        0.0
    } else {
        width * height
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// The horizontal screen edge the selection covers least.
pub fn toolbar_anchor(selection: Option<Rect>, screen: Size, bar: f64) -> Anchor {
    let Some(rect) = selection else {
        return Anchor::Bottom;
    };

    let top = Rect::new(Point::ORIGIN, Size::new(screen.width, bar));
    let bottom = Rect::new(
        Point::new(0.0, (screen.height - bar).max(0.0)),
        Size::new(screen.width, bar),
    );

    match (overlap(rect, bottom), overlap(rect, top)) {
        (0.0, _) => Anchor::Bottom,
        (_, 0.0) => Anchor::Top,
        (in_bottom, in_top) if in_top < in_bottom => Anchor::Top,
        _ => Anchor::Bottom,
    }
}

/// The vertical screen edge the selection covers least.
pub fn sidebar_anchor(selection: Option<Rect>, screen: Size, strip: f64) -> Side {
    let Some(rect) = selection else {
        return Side::Left;
    };

    let left = Rect::new(Point::ORIGIN, Size::new(strip, screen.height));
    let right = Rect::new(
        Point::new((screen.width - strip).max(0.0), 0.0),
        Size::new(strip, screen.height),
    );

    match (overlap(rect, left), overlap(rect, right)) {
        (0.0, _) => Side::Left,
        (_, 0.0) => Side::Right,
        (in_left, in_right) if in_right < in_left => Side::Right,
        _ => Side::Left,
    }
}

/// Convert a selection in logical points to pixels in the captured image.
///
/// `scale` is image pixels per point - the output's scale factor.
pub fn to_pixels(rect: Rect, scale: f64, image: (u32, u32)) -> Option<(u32, u32, u32, u32)> {
    let x = (rect.x * scale).round().max(0.0) as u32;
    let y = (rect.y * scale).round().max(0.0) as u32;
    let width = (rect.width * scale).round().max(0.0) as u32;
    let height = (rect.height * scale).round().max(0.0) as u32;

    // Clamp into the image rather than trusting the pointer: a drag can end a
    // pixel or two outside the surface.
    let width = width.min(image.0.saturating_sub(x));
    let height = height.min(image.1.saturating_sub(y));

    (width > 0 && height > 0).then_some((x, y, width, height))
}

/// Whether a selection is a deliberate drag rather than a stray click.
pub fn is_usable(selection: Option<Rect>) -> bool {
    selection.is_some_and(|rect| rect.width >= MIN_SELECTION && rect.height >= MIN_SELECTION)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Size = Size {
        width: 1920.0,
        height: 1080.0,
    };
    const BAR: f64 = 72.0;

    fn at(x: f64, y: f64, w: f64, h: f64) -> Option<Rect> {
        Some(Rect::new(Point::new(x, y), Size::new(w, h)))
    }

    #[test]
    fn a_rectangle_comes_out_the_same_whichever_corner_you_drag_from() {
        let downhill = from_corners(Point::new(10.0, 10.0), Point::new(40.0, 50.0));
        let uphill = from_corners(Point::new(40.0, 50.0), Point::new(10.0, 10.0));

        assert_eq!(downhill, uphill);
        assert_eq!(downhill.width, 30.0);
        assert_eq!(downhill.height, 40.0);
    }

    #[test]
    fn dimming_covers_everything_except_the_selection() {
        let full = Rect::new(Point::ORIGIN, Size::new(100.0, 100.0));
        let hole = Rect::new(Point::new(20.0, 30.0), Size::new(40.0, 20.0));

        let covered: f64 = surround(full, hole).iter().map(|r| r.width * r.height).sum();

        assert_eq!(covered, 100.0 * 100.0 - 40.0 * 20.0);
    }

    #[test]
    fn a_selection_flush_with_an_edge_produces_no_zero_sized_dimming() {
        let full = Rect::new(Point::ORIGIN, Size::new(100.0, 100.0));
        let hole = Rect::new(Point::ORIGIN, Size::new(100.0, 40.0));

        assert!(surround(full, hole)
            .iter()
            .all(|r| r.width > 0.0 && r.height > 0.0));
    }

    #[test]
    fn scaling_to_pixels_respects_the_output_scale() {
        let rect = Rect::new(Point::new(10.0, 20.0), Size::new(30.0, 40.0));

        assert_eq!(
            to_pixels(rect, 2.0, (1000, 1000)),
            Some((20, 40, 60, 80))
        );
    }

    #[test]
    fn a_selection_running_past_the_edge_is_clamped_into_the_image() {
        let rect = Rect::new(Point::new(90.0, 0.0), Size::new(40.0, 10.0));
        let (x, _, w, _) = to_pixels(rect, 1.0, (100, 100)).expect("non-empty");

        assert_eq!((x, w), (90, 10));
    }

    #[test]
    fn corners_are_found_within_the_grab_radius_and_not_beyond_it() {
        let rect = Rect::new(Point::new(100.0, 100.0), Size::new(200.0, 200.0));

        assert!(nearest_corner(rect, Point::new(104.0, 104.0)).is_some());
        assert!(nearest_corner(rect, Point::new(200.0, 200.0)).is_none());
    }

    #[test]
    fn the_toolbar_sits_at_the_bottom_when_nothing_is_in_the_way() {
        assert_eq!(toolbar_anchor(None, SCREEN, BAR), Anchor::Bottom);
        assert_eq!(
            toolbar_anchor(at(100.0, 100.0, 400.0, 300.0), SCREEN, BAR),
            Anchor::Bottom
        );
    }

    #[test]
    fn it_moves_to_the_top_when_the_selection_reaches_the_bottom() {
        assert_eq!(
            toolbar_anchor(at(100.0, 900.0, 400.0, 180.0), SCREEN, BAR),
            Anchor::Top
        );
    }

    #[test]
    fn the_tool_strip_moves_off_whichever_side_is_covered() {
        assert_eq!(sidebar_anchor(None, SCREEN, 190.0), Side::Left);
        assert_eq!(
            sidebar_anchor(at(0.0, 0.0, 300.0, 1080.0), SCREEN, 190.0),
            Side::Right
        );
        assert_eq!(
            sidebar_anchor(at(1600.0, 0.0, 320.0, 1080.0), SCREEN, 190.0),
            Side::Left
        );
    }

    #[test]
    fn each_edge_is_grabbable_and_the_middle_is_not() {
        let rect = Rect::new(Point::new(100.0, 100.0), Size::new(200.0, 200.0));

        assert_eq!(nearest_edge(rect, Point::new(200.0, 102.0)), Some(Edge::Top));
        assert_eq!(
            nearest_edge(rect, Point::new(200.0, 298.0)),
            Some(Edge::Bottom)
        );
        assert_eq!(nearest_edge(rect, Point::new(102.0, 200.0)), Some(Edge::Left));
        assert_eq!(
            nearest_edge(rect, Point::new(298.0, 200.0)),
            Some(Edge::Right)
        );
        assert_eq!(nearest_edge(rect, Point::new(200.0, 200.0)), None);
    }

    #[test]
    fn dragging_an_edge_moves_only_that_side() {
        let rect = Rect::new(Point::new(100.0, 100.0), Size::new(200.0, 200.0));
        let widened = resize_edge(rect, Edge::Right, Point::new(400.0, 999.0));

        assert_eq!(widened.x, 100.0);
        assert_eq!(widened.y, 100.0);
        assert_eq!(widened.height, 200.0, "the other axis must not move");
        assert_eq!(widened.width, 300.0);
    }

    #[test]
    fn dragging_an_edge_past_its_opposite_flips_instead_of_going_negative() {
        let rect = Rect::new(Point::new(100.0, 100.0), Size::new(200.0, 200.0));
        let flipped = resize_edge(rect, Edge::Left, Point::new(400.0, 0.0));

        assert_eq!(flipped.x, 300.0);
        assert_eq!(flipped.width, 100.0);
    }

    #[test]
    fn a_stray_click_is_not_a_usable_selection() {
        assert!(!is_usable(at(0.0, 0.0, 1.0, 1.0)));
        assert!(!is_usable(None));
        assert!(is_usable(at(0.0, 0.0, 40.0, 30.0)));
    }
}
