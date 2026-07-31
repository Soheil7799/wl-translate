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

    wire_pointer(&canvas, &state);
    wire_keys(&window, &state, &canvas, &finished);

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

// ── input ──────────────────────────────────────────────────────────────────

fn wire_pointer(canvas: &gtk::DrawingArea, state: &Rc<RefCell<State>>) {
    let drag = gtk::GestureDrag::new();

    {
        let state = state.clone();
        let canvas = canvas.clone();

        drag.connect_drag_begin(move |_gesture, x, y| {
            let at = Point::new(x, y);
            let mut state = state.borrow_mut();

            if let Some(tool) = state.tool {
                let (ink, width) = tool.ink(state.ink, state.width);
                state.drawing = Some(Annotation::new(tool, at, ink, width));
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

            canvas.queue_draw();
        });
    }

    {
        let state = state.clone();
        let canvas = canvas.clone();

        drag.connect_drag_end(move |_gesture, _dx, _dy| {
            let mut state = state.borrow_mut();

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
                    state.borrow_mut().tool = None;
                    canvas.queue_draw();
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
            gdk::Key::p => set_tool(&state, &canvas, Tool::Pen),
            gdk::Key::a => set_tool(&state, &canvas, Tool::Arrow),
            gdk::Key::r => set_tool(&state, &canvas, Tool::Rectangle),
            gdk::Key::o => set_tool(&state, &canvas, Tool::Ellipse),
            gdk::Key::h => set_tool(&state, &canvas, Tool::Highlight),
            gdk::Key::b => set_tool(&state, &canvas, Tool::Blur),
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
                    }
                }
                return glib::Propagation::Proceed;
            }
        }

        glib::Propagation::Stop
    });

    window.add_controller(keys);
}

fn set_tool(state: &Rc<RefCell<State>>, canvas: &gtk::DrawingArea, tool: Tool) {
    let mut state = state.borrow_mut();

    // Picking the tool you already hold goes back to selecting, so one key both
    // enters and leaves a tool.
    state.tool = if state.tool == Some(tool) { None } else { Some(tool) };
    state.drawing = None;

    canvas.queue_draw();
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

    for (tool, icon) in [
        (Tool::Pen, "document-edit-symbolic"),
        (Tool::Arrow, "go-next-symbolic"),
        (Tool::Rectangle, "view-grid-symbolic"),
        (Tool::Ellipse, "media-record-symbolic"),
        (Tool::Highlight, "format-text-underline-symbolic"),
        (Tool::Blur, "view-conceal-symbolic"),
    ] {
        let button = gtk::Button::from_icon_name(icon);
        button.set_tooltip_text(Some(tool.label()));

        let state = state.clone();
        let canvas = canvas.clone();
        button.connect_clicked(move |_| set_tool(&state, &canvas, tool));

        tools.append(&button);
    }

    let undo = gtk::Button::from_icon_name("edit-undo-symbolic");
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

        let dot = gtk::DrawingArea::new();
        dot.set_size_request(16, 16);

        let ink = *ink;
        dot.set_draw_func(move |_area, cr, width, height| {
            cr.set_source_rgba(ink[0], ink[1], ink[2], 1.0);
            cr.arc(
                width as f64 / 2.0,
                height as f64 / 2.0,
                (width.min(height) as f64) / 2.0 - 1.0,
                0.0,
                std::f64::consts::TAU,
            );
            let _ = cr.fill();
        });

        swatch.set_child(Some(&dot));

        let state = state.clone();
        swatch.connect_clicked(move |_| state.borrow_mut().ink = ink);

        colors.append(&swatch);
    }

    let strip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    strip.append(&tools);
    strip.append(&colors);
    strip.set_halign(gtk::Align::Start);
    strip.set_valign(gtk::Align::Center);
    strip.set_margin_start(16);
    strip.add_css_class("osd");

    strip.upcast()
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

    bar.set_halign(gtk::Align::Center);
    bar.set_valign(gtk::Align::End);
    bar.set_margin_bottom(24);
    bar.add_css_class("osd");

    bar.upcast()
}
