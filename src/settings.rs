//! Persisted settings.
//!
//! The language pair lives here rather than on the command line. A keybind
//! should say *what to do* - translate the selection - not re-state which
//! languages you happen to be working in today. The UI owns that, and it
//! survives restarts.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How many languages to keep on each side's chip row.
const RECENT_LIMIT: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Source language, or "auto" to let the engine decide.
    pub source: String,
    pub target: String,
    /// Most-recently-used first. Drives the order of the chips.
    pub recent_source: Vec<String>,
    pub recent_target: Vec<String>,
    pub engine: String,
    /// Tesseract languages, "+"-joined.
    pub langs: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source: "auto".into(),
            target: "en".into(),
            recent_source: vec!["it".into(), "en".into(), "fa".into()],
            recent_target: vec!["en".into(), "fa".into(), "it".into()],
            engine: "google".into(),
            langs: "eng+ita+fas".into(),
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));

        base.join("wl-translate").join("config.json")
    }

    /// Missing or unreadable config is not an error - it just means defaults.
    /// Settings should never be the reason a keybind stops working.
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }

        let body = serde_json::to_string_pretty(self).context("could not serialise settings")?;

        std::fs::write(&path, body).with_context(|| format!("could not write {}", path.display()))
    }

    /// Record a language as used on the source side and move it to the front,
    /// which is what makes the chip row reorder itself the way Crow's does.
    pub fn use_source(&mut self, lang: &str) {
        self.source = lang.to_string();
        promote(&mut self.recent_source, lang);
    }

    pub fn use_target(&mut self, lang: &str) {
        self.target = lang.to_string();
        promote(&mut self.recent_target, lang);
    }

    pub fn swap(&mut self) {
        std::mem::swap(&mut self.source, &mut self.target);

        let (source, target) = (self.source.clone(), self.target.clone());
        promote(&mut self.recent_source, &source);
        promote(&mut self.recent_target, &target);
    }

    /// Chips for one side: "auto" first, then most-recent-first.
    pub fn chips(recent: &[String]) -> Vec<String> {
        let mut chips: Vec<String> = vec!["auto".into()];

        for lang in recent.iter().take(RECENT_LIMIT) {
            if lang != "auto" {
                chips.push(lang.clone());
            }
        }

        chips
    }

    /// The target language actually sent to the engine.
    ///
    /// A source of "auto" means "detect it", which engines understand. A target
    /// of "auto" cannot mean that - there is nothing to detect - so it means
    /// "my system language", which is what Crow does with the same button.
    pub fn effective_target(&self) -> String {
        if self.target == "auto" {
            system_language()
        } else {
            self.target.clone()
        }
    }
}

/// System language from the locale, e.g. `it_IT.UTF-8` becomes `it`.
fn system_language() -> String {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        let Some(value) = std::env::var_os(key) else {
            continue;
        };

        let value = value.to_string_lossy().to_string();
        let code = value
            .split(['.', '@'])
            .next()
            .unwrap_or_default()
            .split('_')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();

        // "C" and "POSIX" are not languages.
        if code.len() == 2 {
            return code;
        }
    }

    "en".into()
}

fn promote(recent: &mut Vec<String>, lang: &str) {
    if lang == "auto" {
        return;
    }

    recent.retain(|existing| existing != lang);
    recent.insert(0, lang.to_string());
    recent.truncate(RECENT_LIMIT);
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn using_a_language_moves_it_to_the_front() {
        let mut settings = Settings::default();
        settings.use_target("fa");

        assert_eq!(settings.target, "fa");
        assert_eq!(settings.recent_target.first().unwrap(), "fa");
    }

    #[test]
    fn recent_list_does_not_grow_unbounded_or_duplicate() {
        let mut settings = Settings::default();

        for lang in ["de", "es", "fr", "ja", "zh", "de", "de"] {
            settings.use_source(lang);
        }

        assert!(settings.recent_source.len() <= 5);
        assert_eq!(settings.recent_source.iter().filter(|l| *l == "de").count(), 1);
    }

    #[test]
    fn swapping_exchanges_both_sides() {
        let mut settings = Settings::default();
        settings.source = "it".into();
        settings.target = "fa".into();

        settings.swap();

        assert_eq!(settings.source, "fa");
        assert_eq!(settings.target, "it");
    }

    #[test]
    fn auto_leads_both_chip_rows() {
        let settings = Settings::default();

        assert_eq!(
            Settings::chips(&settings.recent_source).first().unwrap(),
            "auto"
        );
        assert_eq!(
            Settings::chips(&settings.recent_target).first().unwrap(),
            "auto"
        );
    }

    #[test]
    fn auto_target_resolves_to_a_real_language() {
        let mut settings = Settings::default();
        settings.target = "auto".into();

        let resolved = settings.effective_target();

        assert_ne!(resolved, "auto");
        assert_eq!(resolved.len(), 2);
    }
}
