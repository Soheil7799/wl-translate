//! The resident daemon and its popup.
//!
//! Built on `iced::daemon` rather than `iced::application` because that is
//! exactly this shape: a process that runs silently with no window and opens
//! one only when something triggers it.
//!
//! Threading, which is the whole reason this is fast:
//!
//!   D-Bus method  ──►  job channel  ──►  worker thread (owns tesseract)
//!                                              │
//!        iced subscription  ◄── outcome channel┘
//!                │
//!             update ──► window::open
//!
//! The worker thread owns the OCR engine for the life of the process, so the
//! language models load once instead of on every capture. It also means
//! `LepTess` never has to cross a thread boundary, so it never needs to be
//! `Send`.

use std::sync::OnceLock;

use iced::futures::{SinkExt, Stream};
use iced::widget::{button, column, container, row, rule, scrollable, text};
use iced::{window, Element, Length, Subscription, Task, Theme};
use tokio::sync::mpsc;

use crate::clip;
use crate::ipc;
use crate::pipeline::{Job, Outcome, Worker};

/// Tesseract languages for the daemon's OCR engine. A `Subscription` is built
/// from a plain function pointer, which cannot capture, so the configured value
/// is parked here at startup.
static LANGS: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone)]
pub enum Message {
    Finished(Outcome),
    Failed(String),
    Opened(window::Id),
    Copy,
    Dismiss,
    Ignore,
}

#[derive(Default)]
pub struct State {
    entry: Option<Outcome>,
    error: Option<String>,
    window: Option<window::Id>,
    copied: bool,
}

/// Run the daemon. Blocks until the process is interrupted.
pub fn run(langs: String) -> anyhow::Result<()> {
    let _ = LANGS.set(langs);

    iced::daemon(
        || (State::default(), Task::none()),
        update,
        view as fn(&State, window::Id) -> Element<Message>,
    )
    // The id becomes the Wayland app_id. Without it the window has an empty
    // class and compositor rules have nothing to match on, so it gets tiled
    // like an ordinary window instead of floating as a popup.
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
        Message::Finished(outcome) => {
            state.entry = Some(outcome);
            state.error = None;
            state.copied = false;
            show(state)
        }
        Message::Failed(error) => {
            state.error = Some(error);
            state.copied = false;
            show(state)
        }
        Message::Opened(id) => {
            state.window = Some(id);
            Task::none()
        }
        Message::Copy => {
            if let Some(entry) = &state.entry {
                let _ = clip::copy(&entry.translation);
                state.copied = true;
            }
            Task::none()
        }
        Message::Dismiss => match state.window.take() {
            Some(id) => window::close(id),
            None => Task::none(),
        },
        Message::Ignore => Task::none(),
    }
}

/// Open the popup if it is not already up. If it is, the new content simply
/// replaces the old - re-triggering should not stack windows.
fn show(state: &mut State) -> Task<Message> {
    if state.window.is_some() {
        return Task::none();
    }

    let (id, task) = window::open(window::Settings {
        size: iced::Size::new(720.0, 440.0),
        min_size: Some(iced::Size::new(360.0, 240.0)),
        ..Default::default()
    });

    state.window = Some(id);
    task.map(Message::Opened)
}

fn view(state: &State, _window: window::Id) -> Element<'_, Message> {
    let body: Element<Message> = if let Some(error) = &state.error {
        column![text("Something went wrong").size(18), text(error).size(14)]
            .spacing(8)
            .into()
    } else if let Some(entry) = &state.entry {
        // Each side is aligned by its own language. `Alignment::Default` claims
        // to align right-to-left text to the right on its own, but in practice
        // it does not here, so the direction is decided explicitly.
        column![
            text(format!("{} → {}", entry.from, entry.to)).size(12),
            scrollable(
                container(text(&entry.source).size(14))
                    .width(Length::Fill)
                    .align_x(side(&entry.from))
            )
            .width(Length::Fill)
            .height(Length::FillPortion(2)),
            rule::horizontal(1),
            scrollable(
                container(text(&entry.translation).size(20))
                    .width(Length::Fill)
                    .align_x(side(&entry.to))
            )
            .width(Length::Fill)
            .height(Length::FillPortion(3)),
            row![
                button(text(if state.copied { "Copied" } else { "Copy" })).on_press(Message::Copy),
                button(text("Close")).on_press(Message::Dismiss),
            ]
            .spacing(8),
        ]
        .spacing(12)
        .into()
    } else {
        text("Waiting for something to translate...").into()
    };

    // Fill in both directions on purpose. A container sizes to its content by
    // default, and a Fill child inside a Shrink parent has no width to resolve
    // against - which sends right-aligned text off the edge of the window.
    container(body)
        .padding(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Which edge a language's text should sit against.
///
/// Language codes rather than sniffing the characters: a Persian sentence
/// containing a number or a Latin brand name should still align right, and the
/// language is already known for free.
fn side(lang: &str) -> iced::Alignment {
    const RTL: [&str; 10] = ["fa", "ar", "he", "iw", "ur", "ps", "sd", "ug", "yi", "dv"];

    let base = lang.split(['-', '_']).next().unwrap_or(lang);

    if RTL.contains(&base) {
        iced::Alignment::End
    } else {
        iced::Alignment::Start
    }
}

fn subscription(_state: &State) -> Subscription<Message> {
    Subscription::batch([
        Subscription::run(events),
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
                .serve_at(ipc::PATH, ipc::Iface { jobs: job_tx })?
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

        while let Some(result) = out_rx.recv().await {
            let message = match result {
                Ok(Some(outcome)) => Message::Finished(outcome),
                Ok(None) => continue, // user cancelled the region drag
                Err(error) => Message::Failed(error),
            };

            let _ = output.send(message).await;
        }
    })
}
