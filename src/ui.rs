//! The resident daemon and its window.
//!
//! Built on `iced::daemon` rather than `iced::application` because that is
//! exactly this shape: a process that runs silently with no window and opens
//! one only when something triggers it.
//!
//! Threading:
//!
//!   D-Bus method ──► trigger channel ─┐
//!                                     ├─► subscription ──► update
//!   worker thread ──► outcome channel ┘                      │
//!         ▲                                                  │
//!         └──────────────── job channel ─────────────────────┘
//!
//! The worker thread owns the OCR engine for the life of the process, so the
//! language models load once instead of on every capture. It also means
//! `LepTess` never crosses a thread boundary, so it never needs to be `Send`.

use std::sync::OnceLock;
use std::time::Duration;

use iced::futures::{SinkExt, Stream};
use iced::widget::{button, column, container, image, row, rule, text, text_editor};
use iced::{window, Alignment, Element, Length, Subscription, Task, Theme};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::clip;
use crate::ipc;
use crate::pipeline::{Job, Outcome, Product, Verb, Worker};
use crate::overlay::{self, Selection};
use crate::shot;
use crate::settings::Settings;

/// Tesseract languages for the daemon's OCR engine. A `Subscription` is built
/// from a plain function pointer, which cannot capture, so the configured value
/// is parked here at startup.
static LANGS: OnceLock<String> = OnceLock::new();

/// How long to wait after the last keystroke before re-translating. Long enough
/// not to fire on every character, short enough to feel immediate.
const SETTLE: Duration = Duration::from_millis(350);

#[derive(Debug, Clone)]
pub enum Message {
    /// The worker is up; this is how to send it jobs.
    Ready(UnboundedSender<Job>),
    Trigger(Verb),
    Finished(Outcome),
    Failed(String),
    Opened(window::Id),
    /// The window went away by some route other than our Close button. Without
    /// this the stored id outlives the window and we never open another.
    Closed(window::Id),
    /// A frozen output came back; the overlay picks a region out of it.
    Captured(shot::Capture),
    /// The selection changed while dragging.
    Select(Selection),
    PreviewOpened(window::Id),
    /// What to do with the selected region. Each has a button and a key.
    Shot(Commit),
    SourceEdit(text_editor::Action),
    TargetEdit(text_editor::Action),
    PickSource(String),
    PickTarget(String),
    Swap,
    Copy,
    Dismiss,
    /// Debounce tick; carries the edit generation it was scheduled for.
    Settle(u64),
    Ignore,
}

pub struct State {
    settings: Settings,
    source: text_editor::Content,
    target: text_editor::Content,
    jobs: Option<UnboundedSender<Job>>,
    window: Option<window::Id>,
    /// What the engine detected, shown when the source side is on "auto".
    detected: Option<String>,
    status: Option<String>,
    /// Bumped on every edit so a stale debounce tick can be ignored.
    generation: u64,
    /// Set by "extract text", so the recognised text lands on the clipboard as
    /// well as in the window.
    copy_when_done: bool,
    /// A frozen output awaiting a selection and a decision.
    overlay: Option<Overlay>,
    overlay_window: Option<window::Id>,
}

/// What the overlay can do with the region you selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commit {
    Copy,
    Save,
    Extract,
    Translate,
    Close,
}

impl Commit {
    /// Label and the key that also triggers it.
    fn label(self) -> &'static str {
        match self {
            Commit::Copy => "Copy  (c)",
            Commit::Save => "Save  (Enter)",
            Commit::Extract => "Extract text  (e)",
            Commit::Translate => "Translate text  (t)",
            Commit::Close => "Close  (Esc)",
        }
    }
}

/// A frozen output on screen, with whatever is selected out of it.
struct Overlay {
    capture: shot::Capture,
    handle: image::Handle,
    selection: Option<Selection>,
    /// Output size in logical points, for deciding where the toolbar goes.
    screen: Option<iced::Size>,
}

/// The canvas asks for this so a drag can report itself.
impl From<Selection> for Message {
    fn from(selection: Selection) -> Self {
        Message::Select(selection)
    }
}

impl State {
    fn new() -> Self {
        Self {
            settings: Settings::load(),
            source: text_editor::Content::new(),
            target: text_editor::Content::new(),
            jobs: None,
            window: None,
            detected: None,
            status: None,
            generation: 0,
            copy_when_done: false,
            overlay: None,
            overlay_window: None,
        }
    }
}

/// Run the daemon. Blocks until the process is interrupted.
pub fn run(langs: Option<String>) -> anyhow::Result<()> {
    let langs = langs.unwrap_or_else(|| Settings::load().langs);
    let _ = LANGS.set(langs);

    iced::daemon(
        || (State::new(), Task::none()),
        update,
        view as fn(&State, window::Id) -> Element<Message>,
    )
    // The id becomes the Wayland app_id, which is what compositor rules match.
    .settings(iced::Settings {
        id: Some("wl-translate".to_string()),
        ..Default::default()
    })
    // Distinct titles so compositor rules can size the review window
    // differently from the translation window; they share a class.
    .title(|state: &State, id| {
        if state.overlay_window == Some(id) {
            "wl-translate overlay".to_string()
        } else {
            "wl-translate".to_string()
        }
    })
    .theme(|_state: &State, _id| Theme::Dark)
    .subscription(subscription)
    .run()
    .map_err(|e| anyhow::anyhow!("iced daemon failed: {e}"))
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Ready(sender) => {
            state.jobs = Some(sender);
            Task::none()
        }

        Message::Trigger(Verb::Show) => show(state),

        // Deliberately does NOT open the window.
        //
        // For OCR the very next thing that happens is slurp taking over the
        // screen for a region drag, and a window appearing first is both a
        // distraction and a focus thief. For the other verbs the result is
        // ~150ms away, so waiting means the window appears already filled in
        // rather than blank. Either way the window belongs to the result, not
        // to the request.
        Message::Trigger(verb) => {
            if state.window.is_some() {
                state.status = Some("working...".into());
            }
            dispatch(state, verb)
        }

        Message::Finished(outcome) => {
            if std::mem::take(&mut state.copy_when_done) {
                let _ = clip::copy(outcome.translation.trim());
                state.status = Some("copied".into());
            } else {
                state.status = None;
            }

            state.source = text_editor::Content::with_text(&outcome.source);
            state.target = text_editor::Content::with_text(&outcome.translation);
            state.detected = Some(outcome.from);
            show(state)
        }

        Message::Failed(error) => {
            state.status = Some(error);
            show(state)
        }

        Message::Opened(id) => {
            state.window = Some(id);
            Task::none()
        }

        Message::Closed(id) => {
            if state.window == Some(id) {
                state.window = None;
            }
            if state.overlay_window == Some(id) {
                state.overlay_window = None;
                state.overlay = None;
            }
            Task::none()
        }

        Message::Captured(capture) => {
            let selection = capture.preset.map(|(x, y, width, height)| {
                let scale = capture.scale;
                Selection(iced::Rectangle::new(
                    iced::Point::new(x as f32 / scale, y as f32 / scale),
                    iced::Size::new(width as f32 / scale, height as f32 / scale),
                ))
            });

            let screen = shot::png_dimensions(&capture.png).map(|(width, height)| {
                iced::Size::new(
                    width as f32 / capture.scale,
                    height as f32 / capture.scale,
                )
            });

            state.overlay = Some(Overlay {
                handle: image::Handle::from_bytes(capture.png.clone()),
                capture,
                selection,
                screen,
            });

            show_overlay(state)
        }

        Message::Select(selection) => {
            if let Some(overlay) = &mut state.overlay {
                overlay.selection = Some(selection);
            }
            Task::none()
        }

        Message::PreviewOpened(id) => {
            state.overlay_window = Some(id);
            Task::none()
        }

        Message::Shot(what) => commit(state, what),

        Message::SourceEdit(action) => {
            let changed = action.is_edit();
            state.source.perform(action);

            if !changed {
                return Task::none();
            }

            state.generation += 1;
            settle(state.generation)
        }

        // The translation pane is editable so you can tweak wording before
        // copying, but editing it must not trigger anything.
        Message::TargetEdit(action) => {
            state.target.perform(action);
            Task::none()
        }

        Message::PickSource(lang) => {
            state.settings.use_source(&lang);
            persist(&state.settings);
            retranslate(state)
        }

        Message::PickTarget(lang) => {
            state.settings.use_target(&lang);
            persist(&state.settings);
            retranslate(state)
        }

        Message::Swap => {
            state.settings.swap();
            persist(&state.settings);

            // Swap the panes too, so the thing you were reading becomes the
            // thing you are translating.
            let source = state.source.text();
            let target = state.target.text();
            state.source = text_editor::Content::with_text(target.trim());
            state.target = text_editor::Content::with_text(source.trim());

            retranslate(state)
        }

        Message::Settle(generation) => {
            if generation == state.generation {
                retranslate(state)
            } else {
                Task::none()
            }
        }

        Message::Copy => {
            let _ = clip::copy(state.target.text().trim());
            state.status = Some("copied".into());
            Task::none()
        }

        Message::Dismiss => match state.window.take() {
            Some(id) => window::close(id),
            None => Task::none(),
        },

        Message::Ignore => Task::none(),
    }
}

/// Settings failing to save should never break the thing you were doing, so it
/// surfaces on stderr rather than in the UI.
fn persist(settings: &Settings) {
    if let Err(error) = settings.save() {
        eprintln!("wl-translate: could not save settings: {error:#}");
    }
}

fn dispatch(state: &State, verb: Verb) -> Task<Message> {
    if let Some(sender) = &state.jobs {
        let mut job = Job::new(verb);
        job.from = state.settings.source.clone();
        job.to = state.settings.effective_target();
        job.engine = state.settings.engine.clone();
        job.freeze = state.settings.freeze;

        let _ = sender.send(job);
    }

    Task::none()
}

fn retranslate(state: &mut State) -> Task<Message> {
    let source = state.source.text().trim().to_string();

    if source.is_empty() {
        return Task::none();
    }

    dispatch(state, Verb::Text(source))
}

fn settle(generation: u64) -> Task<Message> {
    Task::perform(
        async move {
            tokio::time::sleep(SETTLE).await;
            generation
        },
        Message::Settle,
    )
}

fn show(state: &mut State) -> Task<Message> {
    if state.window.is_some() {
        return Task::none();
    }

    let (id, task) = window::open(window::Settings {
        size: iced::Size::new(760.0, 420.0),
        min_size: Some(iced::Size::new(420.0, 260.0)),
        // This, not `iced::Settings::id`, is what becomes the Wayland app_id.
        // Setting only the application-level id leaves the window with an empty
        // class, and compositor rules have nothing to match on.
        platform_specific: window::settings::PlatformSpecific {
            application_id: "wl-translate".to_string(),
            ..Default::default()
        },
        ..Default::default()
    });

    state.window = Some(id);
    task.map(Message::Opened)
}

/// Crop what is selected out of the frozen output, then act on it.
///
/// Nothing has touched the clipboard or the disk until this runs - that is the
/// whole point of the overlay.
fn commit(state: &mut State, what: Commit) -> Task<Message> {
    if what == Commit::Close {
        return close_overlay(state);
    }

    let Some(overlay) = &state.overlay else {
        return close_overlay(state);
    };

    if !overlay::is_usable(overlay.selection) {
        state.status = Some("nothing selected".into());
        return Task::none();
    }

    let image = shot::png_dimensions(&overlay.capture.png)
        .map(|(width, height)| iced::Size::new(width, height))
        .unwrap_or(iced::Size::new(0, 0));

    let Some(cropped) = overlay
        .selection
        .and_then(|selection| selection.to_pixels(overlay.capture.scale, image))
        .map(|(x, y, width, height)| shot::crop(&overlay.capture.png, x, y, width, height))
    else {
        state.status = Some("nothing selected".into());
        return Task::none();
    };

    let cropped = match cropped {
        Ok(cropped) => cropped,
        Err(error) => {
            state.status = Some(format!("{error:#}"));
            return close_overlay(state);
        }
    };

    match what {
        Commit::Copy => {
            state.status = match shot::copy_image(&cropped) {
                Ok(()) => Some("copied".into()),
                Err(error) => Some(format!("{error:#}")),
            };
        }

        Commit::Save => {
            let _ = shot::copy_image(&cropped);

            state.status = match shot::save(&cropped) {
                Ok(path) => Some(format!("saved {}", path.display())),
                Err(error) => Some(format!("{error:#}")),
            };
        }

        // Both hand the pixels straight to the OCR worker. Re-dragging a region
        // would be absurd when one has just been selected.
        Commit::Extract | Commit::Translate => {
            state.copy_when_done = what == Commit::Extract;

            let verb = Verb::OcrImage {
                png: cropped,
                raw: what == Commit::Extract,
            };

            let dispatched = dispatch(state, verb);
            let closed = close_overlay(state);

            return Task::batch([dispatched, closed]);
        }

        Commit::Close => unreachable!("handled above"),
    }

    close_overlay(state)
}

/// Fullscreen and undecorated/// Fullscreen and undecorated, so the frozen capture lines up pixel for pixel
/// with the screen it was taken from.
fn show_overlay(state: &mut State) -> Task<Message> {
    if state.overlay_window.is_some() {
        return Task::none();
    }

    // Fullscreen is the app's own decision, not a compositor rule's: the
    // overlay has to cover the output exactly or the selection stops lining up
    // with the capture behind it, and that is too important to depend on a
    // window rule being present and correct. The explicit size is a fallback
    // for anything that ignores the fullscreen request.
    //
    // A `float` rule must NOT be added for this window: floating it makes
    // Hyprland honour the toolkit's default 1024x768 instead, which is exactly
    // how the overlay ended up small.
    let size = state
        .overlay
        .as_ref()
        .and_then(|overlay| {
            shot::png_dimensions(&overlay.capture.png).map(|(width, height)| {
                iced::Size::new(
                    width as f32 / overlay.capture.scale,
                    height as f32 / overlay.capture.scale,
                )
            })
        })
        .unwrap_or(iced::Size::new(1920.0, 1080.0));

    // `fullscreen` ALONE. Pairing it with an explicit `size` made Hyprland treat
    // this as an ordinary sized window and tile it at 924x1050, and a `float`
    // rule made it fall back to the toolkit default of 1024x768. Both were
    // tried; only the bare fullscreen request actually covers the output.
    let _ = size;

    let (id, task) = window::open(window::Settings {
        fullscreen: true,
        decorations: false,
        platform_specific: window::settings::PlatformSpecific {
            application_id: "wl-translate-overlay".to_string(),
            ..Default::default()
        },
        ..Default::default()
    });

    state.overlay_window = Some(id);
    task.map(Message::PreviewOpened)
}

fn close_overlay(state: &mut State) -> Task<Message> {
    state.overlay = None;

    match state.overlay_window.take() {
        Some(id) => window::close(id),
        None => Task::none(),
    }
}

/// Height of the toolbar strip, used both to lay it out and to decide which
/// screen edge it can sit on without covering the selection.
const TOOLBAR: f32 = 72.0;

/// The action buttons, stuck to whichever screen edge the selection leaves
/// clear. They do not follow the selection around - they only get out of its
/// way.
fn toolbar(overlay: &Overlay) -> Element<'_, Message> {
    let screen = overlay
        .screen
        .unwrap_or(iced::Size::new(1920.0, 1080.0));

    let anchor = overlay::toolbar_anchor(overlay.selection, screen, TOOLBAR);

    let buttons = [
        Commit::Copy,
        Commit::Save,
        Commit::Extract,
        Commit::Translate,
        Commit::Close,
    ]
    .into_iter()
    .fold(row![].spacing(8), |strip, action| {
        strip.push(button(text(action.label()).size(12)).on_press(Message::Shot(action)))
    });

    container(buttons)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(match anchor {
            overlay::Anchor::Top => Alignment::Start,
            overlay::Anchor::Bottom => Alignment::End,
        })
        .padding(20)
        .into()
}

fn view(state: &State, window_id: window::Id) -> Element<'_, Message> {
    if state.overlay_window == Some(window_id) {
        if let Some(overlay) = &state.overlay {
            // No padding, no chrome: the canvas has to be exactly the window,
            // or the capture would be drawn scaled and every selection
            // coordinate would be off.
            return iced::widget::Stack::new()
                .push(
                    image(overlay.handle.clone())
                        .content_fit(iced::ContentFit::Fill)
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .push(
                    iced::widget::Canvas::new(overlay::Selector {
                        selection: overlay.selection,
                    })
                    .width(Length::Fill)
                    .height(Length::Fill),
                )
                .push(toolbar(overlay))
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        }
    }

    translate_view(state)
}

fn translate_view(state: &State) -> Element<'_, Message> {
    let source_label = match (&state.settings.source, &state.detected) {
        (source, Some(detected)) if source == "auto" => format!("auto - {detected}"),
        _ => String::new(),
    };

    let header = row![
        container(chips(
            Settings::chips(&state.settings.recent_source),
            &state.settings.source,
            Message::PickSource,
        ))
        .width(Length::FillPortion(1)),
        button(text("<>").size(12)).on_press(Message::Swap),
        container(chips(
            Settings::chips(&state.settings.recent_target),
            &state.settings.target,
            Message::PickTarget,
        ))
        .width(Length::FillPortion(1))
        .align_x(Alignment::End),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    // clip(true) contains a cosmic-text bidi bug: a line that mixes Latin into
    // right-to-left text (a Persian sentence containing "OCR", say) is wrapped
    // on the logical string and only then reordered visually, so the drawn line
    // comes out wider than the width it was wrapped to and bleeds past the left
    // edge into the neighbouring pane. Pure-Persian lines are fine. Clipping
    // keeps it inside its own box; the real fix belongs upstream.
    let panes = row![
        container(
            text_editor(&state.source)
                .on_action(Message::SourceEdit)
                .placeholder("Nothing captured yet")
                .height(Length::Fill)
        )
        .width(Length::FillPortion(1))
        .height(Length::Fill)
        .clip(true),
        container(
            text_editor(&state.target)
                .on_action(Message::TargetEdit)
                .placeholder("Translation")
                .height(Length::Fill)
        )
        .width(Length::FillPortion(1))
        .height(Length::Fill)
        .clip(true),
    ]
    .spacing(10)
    .height(Length::Fill);

    let footer = row![
        button(text("Copy").size(12)).on_press(Message::Copy),
        container(
            text(state.status.clone().unwrap_or(source_label))
                .size(11)
                .width(Length::Fill)
        )
        .width(Length::Fill),
        button(text("Close").size(12)).on_press(Message::Dismiss),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    container(
        column![header, rule::horizontal(1), panes, footer]
            .spacing(10)
            .height(Length::Fill),
    )
    .padding(12)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// One side's language row. "auto" leads, then most-recently-used.
///
/// Takes the list by value: it is built fresh each frame, and borrowing it
/// would tie the returned widget to a temporary.
fn chips(
    langs: Vec<String>,
    active: &str,
    on_pick: fn(String) -> Message,
) -> Element<'static, Message> {
    let mut strip = row![].spacing(4);

    for lang in langs {
        let is_active = lang == active;
        let chip = button(text(lang.clone()).size(11)).on_press(on_pick(lang));

        strip = strip.push(if is_active {
            chip.style(button::primary)
        } else {
            chip.style(button::text)
        });
    }

    strip.into()
}

fn subscription(_state: &State) -> Subscription<Message> {
    Subscription::batch([
        Subscription::run(events),
        window::close_events().map(Message::Closed),
        if _state.overlay_window.is_some() {
            iced::keyboard::listen().map(review_key)
        } else {
            iced::keyboard::listen().map(translate_key)
        },
    ])
}

/// While the overlay is up, every toolbar button also has a key.
fn review_key(event: iced::keyboard::Event) -> Message {
    use iced::keyboard::key::Named;
    use iced::keyboard::{Event, Key};

    let Event::KeyPressed { key, modifiers, .. } = event else {
        return Message::Ignore;
    };

    match key {
        Key::Named(Named::Enter | Named::Space) => Message::Shot(Commit::Save),
        Key::Named(Named::Escape) => Message::Shot(Commit::Close),
        Key::Character(character) => match character.as_str() {
            // Ctrl+C as well as plain c, since one is muscle memory and the
            // other is what the button says.
            "c" => Message::Shot(Commit::Copy),
            "s" if !modifiers.control() => Message::Shot(Commit::Save),
            "e" => Message::Shot(Commit::Extract),
            "t" => Message::Shot(Commit::Translate),
            _ => Message::Ignore,
        },
        _ => Message::Ignore,
    }
}

fn translate_key(event: iced::keyboard::Event) -> Message {
    use iced::keyboard::key::Named;
    use iced::keyboard::{Event, Key};

    match event {
        Event::KeyPressed {
            key: Key::Named(Named::Escape),
            ..
        } => Message::Dismiss,
        _ => Message::Ignore,
    }
}

/// Owns the D-Bus server and the worker thread for as long as the daemon runs.
fn events() -> impl Stream<Item = Message> {
    iced::stream::channel(32, async |mut output| {
        let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel::<Verb>();
        let (job_tx, mut job_rx) = mpsc::unbounded_channel::<Job>();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Result<Option<Product>, String>>();

        let langs = LANGS.get().cloned().unwrap_or_else(|| "eng".to_string());

        std::thread::spawn(move || {
            let mut worker = Worker::new(langs);

            while let Some(job) = job_rx.blocking_recv() {
                let result = worker.run(&job).map_err(|error| format!("{error:#}"));

                if out_tx.send(result).is_err() {
                    break; // daemon is shutting down
                }
            }
        });

        let served = async {
            zbus::connection::Builder::session()?
                .name(ipc::SERVICE)?
                .serve_at(
                    ipc::PATH,
                    ipc::Iface {
                        triggers: trigger_tx,
                    },
                )?
                .build()
                .await
        }
        .await;

        // Held for the lifetime of this future: dropping it unregisters the bus
        // name and the daemon silently stops answering.
        let _connection = match served {
            Ok(connection) => connection,
            Err(error) => {
                let _ = output
                    .send(Message::Failed(format!(
                        "could not claim {}: {error}",
                        ipc::SERVICE
                    )))
                    .await;
                return;
            }
        };

        let _ = output.send(Message::Ready(job_tx)).await;

        loop {
            let message = tokio::select! {
                Some(verb) = trigger_rx.recv() => Message::Trigger(verb),
                Some(result) = out_rx.recv() => match result {
                    Ok(Some(Product::Text(outcome))) => Message::Finished(outcome),
                    Ok(Some(Product::Shot(capture))) => Message::Captured(capture),
                    Ok(None) => continue, // cancelled drag, or a UI-only verb
                    Err(error) => Message::Failed(error),
                },
                else => break,
            };

            let _ = output.send(message).await;
        }
    })
}
