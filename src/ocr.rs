//! Text recognition via libtesseract, in-process.
//!
//! Held as a struct rather than a free function so the daemon can construct it
//! once and keep the language models resident - re-initialising tesseract per
//! capture costs ~250ms of the budget.

use anyhow::{Context, Result};
use leptess::LepTess;

pub struct Ocr {
    engine: LepTess,
}

impl Ocr {
    /// `langs` is tesseract's `+`-joined form, e.g. `eng+ita+fas`.
    ///
    /// `datapath` overrides where the models come from, which is how a better
    /// set than the distribution's can be used without touching system files.
    pub fn new(datapath: Option<&str>, langs: &str) -> Result<Self> {
        let engine = LepTess::new(datapath, langs).with_context(|| {
            format!(
                "tesseract init failed for '{langs}' - missing language data? \
                 install with: sudo pacman -S {}",
                langs
                    .split('+')
                    .map(|l| format!("tesseract-data-{l}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        })?;
        Ok(Self { engine })
    }

    /// Recognise an encoded image already in memory (PPM from `capture::grab`).
    pub fn recognize(&mut self, image: &[u8]) -> Result<String> {
        self.engine
            .set_image_from_mem(image)
            .context("leptonica could not decode the capture")?;

        let text = self
            .engine
            .get_utf8_text()
            .context("tesseract returned no text")?;

        Ok(normalize(&text))
    }
}

/// Tesseract emits hard line breaks at the captured image's edges, which turn a
/// single wrapped sentence into several. Rejoin those, but keep blank-line
/// paragraph breaks, so the translator sees whole sentences.
fn normalize(raw: &str) -> String {
    raw.split("\n\n")
        .map(|para| {
            para.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn rejoins_wrapped_lines_but_keeps_paragraphs() {
        let raw = "Il contratto di\nlocazione deve\n\nessere registrato\n";
        assert_eq!(
            normalize(raw),
            "Il contratto di locazione deve\n\nessere registrato"
        );
    }

    #[test]
    fn drops_empty_output() {
        assert_eq!(normalize("\n \n\n"), "");
    }
}
