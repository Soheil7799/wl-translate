//! The selection overlay, on GTK4.
//!
//! Same model as before: the screen is captured up front and shown back
//! fullscreen, so the selection is made on a still image and stays adjustable
//! until a key or a button commits it.
//!
//! The layering is the lesson from the previous toolkit, restated. The frozen
//! capture is a `Picture`, which GTK hands to the GPU as a texture; only the
//! dimming, the selection and the annotations are drawn per frame, on a
//! `DrawingArea` above it. Drawing the capture itself every frame is what made
//! the first attempt stutter, and no amount of renderer choice fixes doing
//! millions of pixels of work per pointer event.
//!
//! All the geometry lives in [`crate::geom`] and every shape in
//! [`crate::annotate`], both toolkit-independent and tested, so this file is
//! only widgets, input and Cairo.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;

use crate::annotate::{self, Annotation, Tool};
use crate::geom::{self, Point, Rect, Size};
use crate::shot;

/// Height of the action bar and width of the tool strip, as the anchor rules
/// see them. Only used to decide which edge is free, so approximate is fine.
const ACTION_BAR: f64 = 84.0;
const TOOL_STRIP: f64 = 130.0;

/// Everything the overlay is holding while it is open.
struct State {
    capture: shot::Capture,
    screen: Size,
    selection: Option<Rect>,
    drag: Option<Drag>,
    tool: Option<Tool>,
    annotations: Vec<Annotation>,
    drawing: Option<Annotation>,
    undone: Vec<Annotation>,
    ink: [f64; 4],
    width: f64,
    /// True while a text annotation is being typed into. Every key belongs to
    /// the text while this is set, which is the whole reason it exists: the
    /// tools and commit actions are single letters, so typing "copy" would
    /// otherwise pick a colour, save the shot and close the overlay.
    typing: bool,
    /// Kept so the active indicator can be moved without rebuilding the strip.
    tool_buttons: Vec<(Option<Tool>, gtk::Button)>,
    swatches: Vec<gtk::DrawingArea>,
}

#[derive(Clone, Copy)]
enum Drag {
    New { origin: Point },
    Move { offset: (f64, f64) },
    Resize { anchor: Point },
    Draw,
}

/// What the overlay decided to do with the selection.
pub enum Done {
    /// Copied, saved, or both - nothing further to do.
    Handled(String),
    /// Recognise this image; `raw` skips translation.
    Recognise { png: Vec<u8>, raw: bool },
    Cancelled,
}

/// Open the overlay for a capture. `finished` is called once, when it closes.
pub fn present(
    app: &gtk::Application,
    capture: shot::Capture,
    finished: impl Fn(Done) + 'static,
) {
    let screen = shot::png_dimensions(&capture.png)
        .map(|(width, height)| {
            Size::new(
                width as f64 / capture.scale,
                height as f64 / capture.scale,
            )
        })
        .unwrap_or(Size::new(1920.0, 1080.0));

    let selection = capture.preset.map(|(x, y, w, h)| {
        let scale = capture.scale;
        Rect::new(
            Point::new(x as f64 / scale, y as f64 / scale),
            Size::new(w as f64 / scale, h as f64 / scale),
        )
    });

    let state = Rc::new(RefCell::new(State {
        capture,
        screen,
        selection,
        drag: None,
        tool: None,
        annotations: Vec::new(),
        drawing: None,
        undone: Vec::new(),
        ink: annotate::PALETTE[0].1,
        width: annotate::WIDTHS[1],
        typing: false,
        tool_buttons: Vec::new(),
        swatches: Vec::new(),
    }));

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .title("wl-translate overlay")
        .build();

    window.fullscreen();

    let picture = {
        let png = state.borrow().capture.png.clone();
        let texture = gdk::Texture::from_bytes(&glib::Bytes::from_owned(png)).ok();

        let picture = gtk::Picture::new();
        picture.set_paintable(texture.as_ref());
        // Fill, not Contain: the window is the output and the capture is that
        // output, so any letterboxing would mean the selection no longer lines
        // up with what is underneath it.
        picture.set_content_fit(gtk::ContentFit::Fill);
        picture
    };

    let canvas = gtk::DrawingArea::new();
    canvas.set_hexpand(true);
    canvas.set_vexpand(true);

    {
        let state = state.clone();
        canvas.set_draw_func(move |_area, cr, width, height| {
            draw(&state.borrow(), cr, width as f64, height as f64);
        });
    }

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&canvas);

    let finished = Rc::new(finished);
    let tools = tool_column(&state, &canvas);
    let actions = action_bar(&window, &state, &finished);

    overlay.add_overlay(&tools);
    overlay.add_overlay(&actions);

    window.set_child(Some(&overlay));

    wire_pointer(&canvas, &state, &tools, &actions);
    wire_keys(&window, &state, &canvas, &finished);

    // Window and screen presets arrive with a selection already made, so the
    // bars have to move before anything is dragged.
    reposition(&state.borrow(), &tools, &actions);

    window.present();
}

// ── drawing ────────────────────────────────────────────────────────────────

fn draw(state: &State, cr: &gtk::cairo::Context, width: f64, height: f64) {
    let full = Rect::new(Point::ORIGIN, Size::new(width, height));

    cr.set_source_rgba(0.0, 0.0, 0.0, 0.55);

    match state.selection {
        // Dim around the selection: Cairo has no hole-punch either, and four
        // rectangles is cheaper than compositing a mask.
        Some(rect) => {
            for around in geom::surround(full, rect) {
                cr.rectangle(around.x, around.y, around.width, around.height);
            }
            let _ = cr.fill();

            cr.set_source_rgb(0.45, 0.55, 1.0);
            cr.set_line_width(2.0);
            cr.rectangle(rect.x, rect.y, rect.width, rect.height);
            let _ = cr.stroke();

            for corner in geom::corners(rect) {
                cr.rectangle(
                    corner.x - geom::HANDLE_DRAW / 2.0,
                    corner.y - geom::HANDLE_DRAW / 2.0,
                    geom::HANDLE_DRAW,
                    geom::HANDLE_DRAW,
                );
            }
            let _ = cr.fill();
        }
        None => {
            cr.rectangle(0.0, 0.0, width, height);
            let _ = cr.fill();
        }
    }

    for annotation in state.annotations.iter().chain(state.drawing.as_ref()) {
        // Blur edits pixels rather than drawing, so the preview stands in for
        // it with a filled box; pixelating per pointer move would cost far more
        // than it tells you.
        if annotation.tool == Tool::Counter || annotation.tool == Tool::Text {
            annotate::draw_special(cr, annotation);
            continue;
        }

        if annotation.tool == Tool::Blur {
            if let Some(area) = annotation.bounds() {
                cr.set_source_rgba(0.08, 0.08, 0.10, 0.85);
                cr.rectangle(area.x, area.y, area.width, area.height);
                let _ = cr.fill();
            }
            continue;
        }

        let [red, green, blue, alpha] = annotation.color;
        cr.set_source_rgba(red, green, blue, alpha);
        cr.set_line_width(annotation.width);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);

        for line in annotate::outline(annotation) {
            for (index, point) in line.iter().enumerate() {
                if index == 0 {
                    cr.move_to(point.x, point.y);
                } else {
                    cr.line_to(point.x, point.y);
                }
            }
            let _ = cr.stroke();
        }
    }
}

/// Put the toolbars on whichever screen edges the selection leaves clear.
///
/// They stick to the screen rather than following the selection around - all
/// they do is get out of its way. Without this they sat where they were built
/// and a selection near an edge simply covered them.
fn reposition(state: &State, tools: &gtk::Widget, actions: &gtk::Widget) {
    match geom::toolbar_anchor(state.selection, state.screen, ACTION_BAR) {
        geom::Anchor::Top => {
            actions.set_valign(gtk::Align::Start);
            actions.set_margin_top(24);
            actions.set_margin_bottom(0);
        }
        geom::Anchor::Bottom => {
            actions.set_valign(gtk::Align::End);
            actions.set_margin_top(0);
            actions.set_margin_bottom(24);
        }
    }

    match geom::sidebar_anchor(state.selection, state.screen, TOOL_STRIP) {
        geom::Side::Left => {
            tools.set_halign(gtk::Align::Start);
            tools.set_margin_start(16);
            tools.set_margin_end(0);
        }
        geom::Side::Right => {
            tools.set_halign(gtk::Align::End);
            tools.set_margin_start(0);
            tools.set_margin_end(16);
        }
    }
}

// ── input ──────────────────────────────────────────────────────────────────

fn wire_pointer(
    canvas: &gtk::DrawingArea,
    state: &Rc<RefCell<State>>,
    tools: &gtk::Widget,
    actions: &gtk::Widget,
) {
    let drag = gtk::GestureDrag::new();

    {
        let state = state.clone();
        let canvas = canvas.clone();

        drag.connect_drag_begin(move |_gesture, x, y| {
            let at = Point::new(x, y);
            let mut state = state.borrow_mut();

            if let Some(tool) = state.tool {
                let (ink, width) = tool.ink(state.ink, state.width);
                let mut annotation = Annotation::new(tool, at, ink, width);

                // Numbered from how many counters are already placed, rather
                // than a running total: undo then hands the number back instead
                // of leaving a gap in the sequence.
                if tool == Tool::Counter {
                    annotation.index = state
                        .annotations
                        .iter()
                        .filter(|a| a.tool == Tool::Counter)
                        .count() as u32
                        + 1;
                }

                state.typing = tool == Tool::Text;
                state.drawing = Some(annotation);
                state.drag = Some(Drag::Draw);
            } else {
                state.drag = Some(match state.selection {
                    Some(rect) => match geom::nearest_corner(rect, at) {
                        Some(corner) => Drag::Resize {
                            anchor: geom::opposite(rect, corner),
                        },
                        None if rect.contains(at) => Drag::Move {
                            offset: (at.x - rect.x, at.y - rect.y),
                        },
                        None => Drag::New { origin: at },
                    },
                    None => Drag::New { origin: at },
                });
            }

            canvas.queue_draw();
        });
    }

    {
        let state = state.clone();
        let canvas = canvas.clone();
        let tools = tools.clone();
        let actions = actions.clone();

        drag.connect_drag_update(move |gesture, dx, dy| {
            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };

            let at = Point::new(start_x + dx, start_y + dy);
            let mut state = state.borrow_mut();

            match state.drag {
                Some(Drag::Draw) => {
                    if let Some(drawing) = &mut state.drawing {
                        drawing.extend(at);
                    }
                }
                Some(Drag::New { origin }) => {
                    state.selection = Some(geom::from_corners(origin, at));
                }
                Some(Drag::Resize { anchor }) => {
                    state.selection = Some(geom::from_corners(anchor, at));
                }
                Some(Drag::Move { offset }) => {
                    if let Some(rect) = state.selection {
                        let limit = state.screen;
                        let x = (at.x - offset.0).clamp(0.0, (limit.width - rect.width).max(0.0));
                        let y = (at.y - offset.1).clamp(0.0, (limit.height - rect.height).max(0.0));

                        state.selection =
                            Some(Rect::new(Point::new(x, y), Size::new(rect.width, rect.height)));
                    }
                }
                None => {}
            }

            reposition(&state, &tools, &actions);
            canvas.queue_draw();
        });
    }

    {
        let state = state.clone();
        let canvas = canvas.clone();

        drag.connect_drag_end(move |_gesture, _dx, _dy| {
            let mut state = state.borrow_mut();

            // Text is still being typed at this point, so releasing the button
            // must leave it alone. It is committed by Enter or Escape.
            if state.typing {
                state.drag = None;
                canvas.queue_draw();
                return;
            }

            // A click that drew nothing is dropped, rather than left as an
            // invisible entry that Undo would appear to ignore.
            if let Some(drawing) = state.drawing.take() {
                if drawing.is_usable() {
                    state.annotations.push(drawing);
                    state.undone.clear();
                }
            }

            state.drag = None;
            canvas.queue_draw();
        });
    }

    canvas.add_controller(drag);

    // The pointer should say what a drag here would do, before you commit to
    // it: a crosshair to draw a region, a four-way arrow to move one, and the
    // matching diagonal to resize from a corner.
    let motion = gtk::EventControllerMotion::new();

    {
        let state = state.clone();
        let canvas = canvas.clone();

        motion.connect_motion(move |_controller, x, y| {
            let at = Point::new(x, y);
            let state = state.borrow();

            let name = if state.tool.is_some() {
                "crosshair"
            } else {
                match state.selection {
                    Some(rect) => match geom::nearest_corner(rect, at) {
                        Some(geom::Corner::TopLeft) | Some(geom::Corner::BottomRight) => {
                            "nwse-resize"
                        }
                        Some(geom::Corner::TopRight) | Some(geom::Corner::BottomLeft) => {
                            "nesw-resize"
                        }
                        None if rect.contains(at) => "move",
                        None => "crosshair",
                    },
                    None => "crosshair",
                }
            };

            canvas.set_cursor_from_name(Some(name));
        });
    }

    canvas.add_controller(motion);
}

fn wire_keys(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<State>>,
    canvas: &gtk::DrawingArea,
    finished: &Rc<impl Fn(Done) + 'static>,
) {
    let keys = gtk::EventControllerKey::new();

    let state = state.clone();
    let canvas = canvas.clone();
    let finished = finished.clone();
    // Cloned for the closure; the original is still needed below to attach the
    // controller to.
    let owner = window.clone();

    keys.connect_key_pressed(move |_controller, key, _code, modifiers| {
        let control = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);

        // While typing, the keyboard belongs to the text and nothing else.
        if state.borrow().typing {
            let mut state = state.borrow_mut();

            match key {
                // Enter keeps it, Escape throws it away. Shift+Enter is a
                // newline, so a caption can be more than one line.
                gdk::Key::Return | gdk::Key::KP_Enter if !shift => {
                    state.typing = false;

                    if let Some(drawing) = state.drawing.take() {
                        if drawing.is_usable() {
                            state.annotations.push(drawing);
                            state.undone.clear();
                        }
                    }
                }
                gdk::Key::Escape => {
                    state.typing = false;
                    state.drawing = None;
                }
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    if let Some(drawing) = &mut state.drawing {
                        drawing.text.push('\n');
                    }
                }
                gdk::Key::BackSpace => {
                    if let Some(drawing) = &mut state.drawing {
                        drawing.text.pop();
                    }
                }
                _ => {
                    if let Some(character) = key.to_unicode() {
                        if !character.is_control() {
                            if let Some(drawing) = &mut state.drawing {
                                drawing.text.push(character);
                            }
                        }
                    }
                }
            }

            drop(state);
            canvas.queue_draw();
            return glib::Propagation::Stop;
        }

        let commit = |what: Commit| {
            let done = commit(&state.borrow(), what);
            owner.close();
            finished(done);
        };

        match key {
            gdk::Key::Return | gdk::Key::space => commit(Commit::Save),
            gdk::Key::Escape => {
                // Esc leaves the tool first, so it can never throw away a
                // capture you were still drawing on.
                let holding = state.borrow().tool.is_some();

                if holding {
                    set_tool(&state, &canvas, None);
                } else {
                    owner.close();
                    finished(Done::Cancelled);
                }
            }
            gdk::Key::c if control => commit(Commit::Copy),
            gdk::Key::c => commit(Commit::Copy),
            gdk::Key::s => commit(Commit::Save),
            gdk::Key::e => commit(Commit::Extract),
            gdk::Key::t => commit(Commit::Translate),
            gdk::Key::z if control && shift => {
                let mut state = state.borrow_mut();
                if let Some(annotation) = state.undone.pop() {
                    state.annotations.push(annotation);
                }
                canvas.queue_draw();
            }
            gdk::Key::z if control => {
                let mut state = state.borrow_mut();
                if let Some(annotation) = state.annotations.pop() {
                    state.undone.push(annotation);
                }
                canvas.queue_draw();
            }
            gdk::Key::v => set_tool(&state, &canvas, None),
            gdk::Key::p => set_tool(&state, &canvas, Some(Tool::Pen)),
            gdk::Key::a => set_tool(&state, &canvas, Some(Tool::Arrow)),
            gdk::Key::r => set_tool(&state, &canvas, Some(Tool::Rectangle)),
            gdk::Key::o => set_tool(&state, &canvas, Some(Tool::Ellipse)),
            gdk::Key::h => set_tool(&state, &canvas, Some(Tool::Highlight)),
            gdk::Key::b => set_tool(&state, &canvas, Some(Tool::Blur)),
            gdk::Key::n => set_tool(&state, &canvas, Some(Tool::Counter)),
            gdk::Key::x => set_tool(&state, &canvas, Some(Tool::Text)),
            gdk::Key::w => {
                let mut state = state.borrow_mut();
                let next = annotate::WIDTHS
                    .iter()
                    .position(|w| (*w - state.width).abs() < 0.01)
                    .map(|index| (index + 1) % annotate::WIDTHS.len())
                    .unwrap_or(0);
                state.width = annotate::WIDTHS[next];
            }
            _ => {
                // Digits pick a colour; the swatches are numbered to match.
                if let Some(digit) = key.to_unicode().and_then(|c| c.to_digit(10)) {
                    let index = digit as usize;

                    if index >= 1 && index <= annotate::PALETTE.len() {
                        state.borrow_mut().ink = annotate::PALETTE[index - 1].1;
                        pick_colour(&state);
                    }
                }
                return glib::Propagation::Proceed;
            }
        }

        glib::Propagation::Stop
    });

    window.add_controller(keys);
}

/// Choose a tool. `None` is the select/move tool, which is a tool like any
/// other rather than the absence of one.
///
/// Deliberately sets rather than toggles: clicking the tool you are already
/// holding used to switch you back to selecting, which meant a stray second
/// click silently changed what the next drag would do. Going back to selecting
/// is now its own button, so the mode is always something you chose.
fn set_tool(state: &Rc<RefCell<State>>, canvas: &gtk::DrawingArea, tool: Option<Tool>) {
    {
        let mut state = state.borrow_mut();
        state.tool = tool;
        state.drawing = None;
    }

    refresh_tools(state);
    canvas.queue_draw();
}

/// Re-mark which tool button is active. Tools and colours have separate
/// indicators on purpose: they are independent choices, and sharing one made
/// picking a colour look like it had changed the tool.
fn refresh_tools(state: &Rc<RefCell<State>>) {
    let current = state.borrow().tool;

    for (tool, button) in state.borrow().tool_buttons.iter() {
        if *tool == current {
            button.add_css_class("suggested-action");
        } else {
            button.remove_css_class("suggested-action");
        }
    }
}

// ── committing ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Commit {
    Copy,
    Save,
    Extract,
    Translate,
}

fn commit(state: &State, what: Commit) -> Done {
    if !geom::is_usable(state.selection) {
        return Done::Cancelled;
    }

    let image = shot::png_dimensions(&state.capture.png).unwrap_or((0, 0));

    let Some(region) = state
        .selection
        .and_then(|rect| geom::to_pixels(rect, state.capture.scale, image))
    else {
        return Done::Cancelled;
    };

    let cropped = match shot::crop(&state.capture.png, region.0, region.1, region.2, region.3) {
        Ok(cropped) => cropped,
        Err(error) => return Done::Handled(format!("{error:#}")),
    };

    // Annotations are recorded in screen points; the crop starts at the
    // selection's corner, so shifting by that and scaling lands them where they
    // were drawn.
    let origin = state
        .selection
        .map(|rect| rect.position())
        .unwrap_or(Point::ORIGIN);

    let cropped = match annotate::rasterize(
        &state.annotations,
        &cropped,
        origin,
        state.capture.scale,
    ) {
        Ok(annotated) => annotated,
        Err(error) => return Done::Handled(format!("{error:#}")),
    };

    match what {
        Commit::Copy => match shot::copy_image(&cropped) {
            Ok(()) => Done::Handled("copied".into()),
            Err(error) => Done::Handled(format!("{error:#}")),
        },
        Commit::Save => {
            let _ = shot::copy_image(&cropped);

            match shot::save(&cropped) {
                Ok(path) => Done::Handled(format!("saved {}", path.display())),
                Err(error) => Done::Handled(format!("{error:#}")),
            }
        }
        Commit::Extract => Done::Recognise {
            png: cropped,
            raw: true,
        },
        Commit::Translate => Done::Recognise {
            png: cropped,
            raw: false,
        },
    }
}

// ── toolbars ───────────────────────────────────────────────────────────────

/// Tools and colours, side by side: one narrow column of icon buttons, and the
/// swatches in their own column next to it. Icons rather than words because the
/// text labels were wider than the thing they described.
fn tool_column(state: &Rc<RefCell<State>>, canvas: &gtk::DrawingArea) -> gtk::Widget {
    let tools = gtk::Box::new(gtk::Orientation::Vertical, 4);

    for (tool, icon, tip) in [
        (None, "find-location-symbolic", "Select and move  (v)"),
        (Some(Tool::Pen), "document-edit-symbolic", Tool::Pen.label()),
        (Some(Tool::Arrow), "go-next-symbolic", Tool::Arrow.label()),
        (
            Some(Tool::Rectangle),
            "view-grid-symbolic",
            Tool::Rectangle.label(),
        ),
        (
            Some(Tool::Ellipse),
            "media-record-symbolic",
            Tool::Ellipse.label(),
        ),
        (
            Some(Tool::Highlight),
            "format-text-underline-symbolic",
            Tool::Highlight.label(),
        ),
        (Some(Tool::Blur), "view-conceal-symbolic", Tool::Blur.label()),
        (
            Some(Tool::Counter),
            "list-add-symbolic",
            Tool::Counter.label(),
        ),
        (
            Some(Tool::Text),
            "insert-text-symbolic",
            Tool::Text.label(),
        ),
    ] {
        let button = gtk::Button::from_icon_name(icon);
        button.set_tooltip_text(Some(tip));
        button.add_css_class("flat");

        {
            let state = state.clone();
            let canvas = canvas.clone();
            button.connect_clicked(move |_| set_tool(&state, &canvas, tool));
        }

        state.borrow_mut().tool_buttons.push((tool, button.clone()));
        tools.append(&button);
    }

    refresh_tools(state);

    let undo = gtk::Button::from_icon_name("edit-undo-symbolic");
    undo.add_css_class("flat");
    undo.set_tooltip_text(Some("Undo  (Ctrl+Z)"));
    {
        let state = state.clone();
        let canvas = canvas.clone();
        undo.connect_clicked(move |_| {
            let mut state = state.borrow_mut();
            if let Some(annotation) = state.annotations.pop() {
                state.undone.push(annotation);
            }
            canvas.queue_draw();
        });
    }
    tools.append(&undo);

    let redo = gtk::Button::from_icon_name("edit-redo-symbolic");
    redo.add_css_class("flat");
    redo.set_tooltip_text(Some("Redo  (Ctrl+Shift+Z)"));
    {
        let state = state.clone();
        let canvas = canvas.clone();
        redo.connect_clicked(move |_| {
            let mut state = state.borrow_mut();
            if let Some(annotation) = state.undone.pop() {
                state.annotations.push(annotation);
            }
            canvas.queue_draw();
        });
    }
    tools.append(&redo);

    // Colours get a column of their own, beside the tools rather than under
    // them, so neither list makes the other taller.
    let colors = gtk::Box::new(gtk::Orientation::Vertical, 4);

    for (index, (name, ink)) in annotate::PALETTE.iter().enumerate() {
        let swatch = gtk::Button::new();
        swatch.set_tooltip_text(Some(&format!("{name}  ({})", index + 1)));
        swatch.set_size_request(28, 28);
        swatch.add_css_class("flat");
        swatch.add_css_class("circular");

        let dot = gtk::DrawingArea::new();
        dot.set_size_request(16, 16);

        let ink = *ink;

        {
            // The selected colour is marked by a ring around its own swatch,
            // not by the button styling the tools use - otherwise picking a
            // colour looks like it changed which tool is active.
            let state = state.clone();

            dot.set_draw_func(move |_area, cr, width, height| {
                let centre = (width as f64 / 2.0, height as f64 / 2.0);
                let radius = (width.min(height) as f64) / 2.0 - 2.0;

                cr.set_source_rgba(ink[0], ink[1], ink[2], 1.0);
                cr.arc(centre.0, centre.1, radius, 0.0, std::f64::consts::TAU);
                let _ = cr.fill();

                if state.borrow().ink == ink {
                    cr.set_source_rgb(1.0, 1.0, 1.0);
                    cr.set_line_width(2.0);
                    cr.arc(centre.0, centre.1, radius + 1.0, 0.0, std::f64::consts::TAU);
                    let _ = cr.stroke();
                }
            });
        }

        swatch.set_child(Some(&dot));
        state.borrow_mut().swatches.push(dot.clone());

        {
            let state = state.clone();
            swatch.connect_clicked(move |_| {
                state.borrow_mut().ink = ink;
                pick_colour(&state);
            });
        }

        colors.append(&swatch);
    }

    tools.set_valign(gtk::Align::Center);
    colors.set_valign(gtk::Align::Center);

    let strip = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    strip.append(&tools);
    strip.append(&colors);
    // Horizontal placement is decided by `reposition`; only the vertical
    // centring is fixed.
    strip.set_valign(gtk::Align::Center);

    // `osd` and `toolbar` are GTK's own style classes, so the strip picks up
    // whatever the system theme says an overlay toolbar looks like - including
    // light/dark - instead of carrying colours of its own.
    strip.add_css_class("osd");
    strip.add_css_class("toolbar");
    strip.set_margin_top(8);
    strip.set_margin_bottom(8);

    strip.upcast()
}

/// Redraw every swatch so the ring follows the choice.
fn pick_colour(state: &Rc<RefCell<State>>) {
    for swatch in state.borrow().swatches.iter() {
        swatch.queue_draw();
    }
}

fn action_bar(
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<State>>,
    finished: &Rc<impl Fn(Done) + 'static>,
) -> gtk::Widget {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    for (what, icon, tip) in [
        (Some(Commit::Copy), "edit-copy-symbolic", "Copy  (c)"),
        (Some(Commit::Save), "document-save-symbolic", "Save  (Enter)"),
        (
            Some(Commit::Extract),
            "insert-text-symbolic",
            "Extract text  (e)",
        ),
        (
            Some(Commit::Translate),
            "accessories-dictionary-symbolic",
            "Translate text  (t)",
        ),
        (None, "window-close-symbolic", "Close  (Esc)"),
    ] {
        let button = gtk::Button::from_icon_name(icon);
        button.set_tooltip_text(Some(tip));
        button.add_css_class("flat");

        let state = state.clone();
        let window = window.clone();
        let finished = finished.clone();

        button.connect_clicked(move |_| {
            let done = match what {
                Some(what) => commit(&state.borrow(), what),
                None => Done::Cancelled,
            };

            window.close();
            finished(done);
        });

        bar.append(&button);
    }

    // Vertical placement is decided by `reposition`.
    bar.set_halign(gtk::Align::Center);
    bar.add_css_class("osd");
    bar.add_css_class("toolbar");

    bar.upcast()
}
