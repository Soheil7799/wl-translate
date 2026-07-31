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
    /// A screenshot came back, held for review rather than written out.
    Shot(Vec<u8>),
    PreviewOpened(window::Id),
    /// Enter / Space: keep it - save to disk and copy.
    ShotKeep,
    /// Ctrl+C: copy only, leave no file behind.
    ShotCopy,
    /// Esc: throw it away.
    ShotDiscard,
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
    /// A captured screenshot awaiting a decision.
    preview: Option<Preview>,
    preview_window: Option<window::Id>,
}

/// A screenshot on screen but not yet committed anywhere.
struct Preview {
    png: Vec<u8>,
    handle: image::Handle,
    /// Window size that shows this capture at roughly life size.
    window: iced::Size,
}

/// Width and height straight out of the PNG header.
///
/// A PNG is an 8-byte signature followed by the IHDR chunk, whose first two
/// fields are the dimensions - so this is a read, not a decode. Decoding a
/// full-screen capture just to learn how big it is would be wasteful.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

    if png.len() < 24 || &png[..8] != SIGNATURE || &png[12..16] != b"IHDR" {
        return None;
    }

    let width = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(png[20..24].try_into().ok()?);

    Some((width, height))
}

/// Window size for a capture: about as big as what was grabbed, plus room for
/// the button row, shrunk to fit on screen and floored so a tiny crop still
/// gives you something you can click.
fn window_size_for(png: &[u8]) -> iced::Size {
    // Padding on both sides, and the button row plus its spacing.
    const CHROME: (f32, f32) = (24.0, 62.0);
    const MAX: (f32, f32) = (1500.0, 850.0);
    const MIN: (f32, f32) = (420.0, 260.0);

    let (width, height) = png_size(png).unwrap_or((900, 560));
    let (mut width, mut height) = (width as f32, height as f32);

    // Shrink oversized captures on both axes at once, so the window keeps the
    // aspect ratio of the thing it is showing.
    let scale = (MAX.0 / width).min(MAX.1 / height).min(1.0);
    width *= scale;
    height *= scale;

    iced::Size::new(
        (width + CHROME.0).max(MIN.0),
        (height + CHROME.1).max(MIN.1),
    )
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
            preview: None,
            preview_window: None,
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
        if state.preview_window == Some(id) {
            "wl-translate screenshot".to_string()
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
            state.source = text_editor::Content::with_text(&outcome.source);
            state.target = text_editor::Content::with_text(&outcome.translation);
            state.detected = Some(outcome.from);
            state.status = None;
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
            if state.preview_window == Some(id) {
                state.preview_window = None;
                state.preview = None;
            }
            Task::none()
        }

        Message::Shot(png) => {
            state.preview = Some(Preview {
                handle: image::Handle::from_bytes(png.clone()),
                window: window_size_for(&png),
                png,
            });
            show_preview(state)
        }

        Message::PreviewOpened(id) => {
            state.preview_window = Some(id);
            Task::none()
        }

        Message::ShotKeep => {
            let saved = state.preview.as_ref().map(|preview| {
                let _ = shot::copy_image(&preview.png);
                shot::save(&preview.png)
            });

            match saved {
                Some(Ok(path)) => state.status = Some(format!("saved {}", path.display())),
                Some(Err(error)) => state.status = Some(format!("{error:#}")),
                None => {}
            }

            close_preview(state)
        }

        Message::ShotCopy => {
            if let Some(preview) = &state.preview {
                let _ = shot::copy_image(&preview.png);
                state.status = Some("copied".into());
            }
            close_preview(state)
        }

        Message::ShotDiscard => close_preview(state),

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

fn show_preview(state: &mut State) -> Task<Message> {
    if state.preview_window.is_some() {
        return Task::none();
    }

    let size = state
        .preview
        .as_ref()
        .map(|preview| preview.window)
        .unwrap_or(iced::Size::new(900.0, 620.0));

    let (id, task) = window::open(window::Settings {
        size,
        platform_specific: window::settings::PlatformSpecific {
            application_id: "wl-translate".to_string(),
            ..Default::default()
        },
        ..Default::default()
    });

    state.preview_window = Some(id);
    task.map(Message::PreviewOpened)
}

fn close_preview(state: &mut State) -> Task<Message> {
    state.preview = None;

    match state.preview_window.take() {
        Some(id) => window::close(id),
        None => Task::none(),
    }
}

/// The captured image, with what each key does spelled out underneath. The
/// shot is not on disk or on the clipboard yet - the keypress decides.
fn preview_view(preview: &Preview) -> Element<'_, Message> {
    column![
        container(
            image(preview.handle.clone()).content_fit(iced::ContentFit::Contain)
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill),
        row![
            button(text("Save + copy  (Enter)").size(12)).on_press(Message::ShotKeep),
            button(text("Copy only  (Ctrl+C)").size(12)).on_press(Message::ShotCopy),
            button(text("Discard  (Esc)").size(12)).on_press(Message::ShotDiscard),
        ]
        .spacing(8),
    ]
    .spacing(10)
    .into()
}

fn view(state: &State, window_id: window::Id) -> Element<'_, Message> {
    if state.preview_window == Some(window_id) {
        if let Some(preview) = &state.preview {
            return container(preview_view(preview))
                .padding(12)
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
        if _state.preview_window.is_some() {
            iced::keyboard::listen().map(review_key)
        } else {
            iced::keyboard::listen().map(translate_key)
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::{png_size, window_size_for};

    /// 1x1 transparent PNG.
    const TINY: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89,
    ];

    #[test]
    fn reads_dimensions_from_the_header() {
        assert_eq!(png_size(TINY), Some((1, 1)));
    }

    #[test]
    fn rejects_things_that_are_not_pngs() {
        assert_eq!(png_size(b"not a png at all, not even close"), None);
    }

    #[test]
    fn a_tiny_crop_still_gets_a_clickable_window() {
        let size = window_size_for(TINY);
        assert!(size.width >= 420.0 && size.height >= 260.0);
    }

    #[test]
    fn an_oversized_capture_is_shrunk_but_keeps_its_shape() {
        // Header for a 3840x2160 image.
        let mut png = TINY.to_vec();
        png[16..20].copy_from_slice(&3840u32.to_be_bytes());
        png[20..24].copy_from_slice(&2160u32.to_be_bytes());

        let size = window_size_for(&png);

        assert!(size.width <= 1500.0 + 24.0);
        assert!(size.height <= 850.0 + 62.0);

        let aspect = (size.width - 24.0) / (size.height - 62.0);
        assert!((aspect - 3840.0 / 2160.0).abs() < 0.01, "aspect drifted");
    }
}

/// While a screenshot is up for review, the keys commit or throw it away.
fn review_key(event: iced::keyboard::Event) -> Message {
    use iced::keyboard::key::Named;
    use iced::keyboard::{Event, Key};

    match event {
        Event::KeyPressed {
            key: Key::Named(Named::Enter | Named::Space),
            ..
        } => Message::ShotKeep,
        Event::KeyPressed {
            key: Key::Character(character),
            modifiers,
            ..
        } if modifiers.control() && character.as_str().eq_ignore_ascii_case("c") => {
            Message::ShotCopy
        }
        Event::KeyPressed {
            key: Key::Named(Named::Escape),
            ..
        } => Message::ShotDiscard,
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
                    Ok(Some(Product::Shot(png))) => Message::Shot(png),
                    Ok(None) => continue, // cancelled drag, or a UI-only verb
                    Err(error) => Message::Failed(error),
                },
                else => break,
            };

            let _ = output.send(message).await;
        }
    })
}
