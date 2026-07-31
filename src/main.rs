//! wl-translate - on-screen OCR and translation for Wayland.
//!
//! The whole point of the CLI shape is that the compositor owns the keybinding,
//! not this program. Wayland gives no app global hotkeys, so every action is a
//! verb you bind from Hyprland/KWin/niri config. Switch compositors and you
//! rewrite one line of keybinds, nothing here.
//!
//! Each verb runs standalone, but if `wl-translate daemon` is running it is
//! handed the job instead - same command, resident tesseract, and the result
//! arrives in a window rather than a notification.

mod capture;
mod clip;
mod ipc;
mod ocr;
mod pipeline;
mod translate;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use pipeline::{Job, Outcome, Verb, Worker};

#[derive(Parser)]
#[command(name = "wl-translate", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Target language (ISO code, e.g. en, it, fa)
    #[arg(short, long, global = true, default_value = "en")]
    to: String,

    /// Source language, or "auto" to detect
    #[arg(short, long, global = true, default_value = "auto")]
    from: String,

    /// Translation backend: "google" (keyless) or "ai" (OpenAI-compatible)
    #[arg(short, long, global = true, default_value = "google")]
    engine: String,

    /// Put the result on the clipboard as well as stdout
    #[arg(short, long, global = true)]
    copy: bool,

    /// Show the result as a desktop notification. Bound to a key there is no
    /// terminal to print to, so this is what makes the keybinds usable.
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
        /// Tesseract languages to load, "+"-joined
        #[arg(long, default_value = "eng+ita+fas")]
        langs: String,

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
        #[arg(required = true, trailing_var_arg = true)]
        words: Vec<String>,
    },

    /// Run resident: keeps tesseract loaded and shows results in a window
    Daemon {
        /// Tesseract languages to keep loaded
        #[arg(long, default_value = "eng+ita+fas")]
        langs: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Command::Daemon { langs } = &cli.command {
        return ui::run(langs.clone());
    }

    let (verb, langs, raw) = cli.job_parts();

    // The daemon's D-Bus methods only carry a target language, so anything that
    // needs the other flags honoured has to run here instead of being forwarded.
    if !cli.no_daemon && cli.forwardable() && ipc::forward(&verb, &cli.to, raw)? {
        return Ok(());
    }

    let mut job = Job::new(verb);
    job.from = cli.from.clone();
    job.to = cli.to.clone();
    job.engine = cli.engine.clone();
    job.raw = raw;

    let Some(outcome) = Worker::new(langs).run(&job)? else {
        return Ok(()); // user cancelled the region drag
    };

    emit(&outcome, &cli)
}

impl Cli {
    /// Split the subcommand into the parts the pipeline needs.
    fn job_parts(&self) -> (Verb, String, bool) {
        const DEFAULT_LANGS: &str = "eng+ita+fas";

        match &self.command {
            Command::Ocr {
                langs,
                raw,
                geometry,
            } => (
                Verb::Ocr {
                    geometry: geometry.clone(),
                },
                langs.clone(),
                *raw,
            ),
            Command::Selection => (Verb::Selection, DEFAULT_LANGS.into(), false),
            Command::Clipboard => (Verb::Clipboard, DEFAULT_LANGS.into(), false),
            Command::Text { words } => (Verb::Text(words.join(" ")), DEFAULT_LANGS.into(), false),
            Command::Daemon { langs } => (Verb::Selection, langs.clone(), false),
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

        self.from == "auto" && self.engine == "google" && !explicit_region
    }
}

/// Print the result, and optionally copy/notify. Source text goes to stderr so
/// `wl-translate ocr | some-tool` pipes only the translation.
fn emit(outcome: &Outcome, cli: &Cli) -> Result<()> {
    if !cli.no_daemon || !outcome.source.is_empty() {
        eprintln!(
            "--- source ({}) ---\n{}\n--- translation ({}) ---",
            outcome.from, outcome.source, outcome.to
        );
    }
    println!("{}", outcome.translation);

    if cli.copy {
        clip::copy(&outcome.translation)?;
    }
    if cli.notify {
        notify(outcome)?;
    }
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
