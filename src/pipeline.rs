//! The work itself: get text from somewhere, then translate it.
//!
//! Deliberately synchronous and single-threaded. The daemon runs one of these
//! on a dedicated thread that owns the OCR engine for its whole life, which is
//! both why tesseract stays resident (saving ~300ms per capture) and why we
//! never need `LepTess` to be `Send`.

use anyhow::Result;

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
    /// Raise the window without changing its contents. Handled by the UI, never
    /// reaches the worker.
    Show,
}

impl Verb {
    pub fn is_raw(&self) -> bool {
        matches!(self, Verb::OcrRaw)
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub verb: Verb,
    pub from: String,
    pub to: String,
    pub engine: String,
}

impl Job {
    pub fn new(verb: Verb) -> Self {
        Self {
            verb,
            from: "auto".into(),
            to: "en".into(),
            engine: "google".into(),
        }
    }
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
    /// Built on first use and then kept. Loading the language models is the
    /// single most expensive step in the pipeline, so it happens once.
    ocr: Option<ocr::Ocr>,
}

impl Worker {
    pub fn new(langs: impl Into<String>) -> Self {
        Self {
            langs: langs.into(),
            ocr: None,
        }
    }

    /// Run one job. `Ok(None)` means there was nothing to do - the user
    /// cancelled the region drag, or the verb was not ours to handle.
    pub fn run(&mut self, job: &Job) -> Result<Option<Outcome>> {
        let Some(source) = self.read_source(job)? else {
            return Ok(None);
        };

        anyhow::ensure!(!source.is_empty(), "no text found");

        if job.verb.is_raw() {
            return Ok(Some(Outcome {
                translation: source.clone(),
                source,
                from: job.from.clone(),
                to: job.to.clone(),
            }));
        }

        let result = translate::backend(&job.engine)?.translate(&source, &job.from, &job.to)?;

        Ok(Some(Outcome {
            source,
            translation: result.text,
            from: result.detected.unwrap_or_else(|| job.from.clone()),
            to: job.to.clone(),
        }))
    }

    fn read_source(&mut self, job: &Job) -> Result<Option<String>> {
        Ok(match &job.verb {
            Verb::Show => None,
            Verb::Text(text) => Some(text.clone()),
            Verb::Selection => Some(crate::clip::primary()?),
            Verb::Clipboard => Some(crate::clip::clipboard()?),
            Verb::OcrRaw => self.capture_text(None)?,
            Verb::Ocr { geometry } => self.capture_text(geometry.as_deref())?,
        })
    }

    fn capture_text(&mut self, geometry: Option<&str>) -> Result<Option<String>> {
        let region = match geometry {
            Some(spec) => capture::Region::parse(spec)?,
            None => match capture::select_region()? {
                Some(region) => region,
                None => return Ok(None),
            },
        };

        let image = capture::grab(&region)?;
        Ok(Some(self.engine()?.recognize(&image)?))
    }

    fn engine(&mut self) -> Result<&mut ocr::Ocr> {
        if self.ocr.is_none() {
            self.ocr = Some(ocr::Ocr::new(&self.langs)?);
        }
        Ok(self.ocr.as_mut().expect("just constructed"))
    }
}
