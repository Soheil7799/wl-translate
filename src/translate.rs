//! Translation backends.
//!
//! Kept behind a trait because the two useful backends have opposite
//! trade-offs: Google is keyless and ~105ms but scraped and rate-limited by IP;
//! an LLM is far better on Persian and idiom but needs a key and a round trip.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

pub struct Translation {
    pub text: String,
    /// What the backend thought the source language was, when it says.
    pub detected: Option<String>,
}

pub trait Translator {
    fn translate(&self, text: &str, from: &str, to: &str) -> Result<Translation>;
}

pub fn backend(name: &str) -> Result<Box<dyn Translator>> {
    match name {
        "google" => Ok(Box::new(Google)),
        "ai" => Ok(Box::new(OpenAiCompatible::from_env()?)),
        other => bail!("unknown engine '{other}' (expected 'google' or 'ai')"),
    }
}

/// The undocumented endpoint the Google Translate web widget calls.
/// No key, no account. This is what Crow Translate used before 4.0 replaced it
/// with Mozhi proxies. Rate-limited per IP; fine for personal use.
pub struct Google;

impl Translator for Google {
    fn translate(&self, text: &str, from: &str, to: &str) -> Result<Translation> {
        let body: Value = ureq::get("https://translate.googleapis.com/translate_a/single")
            .timeout(Duration::from_secs(15))
            .query("client", "gtx")
            .query("sl", from)
            .query("tl", to)
            .query("dt", "t")
            .query("q", text)
            .call()
            .context("google translate request failed")?
            .into_json()
            .context("google returned a body we could not parse")?;

        // Shape: [[[translated, original, ...], ...], null, "detected-lang", ...]
        let segments = body
            .get(0)
            .and_then(Value::as_array)
            .context("unexpected response shape from google")?;

        let mut out = String::new();
        for seg in segments {
            if let Some(chunk) = seg.get(0).and_then(Value::as_str) {
                out.push_str(chunk);
            }
        }

        Ok(Translation {
            text: out,
            detected: body.get(2).and_then(Value::as_str).map(str::to_owned),
        })
    }
}

/// Any OpenAI-compatible chat endpoint - Groq, OpenRouter, a local llama.cpp.
/// Configured entirely by environment so no provider is baked in.
pub struct OpenAiCompatible {
    base_url: String,
    model: String,
    api_key: String,
}

impl OpenAiCompatible {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("WLT_AI_KEY")
            .context("engine 'ai' needs WLT_AI_KEY (and optionally WLT_AI_URL / WLT_AI_MODEL)")?;

        Ok(Self {
            base_url: std::env::var("WLT_AI_URL")
                .unwrap_or_else(|_| "https://api.groq.com/openai/v1".into()),
            model: std::env::var("WLT_AI_MODEL")
                .context("engine 'ai' needs WLT_AI_MODEL, e.g. a Groq model id")?,
            api_key,
        })
    }
}

impl Translator for OpenAiCompatible {
    fn translate(&self, text: &str, from: &str, to: &str) -> Result<Translation> {
        let source = if from == "auto" {
            "the source language".to_string()
        } else {
            from.to_string()
        };

        let body: Value = ureq::post(&format!("{}/chat/completions", self.base_url))
            .timeout(Duration::from_secs(30))
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .send_json(json!({
                "model": self.model,
                "temperature": 0,
                "messages": [
                    {
                        "role": "system",
                        "content": format!(
                            "You are a translation engine. Translate from {source} to {to}. \
                             Preserve line breaks and formatting. The input may contain OCR \
                             errors - silently correct obvious ones. Reply with the translation \
                             only, with no preamble, notes, or quoting."
                        )
                    },
                    { "role": "user", "content": text }
                ]
            }))
            .context("ai translate request failed")?
            .into_json()
            .context("ai backend returned a body we could not parse")?;

        let text = body
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .context("ai backend returned no message content")?
            .trim()
            .to_owned();

        Ok(Translation {
            text,
            detected: None,
        })
    }
}
