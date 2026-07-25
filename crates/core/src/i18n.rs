//! Locale state for the whole process.
//!
//! Deliberately shaped like [`crate::settings`]'s store is not: the current
//! locale is a single byte behind an atomic, because it is read on the order of
//! thousands of times per rendered frame. `get_settings()` deep-clones the
//! whole `AppSettings`, so a lookup must never go through it.
//!
//! This lives in core rather than the UI crate because error messages
//! originate here, and there must be exactly one answer to "what language is
//! this user reading?".

use std::sync::atomic::{AtomicU8, Ordering};

/// A language the interface can be presented in.
///
/// The discriminants are persisted only via [`Locale::tag`], never as numbers,
/// so they are free to change.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub enum Locale {
    /// Simplified Chinese.
    #[default]
    Zh,
    /// English.
    En,
    /// Japanese.
    Ja,
}

impl Locale {
    /// Every supported locale, in the order the UI should offer them.
    pub const ALL: [Locale; 3] = [Locale::Zh, Locale::En, Locale::Ja];

    /// The tag persisted in settings. Stable; do not change.
    pub const fn tag(self) -> &'static str {
        match self {
            Locale::Zh => "zh",
            Locale::En => "en",
            Locale::Ja => "ja",
        }
    }

    /// The language's own name, for the language picker. A picker that labels
    /// languages in the *current* language is useless to someone who cannot
    /// read it, so these are intentionally not translated.
    pub const fn endonym(self) -> &'static str {
        match self {
            Locale::Zh => "简体中文",
            Locale::En => "English",
            Locale::Ja => "日本語",
        }
    }

    /// Parse a persisted tag or a BCP-47 language tag.
    ///
    /// Only the primary subtag is considered, so `zh-Hans-CN`, `zh_CN` and
    /// `zh` all resolve alike. Unknown languages yield `None` rather than
    /// silently becoming Chinese.
    pub fn from_tag(tag: &str) -> Option<Self> {
        let primary = tag
            .split(['-', '_'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match primary.as_str() {
            "zh" => Some(Locale::Zh),
            "en" => Some(Locale::En),
            "ja" => Some(Locale::Ja),
            _ => None,
        }
    }

    const fn to_byte(self) -> u8 {
        match self {
            Locale::Zh => 0,
            Locale::En => 1,
            Locale::Ja => 2,
        }
    }

    const fn from_byte(byte: u8) -> Self {
        match byte {
            1 => Locale::En,
            2 => Locale::Ja,
            _ => Locale::Zh,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The locale every lookup resolves against. Cheap enough to call per string.
#[inline]
pub fn current() -> Locale {
    Locale::from_byte(CURRENT.load(Ordering::Relaxed))
}

/// Set the process-wide locale.
///
/// Callers in the UI must follow this with a repaint; changing this alone does
/// not invalidate anything that has already been rendered.
pub fn install(locale: Locale) {
    CURRENT.store(locale.to_byte(), Ordering::Relaxed);
}

/// The OS's preferred UI language, falling back to Chinese when it is
/// unset or is a language we do not ship.
pub fn os_locale() -> Locale {
    sys_locale::get_locale()
        .as_deref()
        .and_then(Locale::from_tag)
        .unwrap_or_default()
}

/// Resolve the persisted `language` setting to a concrete locale.
///
/// `None` means "follow the OS" — which is what the settings UI has always
/// labelled `auto`, even though nothing implemented it until now.
pub fn resolve(setting: Option<&str>) -> Locale {
    match setting {
        None => os_locale(),
        Some(tag) => Locale::from_tag(tag).unwrap_or_else(os_locale),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip() {
        for locale in Locale::ALL {
            assert_eq!(Locale::from_tag(locale.tag()), Some(locale));
        }
    }

    #[test]
    fn bcp47_tags_reduce_to_their_primary_subtag() {
        assert_eq!(Locale::from_tag("zh-Hans-CN"), Some(Locale::Zh));
        assert_eq!(Locale::from_tag("zh_CN"), Some(Locale::Zh));
        assert_eq!(Locale::from_tag("en-GB"), Some(Locale::En));
        assert_eq!(Locale::from_tag("ja-JP"), Some(Locale::Ja));
        assert_eq!(Locale::from_tag("EN"), Some(Locale::En));
    }

    #[test]
    fn unsupported_languages_are_rejected_rather_than_defaulted() {
        assert_eq!(Locale::from_tag("de"), None);
        assert_eq!(Locale::from_tag(""), None);
    }

    #[test]
    fn an_explicit_setting_wins_over_the_os() {
        assert_eq!(resolve(Some("ja")), Locale::Ja);
        assert_eq!(resolve(Some("en")), Locale::En);
    }

    #[test]
    fn byte_encoding_round_trips_and_clamps() {
        for locale in Locale::ALL {
            assert_eq!(Locale::from_byte(locale.to_byte()), locale);
        }
        assert_eq!(Locale::from_byte(200), Locale::Zh);
    }

    #[test]
    fn install_is_observable() {
        // Serialised with the other locale test by running in one process; the
        // value is restored so ordering cannot leak into another test.
        let before = current();
        install(Locale::Ja);
        assert_eq!(current(), Locale::Ja);
        install(Locale::En);
        assert_eq!(current(), Locale::En);
        install(before);
    }
}
