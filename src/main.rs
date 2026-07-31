//! wl-translate - on-screen OCR and translation for Wayland.
//!
//! The compositor owns the keybinding, not this program. Wayland gives no app
//! global hotkeys, so every action is a verb you bind from Hyprland/KWin/niri
//! config. Switch compositors and you rewrite one line of keybinds.
//!
//! Verbs carry no languages. Which languages you work in is a setting, changed
//! in the window and remembered, not something a keybind should restate. The
//! flags below exist for scripting and are never needed day to day.

mod capture;
mod clip;
mod ipc;
mod ocr;
mod pipeline;
mod settings;
mod shot;
mod translate;
mod ui;

use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use pipeline::{Job, Outcome, Product, Verb, Worker};
use settings::Settings;

#[derive(Parser)]
#[command(name = "wl-translate", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Override the target language for this run
    #[arg(short, long, global = true)]
    to: Option<String>,

    /// Override the source language for this run
    #[arg(short, long, global = true)]
    from: Option<String>,

    /// Override the translation backend: "google" or "ai"
    #[arg(short, long, global = true)]
    engine: Option<String>,

    /// Put the result on the clipboard as well as stdout
    #[arg(short, long, global = true)]
    copy: bool,

    /// Force a desktop notification. Already automatic when stdout is not a
    /// terminal, which is what a keybind is.
    #[arg(short, long, global = true)]
    notify: bool,

    /// Do the work in this process even if the daemon is running
    #[arg(long, global = true)]
    no_daemon: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Select a screen region, read the text in it, translate it
    Ocr {
        /// Extract the text but skip translation
        #[arg(long)]
        raw: bool,

        /// Use this region instead of dragging one, as "X,Y WxH"
        #[arg(long)]
        geometry: Option<String>,
    },

    /// Translate the current mouse selection
    Selection,

    /// Translate the current clipboard contents
    Clipboard,

    /// Translate text given as arguments
    Text {
        // Deliberately NOT `trailing_var_arg`: that captures everything after
        // the first word, so `text "ciao" --to fa` silently translated the
        // string "ciao --to fa" instead of honouring the flag. `num_args`
        // still allows unquoted multi-word text while flags keep parsing
        // wherever they appear.
        #[arg(required = true, num_args = 1..)]
        words: Vec<String>,
    },

    /// Take a screenshot: region, window or screen
    Shot {
        /// region | window | screen
        #[arg(default_value = "region")]
        mode: String,

        /// Copy to the clipboard only, without writing a file
        #[arg(long)]
        no_save: bool,
    },

    /// Raise the daemon's window without changing what is in it
    Show,

    /// Run resident: keeps tesseract loaded and shows results in a window
    Daemon {
        /// Tesseract languages to keep loaded, overriding the saved setting
        #[arg(long)]
        langs: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Command::Daemon { langs } = &cli.command {
        return ui::run(langs.clone());
    }

    // Screenshots never involve the daemon: there is no OCR engine to keep warm
    // and no window to show, so forwarding would only add a hop.
    if let Command::Shot { mode, no_save } = &cli.command {
        let mode: shot::Mode = mode.parse()?;

        // With a daemon running the shot goes there, so it can be looked at
        // before it is committed anywhere. Without one there is no window to
        // review in, so it copies and saves straight away.
        if !cli.no_daemon && ipc::forward(&Verb::Shot(mode))? {
            return Ok(());
        }

        let settings = Settings::load();

        let Some(taken) = shot::take(mode, settings.freeze, !no_save)? else {
            return Ok(()); // cancelled
        };

        return report_shot(&taken);
    }

    let verb = cli.verb();

    // The D-Bus verbs carry no languages, so an invocation that overrides them
    // has to run here rather than be handed to the daemon.
    if !cli.no_daemon && cli.forwardable() && ipc::forward(&verb)? {
        return Ok(());
    }

    if matches!(verb, Verb::Show) {
        anyhow::bail!("`show` needs a running daemon");
    }

    let settings = Settings::load();

    let mut job = Job::new(verb);
    job.from = cli.from.clone().unwrap_or_else(|| settings.source.clone());
    job.to = cli
        .to
        .clone()
        .unwrap_or_else(|| settings.effective_target());
    job.engine = cli.engine.clone().unwrap_or_else(|| settings.engine.clone());
    job.freeze = settings.freeze;

    let Some(product) = Worker::new(settings.langs.clone()).run(&job)? else {
        return Ok(()); // user cancelled the region drag
    };

    // Screenshots are handled above, so anything reaching here is text.
    let Product::Text(outcome) = product else {
        return Ok(());
    };

    emit(&outcome, &cli)
}

impl Cli {
    fn verb(&self) -> Verb {
        match &self.command {
            Command::Ocr {
                raw: true,
                geometry: None,
            } => Verb::OcrRaw,
            Command::Ocr { geometry, .. } => Verb::Ocr {
                geometry: geometry.clone(),
            },
            Command::Selection => Verb::Selection,
            Command::Clipboard => Verb::Clipboard,
            Command::Text { words } => Verb::Text(words.join(" ")),
            Command::Show => Verb::Show,
            // Both are handled before this point; the arms exist only so the
            // match stays exhaustive.
            Command::Shot { .. } | Command::Daemon { .. } => Verb::Show,
        }
    }

    /// Whether the daemon can honour this invocation faithfully.
    fn forwardable(&self) -> bool {
        let explicit_region = matches!(
            &self.command,
            Command::Ocr {
                geometry: Some(_),
                ..
            }
        );

        self.to.is_none()
            && self.from.is_none()
            && self.engine.is_none()
            && !explicit_region
    }
}

/// Print the result, and optionally copy/notify. Source text goes to stderr so
/// `wl-translate ocr | some-tool` pipes only the translation.
///
/// Notifies automatically when stdout is not a terminal and the daemon did not
/// take the job. That is exactly the keybind case: without it the work happens,
/// prints into a terminal that does not exist, and the key looks broken.
fn emit(outcome: &Outcome, cli: &Cli) -> Result<()> {
    eprintln!(
        "--- source ({}) ---\n{}\n--- translation ({}) ---",
        outcome.from, outcome.source, outcome.to
    );
    println!("{}", outcome.translation);

    if cli.copy {
        clip::copy(&outcome.translation)?;
    }
    if cli.notify || !std::io::stdout().is_terminal() {
        notify(outcome)?;
    }
    Ok(())
}

/// Screenshots report where they went. The saved file doubles as the icon, so
/// the notification carries a thumbnail of what was just captured.
fn report_shot(taken: &shot::Shot) -> Result<()> {
    let body = match &taken.saved {
        Some(path) => path.display().to_string(),
        None => format!("{} KB on the clipboard", taken.png.len() / 1024),
    };

    println!("{body}");

    let icon = taken
        .saved
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "image-x-generic".to_string());

    std::process::Command::new("notify-send")
        .args(["--app-name", "wl-translate", "--icon", &icon])
        .arg("Screenshot")
        .arg(&body)
        .status()
        .context("could not run `notify-send`")?;

    Ok(())
}

/// Summary line for the notification title: the source text, collapsed to one
/// line and clipped, so a paragraph does not produce a wall of a notification.
fn summarize(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(max) {
        Some((byte_idx, _)) => format!("{}...", &one_line[..byte_idx].trim_end()),
        None => one_line,
    }
}

fn notify(outcome: &Outcome) -> Result<()> {
    std::process::Command::new("notify-send")
        .args([
            "--app-name",
            "wl-translate",
            "--icon",
            "accessories-dictionary",
        ])
        .arg(summarize(&outcome.source, 60))
        .arg(&outcome.translation)
        .status()
        .context("could not run `notify-send`")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::summarize;

    #[test]
    fn collapses_whitespace_and_clips() {
        assert_eq!(summarize("one   two\nthree", 40), "one two three");
        assert_eq!(summarize("abcdefghij", 4), "abcd...");
    }

    #[test]
    fn clips_on_char_boundaries_not_bytes() {
        // Persian is multi-byte; slicing by byte index here would panic.
        assert_eq!(summarize("سلام حال شما چطور است", 4), "سلام...");
    }
}
