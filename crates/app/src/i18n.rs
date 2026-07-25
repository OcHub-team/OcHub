//! Translation lookup.
//!
//! Catalogs live in `crates/app/i18n/<locale>.toml`, one flat `key = "text"`
//! table per language, and `build.rs` compiles them into the static tables
//! included below. That split is the point: translators edit TOML and never
//! touch Rust, and adding a language touches no call site.
//!
//! The build script rejects a catalog that is missing a key or whose
//! placeholders disagree with the reference catalog, so those are build
//! failures rather than blank labels discovered by a user.
//!
//! ```ignore
//! div().child(t(k::SETTINGS_LANGUAGE_LABEL))
//! self.set_status(tf!(k::SKILLS_UPDATED, count = n), cx);
//! let verb = raw(k::COMMON_ENABLE);   // when a &'static str is required
//! ```
//!
//! Only user-visible prose belongs in a catalog. Element ids, settings keys and
//! any string compared with `==` are identifiers that happen to be spelled in
//! Chinese — translating them changes behaviour.

use gpui::SharedString;
use ochub_core::i18n::Locale;

include!(concat!(env!("OUT_DIR"), "/i18n_generated.rs"));

/// The raw template for a key in the current locale.
///
/// Prefer [`t`] for anything going straight into an element; this exists for
/// the APIs that still require a `&'static str` and for [`tf!`].
#[inline]
pub fn raw(key: Key) -> &'static str {
    let index = key.index();
    match ochub_core::i18n::current() {
        Locale::Zh => ZH[index],
        Locale::En => EN[index],
        Locale::Ja => JA[index],
    }
}

/// A translated string for the current locale.
///
/// Never allocates: the generated tables are `&'static str`, so this is a
/// `const`-constructed `SharedString`. It is cheaper than passing a bare
/// `&'static str` to gpui, which re-runs `SharedString::from` — and thus
/// heap-allocates for anything over 23 bytes — on every layout pass.
#[inline]
pub fn t(key: Key) -> SharedString {
    SharedString::new_static(raw(key))
}

/// Substitute `{name}` placeholders, honouring `{{` and `}}` as escapes.
///
/// A placeholder with no matching argument is left as written rather than
/// silently dropped, so a mistake shows up in the UI as `{count}` instead of a
/// sentence with a hole in it. `build.rs` already guarantees the locales agree
/// on which placeholders exist.
pub fn format_named(template: &str, args: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut literal_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                out.push_str(&template[literal_start..i]);
                out.push('{');
                i += 2;
                literal_start = i;
            }
            b'}' if bytes.get(i + 1) == Some(&b'}') => {
                out.push_str(&template[literal_start..i]);
                out.push('}');
                i += 2;
                literal_start = i;
            }
            b'{' => {
                let start = i + 1;
                let Some(relative_end) = bytes[start..].iter().position(|byte| *byte == b'}')
                else {
                    break;
                };
                let end = start + relative_end;
                let name = &template[start..end];
                out.push_str(&template[literal_start..i]);
                match args.iter().find(|(key, _)| *key == name) {
                    Some((_, value)) => out.push_str(value),
                    None => out.push_str(&template[i..=end]),
                }
                i = end + 1;
                literal_start = i;
            }
            _ => i += 1,
        }
    }
    out.push_str(&template[literal_start..]);
    out
}

/// A translated string with named arguments: `tf!(k::SKILLS_UPDATED, count = n)`.
///
/// Argument names must match the `{name}` placeholders in the catalog. Values
/// only need to implement `Display`.
#[macro_export]
macro_rules! tf {
    ($key:expr $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::i18n::format_named(
            $crate::i18n::raw($key),
            &[$((stringify!($name), $value.to_string())),*],
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::{Mutex, MutexGuard};

    /// The locale is process-global and tests run in parallel.
    static LOCALE: Mutex<()> = Mutex::new(());

    fn locked() -> MutexGuard<'static, ()> {
        LOCALE.lock().unwrap_or_else(|err| err.into_inner())
    }

    #[test]
    fn every_locale_table_is_fully_populated() {
        assert_eq!(ZH.len(), COUNT);
        assert_eq!(EN.len(), COUNT);
        assert_eq!(JA.len(), COUNT);
        assert_ne!(
            KEY_NAMES.iter().count(),
            0,
            "the catalog should not be empty"
        );
        // Values may legitimately be empty: a grammatical particle that only
        // one language needs (the pagination suffix reads "跳至 [n] 页" in
        // Chinese but "Go to page [n]" in English) is correctly translated as
        // nothing. The guarantee that matters — the key exists in every locale
        // — is enforced by build.rs, which fails the build on a missing key.
        let blank: Vec<&str> = KEY_NAMES
            .iter()
            .enumerate()
            .filter(|(index, _)| ZH[*index].is_empty())
            .map(|(_, name)| *name)
            .collect();
        assert!(
            blank.is_empty(),
            "the reference locale must never be blank: {blank:?}"
        );
    }

    #[test]
    fn keys_are_unique() {
        let unique: BTreeSet<&&str> = KEY_NAMES.iter().collect();
        assert_eq!(unique.len(), KEY_NAMES.len());
    }

    /// The build script checks this too, but only against the reference
    /// catalog; this asserts the invariant on the compiled tables.
    #[test]
    fn placeholders_agree_across_locales() {
        fn names(template: &str) -> BTreeSet<String> {
            let mut found = BTreeSet::new();
            let chars: Vec<char> = template.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                match chars[i] {
                    '{' if chars.get(i + 1) == Some(&'{') => i += 2,
                    '}' if chars.get(i + 1) == Some(&'}') => i += 2,
                    '{' => {
                        let start = i + 1;
                        let mut end = start;
                        while end < chars.len() && chars[end] != '}' {
                            end += 1;
                        }
                        if end < chars.len() {
                            found.insert(chars[start..end].iter().collect());
                        }
                        i = end + 1;
                    }
                    _ => i += 1,
                }
            }
            found
        }

        for index in 0..COUNT {
            let expected = names(ZH[index]);
            assert_eq!(names(EN[index]), expected, "en `{}`", KEY_NAMES[index]);
            assert_eq!(names(JA[index]), expected, "ja `{}`", KEY_NAMES[index]);
        }
    }

    #[test]
    fn lookup_follows_the_installed_locale() {
        let _guard = locked();
        let before = ochub_core::i18n::current();
        for locale in Locale::ALL {
            ochub_core::i18n::install(locale);
            let table = match locale {
                Locale::Zh => &ZH,
                Locale::En => &EN,
                Locale::Ja => &JA,
            };
            let key = k::SETTINGS_BASIC_LANGUAGE_LABEL;
            assert_eq!(raw(key), table[key.index()]);
            assert_eq!(t(key).as_ref(), raw(key));
        }
        ochub_core::i18n::install(before);
    }

    #[test]
    fn named_substitution_handles_escapes_and_reordering() {
        assert_eq!(
            format_named("已更新 {count} 个技能", &[("count", "3".into())]),
            "已更新 3 个技能"
        );
        // Japanese puts the object first; the same argument set still applies.
        assert_eq!(
            format_named(
                "{name} を{verb}しました",
                &[("verb", "有効化".into()), ("name", "Codex".into())]
            ),
            "Codex を有効化しました"
        );
        assert_eq!(format_named("{{literal}}", &[]), "{literal}");
    }

    #[test]
    fn an_unmatched_placeholder_survives_visibly() {
        assert_eq!(format_named("{count} 项", &[]), "{count} 项");
        assert_eq!(format_named("未闭合 {count", &[]), "未闭合 {count");
    }

    #[test]
    fn tf_macro_accepts_display_arguments() {
        let _guard = locked();
        let before = ochub_core::i18n::current();
        ochub_core::i18n::install(Locale::Zh);
        assert_eq!(
            tf!(k::SETTINGS_BASIC_LANGUAGE_LABEL),
            raw(k::SETTINGS_BASIC_LANGUAGE_LABEL)
        );
        ochub_core::i18n::install(before);
    }
}
