//! The resident daemon and its translator window, on GTK4.
//!
//! Why GTK: this window exists to *edit* text, often Persian. Pango implements
//! the Unicode bidirectional algorithm and Arabic shaping properly, including
//! caret movement and selection through mixed right-to-left and left-to-right
//! runs. That last part is what the previous toolkit got wrong - it rendered
//! Persian correctly but could not put the caret in the right place inside it,
//! which makes an editor useless for the language it is most needed for.
//!
//! Threading is unchanged from before; only the destination differs:
//!
//!   D-Bus (tokio thread) ──┐
//!                          ├──► async channel ──► GTK main loop
//!   worker thread ─────────┘
//!
//! The worker still owns the OCR engine for the process lifetime, so tesseract
//! loads once and `LepTess` never crosses a thread boundary.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::ipc;
use crate::pipeline::{Job, Outcome, Product, Verb, Worker};
use crate::settings::Settings;

static LANGS: OnceLock<String> = OnceLock::new();

/// Anything the background threads need the UI to know about.
enum Event {
    Finished(Outcome),
    Failed(String),
}

/// Widgets the update path needs to reach back into.
struct Window {
    window: gtk::ApplicationWindow,
    source: gtk::TextView,
    target: gtk::TextView,
    status: gtk::Label,
    chips: gtk::Box,
    settings: RefCell<Settings>,
    jobs: std::sync::mpsc::Sender<Job>,
}

/// Run the daemon. Blocks until the process is interrupted.
pub fn run(langs: Option<String>) -> Result<()> {
    let langs = langs.unwrap_or_else(|| Settings::load().langs);
    let _ = LANGS.set(langs.clone());

    let app = gtk::Application::builder()
        .application_id("org.wl_translate.Gtk")
        // NOT IS_SERVICE: that defers activation until something calls the
        // app over its own D-Bus name, so  never fired and the
        // window and its channel readers were never created. Our D-Bus is
        // served separately by zbus, so plain flags are what we want.
        .flags(gio::ApplicationFlags::empty())
        .build();

    // Jobs go to the worker; verbs and results come back over async channels,
    // which are the only thing the GTK main loop is able to await.
    let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
    let (event_tx, event_rx) = async_channel::unbounded::<Event>();
    let (trigger_tx, trigger_rx) = async_channel::unbounded::<Verb>();

    spawn_worker(langs, job_rx, event_tx.clone());
    spawn_dbus(trigger_tx, event_tx)?;

    app.connect_activate(move |app| {
        // Keeps the application alive with no window on screen, which is what
        // makes this a daemon rather than an app that quits when you close it.
        let hold = app.hold();

        let window = Rc::new(build_window(app, job_tx.clone()));
        window.rebuild_chips();

        {
            let window = window.clone();
            let events = event_rx.clone();

            glib::spawn_future_local(async move {
                while let Ok(event) = events.recv().await {
                    match event {
                        Event::Finished(outcome) => window.show_outcome(&outcome),
                        Event::Failed(error) => window.show_error(&error),
                    }
                }
            });
        }

        {
            let window = window.clone();
            let triggers = trigger_rx.clone();

            glib::spawn_future_local(async move {
                let _hold = hold;

                while let Ok(verb) = triggers.recv().await {
                    window.dispatch(verb);
                }
            });
        }
    });

    // GTK would otherwise try to parse our own CLI arguments as its own.
    let empty: [&str; 0] = [];
    app.run_with_args(&empty);

    Ok(())
}

fn spawn_worker(
    langs: String,
    jobs: std::sync::mpsc::Receiver<Job>,
    events: async_channel::Sender<Event>,
) {
    std::thread::spawn(move || {
        let mut worker = Worker::new(langs);

        while let Ok(job) = jobs.recv() {
            let event = match worker.run(&job) {
                Ok(Some(Product::Text(outcome))) => Event::Finished(outcome),
                // The selection overlay is not ported to GTK yet.
                Ok(Some(Product::Shot(_))) | Ok(None) => continue,
                Err(error) => Event::Failed(format!("{error:#}")),
            };

            if events.send_blocking(event).is_err() {
                break; // UI is gone
            }
        }
    });
}

/// Serve the D-Bus verbs from a tokio thread of their own.
fn spawn_dbus(
    triggers: async_channel::Sender<Verb>,
    events: async_channel::Sender<Event>,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start the D-Bus runtime")?;

    std::thread::spawn(move || {
        runtime.block_on(async move {
            let (verb_tx, mut verb_rx) = tokio::sync::mpsc::unbounded_channel::<Verb>();

            let built = async {
                zbus::connection::Builder::session()?
                    .name(ipc::SERVICE)?
                    .serve_at(ipc::PATH, ipc::Iface { triggers: verb_tx })?
                    .build()
                    .await
            }
            .await;

            // Held for the lifetime of this task: dropping it releases the bus
            // name and the daemon silently stops answering.
            let _connection = match built {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = events
                        .send(Event::Failed(format!(
                            "could not claim {}: {error}",
                            ipc::SERVICE
                        )))
                        .await;
                    return;
                }
            };

            while let Some(verb) = verb_rx.recv().await {
                if triggers.send(verb).await.is_err() {
                    break;
                }
            }
        });
    });

    Ok(())
}

fn build_window(app: &gtk::Application, jobs: std::sync::mpsc::Sender<Job>) -> Window {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("wl-translate")
        .default_width(820)
        .default_height(440)
        .build();

    let source = text_pane();
    let target = text_pane();

    let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let status = gtk::Label::builder().xalign(0.0).hexpand(true).build();
    status.add_css_class("dim-label");

    let panes = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    panes.set_homogeneous(true);
    panes.set_vexpand(true);
    panes.append(&scrolled(&source));
    panes.append(&scrolled(&target));

    let copy = gtk::Button::with_label("Copy");
    let close = gtk::Button::with_label("Close");

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.append(&copy);
    footer.append(&status);
    footer.append(&close);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.append(&chips);
    root.append(&panes);
    root.append(&footer);

    window.set_child(Some(&root));

    {
        let window = window.clone();
        close.connect_clicked(move |_| window.set_visible(false));
    }

    {
        let target = target.clone();
        copy.connect_clicked(move |_| {
            let buffer = target.buffer();
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
            let _ = crate::clip::copy(text.trim());
        });
    }

    Window {
        window,
        source,
        target,
        status,
        chips,
        settings: RefCell::new(Settings::load()),
        jobs,
    }
}

/// A wrapping, editable text view.
///
/// The important part is what is NOT configured here. Pango derives paragraph
/// direction from the text itself, so a Persian paragraph lays out right to
/// left and an Italian one left to right - in the same buffer - with the caret
/// and selection behaving correctly in both. That is the whole reason for this
/// branch, and it costs zero lines.
fn text_pane() -> gtk::TextView {
    let view = gtk::TextView::new();
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_top_margin(8);
    view.set_bottom_margin(8);
    view.set_left_margin(8);
    view.set_right_margin(8);
    view
}

fn scrolled(view: &gtk::TextView) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(view)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .has_frame(true)
        .vexpand(true)
        .build()
}

impl Window {
    fn dispatch(&self, verb: Verb) {
        if matches!(verb, Verb::Show) {
            self.present();
            return;
        }

        let settings = self.settings.borrow();

        let mut job = Job::new(verb);
        job.from = settings.source.clone();
        job.to = settings.effective_target();
        job.engine = settings.engine.clone();
        job.freeze = settings.freeze;

        let _ = self.jobs.send(job);
    }

    fn present(&self) {
        self.window.set_visible(true);
        self.window.present();
    }

    fn show_outcome(&self, outcome: &Outcome) {
        self.source.buffer().set_text(&outcome.source);
        self.target.buffer().set_text(&outcome.translation);
        self.status
            .set_text(&format!("{} \u{2192} {}", outcome.from, outcome.to));

        self.present();
    }

    fn show_error(&self, error: &str) {
        self.status.set_text(error);
        self.present();
    }

    /// Language chips, in the most-recently-used order the settings keep.
    fn rebuild_chips(&self) {
        while let Some(child) = self.chips.first_child() {
            self.chips.remove(&child);
        }

        let settings = self.settings.borrow();

        for lang in Settings::chips(&settings.recent_source) {
            let chip = gtk::Button::with_label(&lang);

            if lang == settings.source {
                chip.add_css_class("suggested-action");
            }

            self.chips.append(&chip);
        }
    }
}
