//! The work itself: get text from somewhere, then translate it.
//!
//! Deliberately synchronous and single-threaded. The daemon runs one of these
//! on a dedicated thread that owns the OCR engine for its whole life, which is
//! both why tesseract stays resident (saving ~300ms per capture) and why we
//! never need `LepTess` to be `Send`.

use anyhow::Result;

use crate::shot;
use crate::{capture, ocr, translate};

/// Where the text comes from.
#[derive(Debug, Clone)]
pub enum Verb {
    Ocr {
        /// Skip the interactive drag and use this region.
        geometry: Option<String>,
    },
    /// OCR a region but do not translate it.
    OcrRaw,
    Selection,
    Clipboard,
    Text(String),
    /// Take a screenshot. The bytes come back for review rather than being
    /// written straight out, so the keypress decides what happens to them.
    Shot(shot::Mode),
    /// Read an image we already hold, rather than capturing a new one. This is
    /// what the overlay's "extract text" and "translate text" run: the pixels
    /// are already selected, so re-dragging a region would be absurd.
    OcrImage {
        png: Vec<u8>,
        /// Extract only, no translation.
        raw: bool,
    },
    /// Raise the window without changing its contents. Handled by the UI, never
    /// reaches the worker.
    Show,
}

impl Verb {
    pub fn is_raw(&self) -> bool {
        matches!(self, Verb::OcrRaw | Verb::OcrImage { raw: true, .. })
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub verb: Verb,
    pub from: String,
    pub to: String,
    pub engine: String,
    /// Hold the screen still while a region is dragged.
    pub freeze: bool,
}

impl Job {
    pub fn new(verb: Verb) -> Self {
        Self {
            verb,
            from: "auto".into(),
            to: "en".into(),
            engine: "google".into(),
            freeze: true,
        }
    }
}

/// What a job produced. Screenshots are not text, so they cannot ride in an
/// `Outcome` - the UI has to be able to tell the two apart.
#[derive(Debug, Clone)]
pub enum Product {
    Text(Outcome),
    /// A frozen output to pick a region out of, deliberately not yet cropped,
    /// saved or copied.
    Shot(shot::Capture),
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub source: String,
    pub translation: String,
    /// What the engine decided the source language was.
    pub from: String,
    pub to: String,
}

/// Owns the OCR engine across jobs.
pub struct Worker {
    langs: String,
    tessdata: Option<String>,
    /// Built on first use and then kept. Loading the language models is the
    /// single most expensive step in the pipeline, so it happens once.
    ocr: Option<ocr::Ocr>,
}

impl Worker {
    pub fn new(langs: impl Into<String>) -> Self {
        Self {
            langs: langs.into(),
            tessdata: crate::settings::Settings::load().tessdata,
            ocr: None,
        }
    }

    /// Run one job. `Ok(None)` means there was nothing to do - the user
    /// cancelled the region drag, or the verb was not ours to handle.
    pub fn run(&mut self, job: &Job) -> Result<Option<Product>> {
        // Screenshots short-circuit the whole text pipeline.
        if let Verb::Shot(mode) = &job.verb {
            return Ok(Some(Product::Shot(shot::capture_for_overlay(*mode)?)));
        }

        let Some(source) = self.read_source(job)? else {
            return Ok(None);
        };

        anyhow::ensure!(!source.is_empty(), "no text found");

        if job.verb.is_raw() {
            return Ok(Some(Product::Text(Outcome {
                translation: source.clone(),
                source,
                from: job.from.clone(),
                to: job.to.clone(),
            })));
        }

        let result = translate::backend(&job.engine)?.translate(&source, &job.from, &job.to)?;

        Ok(Some(Product::Text(Outcome {
            source,
            translation: result.text,
            from: result.detected.unwrap_or_else(|| job.from.clone()),
            to: job.to.clone(),
        })))
    }

    fn read_source(&mut self, job: &Job) -> Result<Option<String>> {
        Ok(match &job.verb {
            Verb::Show | Verb::Shot(_) => None,
            Verb::Text(text) => Some(text.clone()),
            Verb::Selection => Some(crate::clip::primary()?),
            Verb::Clipboard => Some(crate::clip::clipboard()?),
            Verb::OcrImage { png, .. } => Some(self.engine()?.recognize(png)?),
            Verb::OcrRaw => self.capture_text(None, job.freeze)?,
            Verb::Ocr { geometry } => self.capture_text(geometry.as_deref(), job.freeze)?,
        })
    }

    fn capture_text(&mut self, geometry: Option<&str>, freeze: bool) -> Result<Option<String>> {
        // A geometry given up front needs no drag, so nothing to hold still.
        let image = match geometry {
            Some(spec) => capture::grab(&capture::Region::parse(spec)?)?,
            None => match capture::interactive(freeze)? {
                Some(image) => image,
                None => return Ok(None),
            },
        };

        Ok(Some(self.engine()?.recognize(&image)?))
    }

    /// Drop the OCR engine.
    ///
    /// tesseract memory-maps its language models, and the better ones are not
    /// small: with eng, ita and fas from tessdata_best resident the daemon sits
    /// on well over a hundred megabytes for as long as it runs, almost all of
    /// it mapped model data it is not using. Letting it go when idle costs one
    /// reload - a few hundred milliseconds on the next capture - and gives back
    /// the memory for the hours in between.
    pub fn rest(&mut self) {
        self.ocr = None;
    }

    fn engine(&mut self) -> Result<&mut ocr::Ocr> {
        if self.ocr.is_none() {
            self.ocr = Some(ocr::Ocr::new(self.tessdata.as_deref(), &self.langs)?);
        }
        Ok(self.ocr.as_mut().expect("just constructed"))
    }
}
