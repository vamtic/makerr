//! UI strings with English and Hungarian dictionaries.
//!
//! Same keys and `%1`-style placeholders as the V1 dictionaries, so
//! the dictionary tables could be copied between the two apps unchanged.

use std::sync::RwLock;

use crate::settings::Language;

/// `tr(key)` with no args, or `trf(key, &[a, b, ...])` for `%1..%N`
/// substitution.
pub fn tr(lang: Language, key: &str) -> String {
    trf(lang, key, &[])
}

/// Substitutes `%1`, `%2`, … with the provided args, V1-style.
pub fn trf(lang: Language, key: &str, args: &[&str]) -> String {
    let mut out = match lookup(dict(lang), key).or_else(|| lookup(dict(Language::En), key)) {
        Some(t) => t.to_string(),
        None => {
            eprintln!("TranslationManager: missing key \"{key}\"");
            key.to_string()
        }
    };
    for (i, arg) in args.iter().enumerate() {
        out = out.replace(&format!("%{}", i + 1), arg);
    }
    out
}

type Dict = &'static [(&'static str, &'static str)];

fn dict(lang: Language) -> Dict {
    match lang {
        Language::En => ENGLISH,
        Language::Hu => HUNGARIAN,
    }
}

fn lookup(dict: Dict, key: &str) -> Option<&'static str> {
    dict.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

// The shared live dictionary used by the GUI front-end so it can switch
// languages at runtime without re-deriving tables.
static CURRENT: RwLock<Option<Language>> = RwLock::new(None);

pub fn set_language(lang: Language) {
    *CURRENT.write().unwrap() = Some(lang);
}

/// Translates against the language set by [`set_language`] (defaults to
/// English).
pub fn t(key: &str) -> String {
    tr(current_language(), key)
}

pub fn tf(key: &str, args: &[&str]) -> String {
    trf(current_language(), key, args)
}

pub fn current_language() -> Language {
    *CURRENT.read().unwrap().as_ref().unwrap_or(&Language::En)
}

// The two dictionaries live in a generated sibling file to keep this module
// readable.
include!("i18n_maps.rs");

/// Log message severity, shared by the pipeline, the GUI log view and the
/// console printer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Success,
    Warning,
    Error,
    Processing,
}

impl LogLevel {
    /// The ANSI-ish color used in the GUI log (matches the V1 palette).
    pub fn color(self) -> &'static str {
        match self {
            LogLevel::Info => "#f8fafc",
            LogLevel::Success => "#60a5fa",
            LogLevel::Warning => "#fbbf24",
            LogLevel::Error => "#f87171",
            LogLevel::Processing => "#c084fc",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_and_hungarian_share_keys() {
        // Every Hungarian entry must have an English counterpart so a
        // missing-key fallback can never trigger for a supported language.
        for (key, _) in HUNGARIAN {
            assert!(ENGLISH.iter().any(|(k, _)| k == key), "HU-only key: {key}");
        }
    }

    #[test]
    fn placeholder_substitution() {
        assert_eq!(
            trf(Language::En, "canvas_size", &["65536"]),
            "Canvas size: 65536"
        );
        assert_eq!(
            trf(Language::Hu, "stats_tile_info", &["7", "64"]),
            "7 nap  ·  64 mozaik képkockánként"
        );
    }

    #[test]
    fn multi_digit_placeholders() {
        assert_eq!(
            trf(Language::En, "preview_day", &["2000-01-02", "3", "10"]),
            "Preview day 3/10 (2000-01-02)"
        );
    }

    #[test]
    fn missing_key_falls_back_to_key_itself() {
        assert_eq!(tr(Language::En, "no_such_key_exists"), "no_such_key_exists");
    }

    #[test]
    fn runtime_language_switch() {
        set_language(Language::Hu);
        assert_eq!(t("cancel"), "Mégse");
        set_language(Language::En);
        assert_eq!(t("cancel"), "Cancel");
    }
}
