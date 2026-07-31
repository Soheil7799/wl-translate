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
use iced::widget::{button, column, container, row, rule, text, text_editor};
use iced::{window, Alignment, Element, Length, Subscription, Task, Theme};
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::clip;
use crate::ipc;
use crate::pipeline::{Job, Outcome, Verb, Worker};
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
    .title(|_state: &State, _id| "wl-translate".to_string())
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

        Message::Trigger(verb) => {
            state.status = Some("working...".into());
            let dispatched = dispatch(state, verb);
            Task::batch([dispatched, show(state)])
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
            Task::none()
        }

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
        ..Default::default()
    });

    state.window = Some(id);
    task.map(Message::Opened)
}

fn view(state: &State, _window: window::Id) -> Element<'_, Message> {
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

    let panes = row![
        text_editor(&state.source)
            .on_action(Message::SourceEdit)
            .placeholder("Nothing captured yet")
            .height(Length::Fill),
        text_editor(&state.target)
            .on_action(Message::TargetEdit)
            .placeholder("Translation")
            .height(Length::Fill),
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
        iced::keyboard::listen().map(|event| match event {
            iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
                ..
            } => Message::Dismiss,
            _ => Message::Ignore,
        }),
    ])
}

/// Owns the D-Bus server and the worker thread for as long as the daemon runs.
fn events() -> impl Stream<Item = Message> {
    iced::stream::channel(32, async |mut output| {
        let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel::<Verb>();
        let (job_tx, mut job_rx) = mpsc::unbounded_channel::<Job>();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Result<Option<Outcome>, String>>();

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
                    Ok(Some(outcome)) => Message::Finished(outcome),
                    Ok(None) => continue, // cancelled drag, or a UI-only verb
                    Err(error) => Message::Failed(error),
                },
                else => break,
            };

            let _ = output.send(message).await;
        }
    })
}
