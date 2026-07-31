//! The selection overlay.
//!
//! Flameshot's model: the screen is captured up front, that still image is
//! shown fullscreen, and you pick your region on the frozen copy. Nothing can
//! scroll or animate out from under the selection because nothing on screen is
//! live any more - and the selection stays adjustable, because committing is a
//! separate keypress rather than the mouse button coming up.
//!
//! Coordinates are the fiddly part. The capture is in physical pixels; the
//! canvas lays out in logical points. Everything here works in canvas points
//! and converts once, at the edge, in [`Selection::to_pixels`].

use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme};

/// How close to a corner counts as grabbing it, in points.
const HANDLE_GRAB: f32 = 18.0;
/// Drawn size of a corner handle.
const HANDLE_DRAW: f32 = 8.0;
/// Selections smaller than this in either axis are treated as a stray click.
const MIN_SELECTION: f32 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Selection(pub Rectangle);

impl Selection {
    /// Convert a selection in canvas points to pixels in the captured image.
    ///
    /// `scale` is image pixels per canvas point, which is the output's scale
    /// factor - 1.0 on an unscaled display, 2.0 on a HiDPI one.
    pub fn to_pixels(self, scale: f32, image: Size<u32>) -> Option<(u32, u32, u32, u32)> {
        let rect = self.0;

        let x = (rect.x * scale).round().max(0.0) as u32;
        let y = (rect.y * scale).round().max(0.0) as u32;
        let width = (rect.width * scale).round() as u32;
        let height = (rect.height * scale).round() as u32;

        // Clamp into the image rather than trusting the pointer: a drag can end
        // a pixel or two outside the surface.
        let width = width.min(image.width.saturating_sub(x));
        let height = height.min(image.height.saturating_sub(y));

        (width > 0 && height > 0).then_some((x, y, width, height))
    }
}

#[derive(Debug, Clone, Copy)]
enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, Copy)]
enum Drag {
    /// Dragging out a brand new selection from a fixed origin.
    New { origin: Point },
    /// Moving an existing selection; holds the grab offset inside it.
    Move { offset: iced::Vector },
    /// Resizing from one corner; the opposite corner stays put.
    Resize { anchor: Point },
}

#[derive(Default)]
pub struct State {
    drag: Option<Drag>,
}

/// The canvas program. Rebuilt each frame from the app state, so the current
/// selection is passed in rather than owned here.
///
/// It draws only the dimming and the selection. The frozen capture is an
/// `image` widget stacked underneath, because geometry and images render in
/// separate passes: an image drawn inside the canvas ends up on top of the
/// dimming regardless of the order the calls were made in, which is exactly
/// the bug that made the first overlay look undimmed.
pub struct Selector {
    pub selection: Option<Selection>,
}

impl<Message> canvas::Program<Message> for Selector
where
    Message: From<Selection> + Clone,
{
    type State = State;

    fn update(
        &self,
        state: &mut Self::State,
        event: &iced::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        let position = cursor.position_in(bounds)?;

        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                state.drag = Some(self.grab(position));
                Some(canvas::Action::request_redraw().and_capture())
            }

            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let drag = state.drag?;
                let selection = self.apply(drag, position, bounds);

                Some(canvas::Action::publish(Message::from(selection)).and_capture())
            }

            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.drag = None;
                Some(canvas::Action::request_redraw().and_capture())
            }

            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        let full = Rectangle::new(Point::ORIGIN, bounds.size());

        let dim = Color::from_rgba(0.0, 0.0, 0.0, 0.55);

        match self.selection.map(|selection| selection.0) {
            // Dim around the selection rather than over it: a frame has no way
            // to punch a hole, and four rectangles is cheaper than compositing.
            Some(rect) => {
                for around in surround(full, rect) {
                    frame.fill_rectangle(around.position(), around.size(), dim);
                }

                frame.stroke(
                    &Path::rectangle(rect.position(), rect.size()),
                    Stroke::default()
                        .with_color(Color::from_rgb(0.45, 0.55, 1.0))
                        .with_width(2.0),
                );

                for corner in corners(rect) {
                    frame.fill_rectangle(
                        Point::new(corner.x - HANDLE_DRAW / 2.0, corner.y - HANDLE_DRAW / 2.0),
                        Size::new(HANDLE_DRAW, HANDLE_DRAW),
                        Color::from_rgb(0.45, 0.55, 1.0),
                    );
                }
            }
            None => frame.fill_rectangle(full.position(), full.size(), dim),
        }

        // No hint line here any more: the toolbar spells out every action and
        // its key, and the two disagreed with each other as soon as the buttons
        // gained their own shortcuts.

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        let Some(position) = cursor.position_in(bounds) else {
            return mouse::Interaction::default();
        };

        let Some(rect) = self.selection.map(|selection| selection.0) else {
            return mouse::Interaction::Crosshair;
        };

        if let Some(corner) = nearest_corner(rect, position) {
            return resize_cursor(corner);
        }

        if rect.contains(position) {
            return mouse::Interaction::Move;
        }

        mouse::Interaction::Crosshair
    }
}

impl Selector {
    /// Decide what a press at `position` starts: resizing from a corner,
    /// moving the whole selection, or drawing a new one.
    fn grab(&self, position: Point) -> Drag {
        let Some(rect) = self.selection.map(|selection| selection.0) else {
            return Drag::New { origin: position };
        };

        if let Some(corner) = nearest_corner(rect, position) {
            return Drag::Resize {
                anchor: opposite(rect, corner),
            };
        }

        if rect.contains(position) {
            return Drag::Move {
                offset: position - rect.position(),
            };
        }

        Drag::New { origin: position }
    }

    fn apply(&self, drag: Drag, position: Point, bounds: Rectangle) -> Selection {
        let limit = Size::new(bounds.width, bounds.height);

        match drag {
            Drag::New { origin } => Selection(from_corners(origin, position)),
            Drag::Resize { anchor } => Selection(from_corners(anchor, position)),
            Drag::Move { offset } => {
                let rect = self.selection.map(|selection| selection.0).unwrap_or_default();

                // Keep a moved selection on screen; a dragged-off selection
                // would crop to nothing.
                let x = (position.x - offset.x).clamp(0.0, (limit.width - rect.width).max(0.0));
                let y = (position.y - offset.y).clamp(0.0, (limit.height - rect.height).max(0.0));

                Selection(Rectangle::new(Point::new(x, y), rect.size()))
            }
        }
    }
}

/// A rectangle from two opposite corners, in any order.
fn from_corners(a: Point, b: Point) -> Rectangle {
    Rectangle::new(
        Point::new(a.x.min(b.x), a.y.min(b.y)),
        Size::new((a.x - b.x).abs(), (a.y - b.y).abs()),
    )
}

fn corners(rect: Rectangle) -> [Point; 4] {
    [
        Point::new(rect.x, rect.y),
        Point::new(rect.x + rect.width, rect.y),
        Point::new(rect.x, rect.y + rect.height),
        Point::new(rect.x + rect.width, rect.y + rect.height),
    ]
}

fn nearest_corner(rect: Rectangle, position: Point) -> Option<Corner> {
    let all = [
        (Corner::TopLeft, Point::new(rect.x, rect.y)),
        (Corner::TopRight, Point::new(rect.x + rect.width, rect.y)),
        (Corner::BottomLeft, Point::new(rect.x, rect.y + rect.height)),
        (
            Corner::BottomRight,
            Point::new(rect.x + rect.width, rect.y + rect.height),
        ),
    ];

    all.into_iter()
        .find(|(_, corner)| {
            (corner.x - position.x).abs() <= HANDLE_GRAB
                && (corner.y - position.y).abs() <= HANDLE_GRAB
        })
        .map(|(corner, _)| corner)
}

/// The cursor for dragging a given corner.
///
/// A corner resizes along the diagonal it sits on, so the arrow has to point
/// that way. Top-left and bottom-right share the ↖↘ axis; top-right and
/// bottom-left share ↗↙. A grab hand says "you can pick this up and move it",
/// which is what dragging the middle does, not what dragging a corner does.
fn resize_cursor(corner: Corner) -> mouse::Interaction {
    match corner {
        Corner::TopLeft | Corner::BottomRight => mouse::Interaction::ResizingDiagonallyDown,
        Corner::TopRight | Corner::BottomLeft => mouse::Interaction::ResizingDiagonallyUp,
    }
}

fn opposite(rect: Rectangle, corner: Corner) -> Point {
    match corner {
        Corner::TopLeft => Point::new(rect.x + rect.width, rect.y + rect.height),
        Corner::TopRight => Point::new(rect.x, rect.y + rect.height),
        Corner::BottomLeft => Point::new(rect.x + rect.width, rect.y),
        Corner::BottomRight => Point::new(rect.x, rect.y),
    }
}

/// The four rectangles covering `full` except for `hole`.
fn surround(full: Rectangle, hole: Rectangle) -> Vec<Rectangle> {
    let bottom = hole.y + hole.height;
    let right = hole.x + hole.width;

    [
        Rectangle::new(full.position(), Size::new(full.width, hole.y.max(0.0))),
        Rectangle::new(
            Point::new(full.x, bottom),
            Size::new(full.width, (full.height - bottom).max(0.0)),
        ),
        Rectangle::new(
            Point::new(full.x, hole.y),
            Size::new(hole.x.max(0.0), hole.height),
        ),
        Rectangle::new(
            Point::new(right, hole.y),
            Size::new((full.width - right).max(0.0), hole.height),
        ),
    ]
    .into_iter()
    .filter(|rect| rect.width > 0.0 && rect.height > 0.0)
    .collect()
}

/// Which screen edge the toolbar sits against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Top,
    Bottom,
}

/// Pick the screen edge that the selection covers least.
///
/// The toolbar sticks to the *screen*, not to the selection - it does not
/// follow the box around. All it does is move out of the way, so it never sits
/// on top of what you are trying to look at.
///
/// Bottom is preferred because that is where the hint line already is; it flips
/// to the top only when the selection reaches into the bottom strip. A
/// selection tall enough to cover both gets whichever it overlaps less.
pub fn toolbar_anchor(selection: Option<Selection>, screen: Size, bar: f32) -> Anchor {
    let Some(rect) = selection.map(|selection| selection.0) else {
        return Anchor::Bottom;
    };

    let top = Rectangle::new(Point::ORIGIN, Size::new(screen.width, bar));
    let bottom = Rectangle::new(
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

/// Area shared by two rectangles.
fn overlap(a: Rectangle, b: Rectangle) -> f32 {
    let width = (a.x + a.width).min(b.x + b.width) - a.x.max(b.x);
    let height = (a.y + a.height).min(b.y + b.height) - a.y.max(b.y);

    if width <= 0.0 || height <= 0.0 {
        0.0
    } else {
        width * height
    }
}

/// Whether a selection is big enough to be a deliberate drag rather than a
/// stray click.
pub fn is_usable(selection: Option<Selection>) -> bool {
    selection.is_some_and(|selection| {
        selection.0.width >= MIN_SELECTION && selection.0.height >= MIN_SELECTION
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let full = Rectangle::new(Point::ORIGIN, Size::new(100.0, 100.0));
        let hole = Rectangle::new(Point::new(20.0, 30.0), Size::new(40.0, 20.0));

        let covered: f32 = surround(full, hole).iter().map(|r| r.width * r.height).sum();

        assert_eq!(covered, 100.0 * 100.0 - 40.0 * 20.0);
    }

    #[test]
    fn a_selection_flush_with_an_edge_produces_no_zero_sized_dimming() {
        let full = Rectangle::new(Point::ORIGIN, Size::new(100.0, 100.0));
        let hole = Rectangle::new(Point::ORIGIN, Size::new(100.0, 40.0));

        assert!(surround(full, hole).iter().all(|r| r.width > 0.0 && r.height > 0.0));
    }

    #[test]
    fn scaling_to_pixels_respects_the_output_scale() {
        let selection = Selection(Rectangle::new(Point::new(10.0, 20.0), Size::new(30.0, 40.0)));

        let (x, y, w, h) = selection
            .to_pixels(2.0, Size::new(1000, 1000))
            .expect("non-empty");

        assert_eq!((x, y, w, h), (20, 40, 60, 80));
    }

    #[test]
    fn a_selection_running_past_the_edge_is_clamped_into_the_image() {
        let selection = Selection(Rectangle::new(Point::new(90.0, 0.0), Size::new(40.0, 10.0)));

        let (x, _, w, _) = selection
            .to_pixels(1.0, Size::new(100, 100))
            .expect("non-empty");

        assert_eq!((x, w), (90, 10));
    }

    const SCREEN: Size = Size {
        width: 1920.0,
        height: 1080.0,
    };
    const BAR: f32 = 60.0;

    fn at(x: f32, y: f32, w: f32, h: f32) -> Option<Selection> {
        Some(Selection(Rectangle::new(
            Point::new(x, y),
            Size::new(w, h),
        )))
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
    fn a_selection_covering_both_edges_takes_the_one_it_covers_least() {
        // Full height, but only clipping a sliver of the top strip.
        let selection = at(0.0, 40.0, 400.0, 1040.0);

        assert_eq!(toolbar_anchor(selection, SCREEN, BAR), Anchor::Top);
    }

    #[test]
    fn each_corner_points_along_the_diagonal_it_resizes() {
        assert_eq!(
            resize_cursor(Corner::TopLeft),
            mouse::Interaction::ResizingDiagonallyDown
        );
        assert_eq!(
            resize_cursor(Corner::BottomRight),
            mouse::Interaction::ResizingDiagonallyDown
        );
        assert_eq!(
            resize_cursor(Corner::TopRight),
            mouse::Interaction::ResizingDiagonallyUp
        );
        assert_eq!(
            resize_cursor(Corner::BottomLeft),
            mouse::Interaction::ResizingDiagonallyUp
        );
    }

    #[test]
    fn corners_are_found_within_the_grab_radius_and_not_beyond_it() {
        let rect = Rectangle::new(Point::new(100.0, 100.0), Size::new(200.0, 200.0));

        assert!(nearest_corner(rect, Point::new(104.0, 104.0)).is_some());
        assert!(nearest_corner(rect, Point::new(200.0, 200.0)).is_none());
    }

    #[test]
    fn a_stray_click_is_not_a_usable_selection() {
        let click = Selection(Rectangle::new(Point::ORIGIN, Size::new(1.0, 1.0)));

        assert!(!is_usable(Some(click)));
        assert!(!is_usable(None));
    }
}
