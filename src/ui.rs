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

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

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
    /// A frozen output to pick a region out of.
    Captured(crate::shot::Capture),
}

/// How long to wait after the last keystroke before re-translating.
const SETTLE: Duration = Duration::from_millis(350);

/// How many past translations to keep. Small on purpose: this is for stepping
/// back a few, not for archiving.
const HISTORY: usize = 30;

/// Languages the picker offers, beyond whichever ones are already on the chip
/// row. Not exhaustive - the engines accept far more - but it covers what a
/// person actually reaches for, and the chips remember anything you use.
const LANGUAGES: &[(&str, &str)] = &[
    ("auto", "Detect automatically"),
    ("ar", "Arabic"),
    ("zh", "Chinese"),
    ("nl", "Dutch"),
    ("en", "English"),
    ("fr", "French"),
    ("de", "German"),
    ("el", "Greek"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("ku", "Kurdish"),
    ("fa", "Persian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("ru", "Russian"),
    ("es", "Spanish"),
    ("sv", "Swedish"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
];

/// Widgets the update path needs to reach back into.
struct Window {
    window: gtk::ApplicationWindow,
    source: gtk::TextView,
    target: gtk::TextView,
    status: gtk::Label,
    source_chips: gtk::Box,
    target_chips: gtk::Box,
    swap_button: gtk::Button,
    settings: RefCell<Settings>,
    jobs: std::sync::mpsc::Sender<Job>,
    /// Bumped on every edit so a stale debounce tick can be ignored.
    generation: Cell<u64>,
    /// Set while the code fills the buffers, because `connect_changed` cannot
    /// tell our writes from the user's - without this, showing a translation
    /// would look like an edit and translate itself again, forever.
    quiet: Cell<bool>,
    /// Past translations, newest first.
    history: RefCell<Vec<Outcome>>,
    history_button: gtk::MenuButton,
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
        wire(&window);
        rebuild_chips(&window);

        {
            let window = window.clone();
            let events = event_rx.clone();
            let app = app.clone();

            glib::spawn_future_local(async move {
                while let Ok(event) = events.recv().await {
                    match event {
                        Event::Finished(outcome) => window.show_outcome(&outcome),
                        Event::Failed(error) => window.show_error(&error),

                        Event::Captured(capture) => {
                            let window = window.clone();

                            crate::overlay::present(&app, capture, move |done| {
                                match done {
                                    // Extract and Translate hand the cropped
                                    // pixels straight back to the OCR worker;
                                    // re-dragging a region would be absurd when
                                    // one has just been selected.
                                    crate::overlay::Done::Recognise { png, raw } => {
                                        window.dispatch(Verb::OcrImage { png, raw });
                                    }
                                    crate::overlay::Done::Handled(status) => {
                                        window.note(&status);
                                    }
                                    crate::overlay::Done::Cancelled => {}
                                }
                            });
                        }
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
        use std::sync::mpsc::RecvTimeoutError;

        // Long enough that a working session never pays for a reload, short
        // enough that a daemon left running overnight is not holding the
        // language models the whole time.
        const IDLE: Duration = Duration::from_secs(180);

        let mut worker = Worker::new(langs);

        let job = loop {
            match jobs.recv_timeout(IDLE) {
                Ok(job) => break job,
                Err(RecvTimeoutError::Timeout) => {
                    worker.rest();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };

        let mut job = job;

        loop {
            let event = match worker.run(&job) {
                Ok(Some(Product::Text(outcome))) => Event::Finished(outcome),
                Ok(Some(Product::Shot(capture))) => Event::Captured(capture),
                Ok(None) => continue,
                Err(error) => Event::Failed(format!("{error:#}")),
            };

            if events.send_blocking(event).is_err() {
                return; // UI is gone
            }

            job = loop {
                match jobs.recv_timeout(IDLE) {
                    Ok(job) => break job,
                    Err(RecvTimeoutError::Timeout) => {
                        worker.rest();
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
        }
    });
}

/// Serve the D-Bus verbs from a tokio thread of their own.
fn spawn_dbus(
    triggers: async_channel::Sender<Verb>,
    events: async_channel::Sender<Event>,
) -> Result<()> {
    // Single-threaded on purpose. This runtime does nothing but answer D-Bus
    // calls and forward them; a multi-threaded one spawned a worker per core
    // and their stacks showed up as two dozen threads for no benefit at all.
    let runtime = tokio::runtime::Builder::new_current_thread()
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

    let source_chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let target_chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    source_chips.set_hexpand(true);
    target_chips.set_halign(gtk::Align::End);
    target_chips.set_hexpand(true);

    let swap = gtk::Button::from_icon_name("object-flip-horizontal-symbolic");
    swap.set_tooltip_text(Some("Swap languages"));
    swap.add_css_class("flat");
    swap.add_css_class("circular");

    let source_more = gtk::MenuButton::new();
    source_more.set_icon_name("view-more-symbolic");
    source_more.set_tooltip_text(Some("All languages"));
    source_more.add_css_class("flat");

    let target_more = gtk::MenuButton::new();
    target_more.set_icon_name("view-more-symbolic");
    target_more.set_tooltip_text(Some("All languages"));
    target_more.add_css_class("flat");

    let history_button = gtk::MenuButton::new();
    history_button.set_icon_name("document-open-recent-symbolic");
    history_button.set_tooltip_text(Some("Recent translations"));
    history_button.add_css_class("flat");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.append(&source_chips);
    header.append(&source_more);
    header.append(&swap);
    header.append(&target_more);
    header.append(&target_chips);
    header.append(&history_button);

    // Parked on the header so `wire` can find them once the Window exists.
    unsafe {
        header.set_data("source_more", source_more);
        header.set_data("target_more", target_more);
    }

    let status = gtk::Label::builder().xalign(0.0).hexpand(true).build();
    status.add_css_class("dim-label");

    let panes = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    panes.set_homogeneous(true);
    panes.set_vexpand(true);
    panes.append(&scrolled(&source));
    panes.append(&scrolled(&target));

    // Icons and GTK's own style classes throughout, so the window looks like
    // whatever the system theme says a GTK window looks like. Nothing here sets
    // a colour or a font of its own.
    let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy.set_tooltip_text(Some("Copy the translation"));

    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.set_tooltip_text(Some("Close"));

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.append(&copy);
    footer.append(&status);
    footer.append(&close);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.append(&header);
    root.append(&panes);
    root.append(&footer);

    window.set_child(Some(&root));

    {
        let window = window.clone();
        close.connect_clicked(move |_| window.set_visible(false));
    }

    // Esc closes it. The window is opened by a keybind and glanced at, so
    // reaching for the mouse to dismiss it breaks the flow it exists to serve.
    {
        let keys = gtk::EventControllerKey::new();
        // Cloned for the closure; the original still has to receive the
        // controller afterwards.
        let target = window.clone();

        keys.connect_key_pressed(move |_controller, key, _code, _modifiers| {
            if key == gtk::gdk::Key::Escape {
                target.set_visible(false);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });

        window.add_controller(keys);
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
        source_chips,
        target_chips,
        swap_button: swap,
        settings: RefCell::new(Settings::load()),
        jobs,
        generation: Cell::new(0),
        quiet: Cell::new(false),
        history: RefCell::new(Vec::new()),
        history_button,
    }
}

/// A popover listing every language, so one you have not used recently is still
/// reachable. The chips only ever show recent choices, which left anything else
/// requiring a hand-edit of the config file.
fn language_picker(window: &Rc<Window>, is_source: bool) -> gtk::Popover {
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search languages"));

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);

    for (code, name) in LANGUAGES {
        let row = gtk::ListBoxRow::new();
        let label = gtk::Label::builder()
            .label(format!("{name}   ({code})"))
            .xalign(0.0)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(10)
            .margin_end(10)
            .build();

        row.set_child(Some(&label));
        // Kept for the filter, so searching matches the name as well as the code.
        unsafe { row.set_data("search", format!("{name} {code}").to_lowercase()) };
        list.append(&row);
    }

    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .min_content_height(320)
        .min_content_width(240)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(&search);
    content.append(&scroller);

    let popover = gtk::Popover::new();
    popover.set_child(Some(&content));

    {
        let list = list.clone();
        search.connect_search_changed(move |entry| {
            let needle = entry.text().to_lowercase();
            let mut row = list.first_child();

            while let Some(child) = row {
                if let Ok(row) = child.clone().downcast::<gtk::ListBoxRow>() {
                    let haystack = unsafe { row.data::<String>("search") };
                    let shown = match haystack {
                        Some(text) => unsafe { text.as_ref() }.contains(&needle),
                        None => true,
                    };
                    row.set_visible(shown);
                }
                row = child.next_sibling();
            }
        });
    }

    {
        let window = window.clone();
        let popover = popover.clone();

        list.connect_row_activated(move |_list, row| {
            let index = row.index();

            if let Some((code, _)) = LANGUAGES.get(index as usize) {
                window.pick_language(code, is_source);
                rebuild_chips(&window);
            }

            popover.popdown();
        });
    }

    list.set_activate_on_single_click(true);
    popover
}

/// A popover of past translations. Choosing one puts it back in the panes.
fn history_popover(window: &Rc<Window>) -> gtk::Popover {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.set_activate_on_single_click(true);

    for entry in window.history.borrow().iter() {
        let row = gtk::ListBoxRow::new();

        let summary = gtk::Label::builder()
            .label(summarise(&entry.source))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(10)
            .margin_end(10)
            .build();

        row.set_child(Some(&summary));
        list.append(&row);
    }

    if window.history.borrow().is_empty() {
        let empty = gtk::Label::builder()
            .label("Nothing translated yet")
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        empty.add_css_class("dim-label");
        list.append(&empty);
    }

    let popover = gtk::Popover::new();
    popover.set_child(Some(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .min_content_height(260)
            .min_content_width(320)
            .build(),
    ));

    {
        let window = window.clone();
        let popover = popover.clone();

        list.connect_row_activated(move |_list, row| {
            let entry = window.history.borrow().get(row.index() as usize).cloned();

            if let Some(entry) = entry {
                window.show_outcome(&entry);
            }

            popover.popdown();
        });
    }

    popover
}

/// One line of a translation, for a history row.
fn summarise(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");

    match line.char_indices().nth(60) {
        Some((byte, _)) => format!("{}...", &line[..byte].trim_end()),
        None => line,
    }
}

/// Signals that need the finished `Window`, so they cannot be connected while
/// it is still being built. Everything captures a weak reference: a strong one
/// would have the window own a closure that owns the window.
fn wire(window: &Rc<Window>) {
    let buffer = window.source.buffer();
    let weak = Rc::downgrade(window);

    buffer.connect_changed(move |_| {
        let Some(window) = weak.upgrade() else {
            return;
        };

        // Our own writes are not edits.
        if window.quiet.get() {
            return;
        }

        let generation = window.generation.get() + 1;
        window.generation.set(generation);

        let weak = Rc::downgrade(&window);

        glib::timeout_add_local_once(SETTLE, move || {
            let Some(window) = weak.upgrade() else {
                return;
            };

            // Only the newest tick survives; the rest were superseded by
            // further typing.
            if window.generation.get() == generation {
                window.retranslate();
            }
        });
    });

    if let Some(header) = window.source_chips.parent() {
        for (key, is_source) in [("source_more", true), ("target_more", false)] {
            if let Some(button) = unsafe { header.data::<gtk::MenuButton>(key) } {
                let button = unsafe { button.as_ref() };
                button.set_popover(Some(&language_picker(window, is_source)));
            }
        }
    }

    {
        // Rebuilt on each open so it reflects what has happened since.
        let weak = Rc::downgrade(window);

        // set_create_popup_func runs just before the popover is shown, which is
        // exactly when the list should be built - the history has usually grown
        // since the last time it was opened.
        window.history_button.set_create_popup_func(move |button| {
            if let Some(window) = weak.upgrade() {
                button.set_popover(Some(&history_popover(&window)));
            }
        });
    }

    let weak = Rc::downgrade(window);

    window.swap_button.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.swap();
            rebuild_chips(&window);
        }
    });
}

/// Language chips for both sides, rebuilt so the most-recently-used order stays
/// visible and the active language stays highlighted.
fn rebuild_chips(window: &Rc<Window>) {
    for strip in [&window.source_chips, &window.target_chips] {
        while let Some(child) = strip.first_child() {
            strip.remove(&child);
        }
    }

    let (source, target, recent_source, recent_target) = {
        let settings = window.settings.borrow();
        (
            settings.source.clone(),
            settings.target.clone(),
            settings.recent_source.clone(),
            settings.recent_target.clone(),
        )
    };

    for (strip, recent, active, is_source) in [
        (&window.source_chips, recent_source, source, true),
        (&window.target_chips, recent_target, target, false),
    ] {
        for lang in Settings::chips(&recent) {
            let chip = gtk::Button::with_label(&lang);
            chip.add_css_class("flat");

            if lang == active {
                chip.add_css_class("suggested-action");
            }

            let weak = Rc::downgrade(window);
            let lang = lang.clone();

            chip.connect_clicked(move |_| {
                let Some(window) = weak.upgrade() else {
                    return;
                };

                window.pick_language(&lang, is_source);
                rebuild_chips(&window);
            });

            strip.append(&chip);
        }
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

    fn remember(&self, outcome: &Outcome) {
        if outcome.source.trim().is_empty() {
            return;
        }

        let mut history = self.history.borrow_mut();

        // Re-translating the same text should move it up, not stack duplicates.
        history.retain(|past| past.source != outcome.source);
        history.insert(0, outcome.clone());
        history.truncate(HISTORY);
    }

    fn show_outcome(&self, outcome: &Outcome) {
        self.remember(outcome);
        self.quiet.set(true);
        self.source.buffer().set_text(&outcome.source);
        self.target.buffer().set_text(&outcome.translation);
        self.quiet.set(false);

        // Say when the source was guessed rather than chosen. A wrong guess
        // otherwise only shows up as a translation that makes no sense, with
        // nothing on screen hinting at why.
        let detected = self.settings.borrow().source == "auto";

        self.status.set_text(&if detected {
            format!(
                "detected {} \u{2192} {}   \u{00b7}   click a language to correct it",
                outcome.from, outcome.to
            )
        } else {
            format!("{} \u{2192} {}", outcome.from, outcome.to)
        });

        self.present();
    }

    fn source_text(&self) -> String {
        let buffer = self.source.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .trim()
            .to_string()
    }

    /// Translate whatever is in the source pane, with the current languages.
    fn retranslate(&self) {
        let text = self.source_text();

        if !text.is_empty() {
            self.dispatch(Verb::Text(text));
        }
    }

    fn pick_language(&self, lang: &str, is_source: bool) {
        {
            let mut settings = self.settings.borrow_mut();

            if is_source {
                settings.use_source(lang);
            } else {
                settings.use_target(lang);
            }

            if let Err(error) = settings.save() {
                eprintln!("wl-translate: could not save settings: {error:#}");
            }
        }

        self.retranslate();
    }

    fn swap(&self) {
        {
            let mut settings = self.settings.borrow_mut();
            settings.swap();

            if let Err(error) = settings.save() {
                eprintln!("wl-translate: could not save settings: {error:#}");
            }
        }

        // Swap the panes too, so what you were reading becomes what you are
        // translating.
        let target_buffer = self.target.buffer();
        let translation = target_buffer
            .text(&target_buffer.start_iter(), &target_buffer.end_iter(), false)
            .trim()
            .to_string();
        let source = self.source_text();

        self.quiet.set(true);
        self.source.buffer().set_text(&translation);
        self.target.buffer().set_text(&source);
        self.quiet.set(false);

        self.retranslate();
    }

    /// Report something without forcing the window into view - a saved
    /// screenshot should not drag the translator up in front of you.
    fn note(&self, status: &str) {
        self.status.set_text(status);
    }

    fn show_error(&self, error: &str) {
        self.status.set_text(error);
        self.present();
    }

}
