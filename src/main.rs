//! wl-translate - on-screen OCR and translation for Wayland.
//!
//! The whole point of the CLI shape is that the compositor owns the keybinding,
//! not this program. Wayland gives no app global hotkeys, so every action is a
//! verb you bind from Hyprland/KWin/niri config. Switch compositors and you
//! rewrite one line of keybinds, nothing here.

mod capture;
mod clip;
mod ocr;
mod translate;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let (source, translated) = match &cli.command {
        Command::Ocr { langs, raw } => {
            let Some(region) = capture::select_region()? else {
                return Ok(()); // user cancelled the drag; not an error
            };

            let image = capture::grab(&region)?;
            let text = ocr::Ocr::new(langs)?.recognize(&image)?;

            anyhow::ensure!(!text.is_empty(), "no text found in that region");

            if *raw {
                emit(&text, None, cli.copy)?;
                return Ok(());
            }
            let out = translate(&cli, &text)?;
            (text, out)
        }

        Command::Selection => {
            let text = clip::primary()?;
            anyhow::ensure!(!text.is_empty(), "nothing is selected");
            let out = translate(&cli, &text)?;
            (text, out)
        }

        Command::Clipboard => {
            let text = clip::clipboard()?;
            anyhow::ensure!(!text.is_empty(), "clipboard is empty");
            let out = translate(&cli, &text)?;
            (text, out)
        }

        Command::Text { words } => {
            let text = words.join(" ");
            let out = translate(&cli, &text)?;
            (text, out)
        }
    };

    emit(&translated, Some(&source), cli.copy)
}

fn translate(cli: &Cli, text: &str) -> Result<String> {
    let backend = translate::backend(&cli.engine)?;
    let result = backend
        .translate(text, &cli.from, &cli.to)
        .context("translation failed")?;

    if let Some(lang) = result.detected {
        if cli.from == "auto" {
            eprintln!("detected: {lang}");
        }
    }
    Ok(result.text)
}

/// Print the result, and optionally put it on the clipboard. Source text goes to
/// stderr so `wl-translate ocr | some-tool` pipes only the translation.
fn emit(result: &str, source: Option<&str>, copy: bool) -> Result<()> {
    if let Some(src) = source {
        eprintln!("--- source ---\n{src}\n--- translation ---");
    }
    println!("{result}");

    if copy {
        clip::copy(result)?;
    }
    Ok(())
}
