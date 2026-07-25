//! Application-wide error type.
//!
//! Ported faithfully from cc-switch `src-tauri/src/error.rs`. Error display
//! strings keep their original (bilingual / Chinese) wording so log output and
//! localized error keys match the reference implementation 1:1.

use std::path::Path;
use std::sync::PoisonError;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("配置错误: {0}")]
    Config(String),
    #[error("无效输入: {0}")]
    InvalidInput(String),
    #[error("IO 错误: {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{context}: {source}")]
    IoContext {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON 解析错误: {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("JSON 序列化失败: {source}")]
    JsonSerialize {
        #[source]
        source: serde_json::Error,
    },
    #[error("TOML 解析错误: {path}: {source}")]
    Toml {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("锁获取失败: {0}")]
    Lock(String),
    #[error("MCP 校验失败: {0}")]
    McpValidation(String),
    #[error("{0}")]
    Message(String),
    #[error("HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("{}", localized_message(zh, en, ja.as_deref()))]
    Localized {
        key: &'static str,
        zh: String,
        en: String,
        /// `None` falls back to English, which is closer for a Japanese
        /// reader than Chinese and marks the string as not yet translated.
        ja: Option<String>,
    },
    #[error("数据库错误: {0}")]
    Database(String),
    #[error("应用已停用: {0}")]
    AppDisabled(String),
    #[error("OMO 配置文件不存在")]
    OmoConfigNotFound,
    #[error("所有供应商已熔断，无可用上游")]
    AllProvidersCircuitOpen,
    #[error("未配置供应商")]
    NoProvidersConfigured,
}

pub type Result<T> = std::result::Result<T, AppError>;

impl AppError {
    pub fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    pub fn json(path: impl AsRef<Path>, source: serde_json::Error) -> Self {
        Self::Json {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    pub fn toml(path: impl AsRef<Path>, source: toml::de::Error) -> Self {
        Self::Toml {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub fn localized(key: &'static str, zh: impl Into<String>, en: impl Into<String>) -> Self {
        Self::Localized {
            key,
            zh: zh.into(),
            en: en.into(),
            ja: None,
        }
    }

    /// [`AppError::localized`] with a Japanese rendering.
    pub fn localized_ja(
        key: &'static str,
        zh: impl Into<String>,
        en: impl Into<String>,
        ja: impl Into<String>,
    ) -> Self {
        Self::Localized {
            key,
            zh: zh.into(),
            en: en.into(),
            ja: Some(ja.into()),
        }
    }
}

/// Pick the rendering for the installed locale.
///
/// This used to be `"{zh} ({en})"` — both languages concatenated, so every
/// reader saw one language they did not want. Now that core owns the locale,
/// an error can answer in it.
fn localized_message<'a>(zh: &'a str, en: &'a str, ja: Option<&'a str>) -> &'a str {
    match crate::i18n::current() {
        crate::i18n::Locale::Zh => zh,
        crate::i18n::Locale::En => en,
        crate::i18n::Locale::Ja => ja.unwrap_or(en),
    }
}

impl<T> From<PoisonError<T>> for AppError {
    fn from(err: PoisonError<T>) -> Self {
        Self::Lock(err.to_string())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Database(err.to_string())
    }
}

impl From<AppError> for String {
    fn from(err: AppError) -> Self {
        err.to_string()
    }
}

impl serde::Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// Format a structured skill error as a JSON string the frontend can parse.
pub fn format_skill_error(
    code: &str,
    context: &[(&str, &str)],
    suggestion: Option<&str>,
) -> String {
    use serde_json::json;

    let mut ctx_map = serde_json::Map::new();
    for (key, value) in context {
        ctx_map.insert(key.to_string(), json!(value));
    }

    let error_obj = json!({
        "code": code,
        "context": ctx_map,
        "suggestion": suggestion,
    });

    serde_json::to_string(&error_obj).unwrap_or_else(|_| format!("ERROR:{code}"))
}

#[cfg(test)]
mod localized_display_tests {
    use super::*;
    use crate::i18n::{self, Locale};
    use std::sync::Mutex;

    /// The locale is process-global and tests run in parallel.
    static LOCALE: Mutex<()> = Mutex::new(());

    #[test]
    fn renders_only_the_installed_locale() {
        let _guard = LOCALE.lock().unwrap_or_else(|err| err.into_inner());
        let before = i18n::current();
        let err = AppError::localized_ja("t.key", "连接失败", "Connection failed", "接続に失敗");

        i18n::install(Locale::Zh);
        assert_eq!(err.to_string(), "连接失败");
        i18n::install(Locale::En);
        assert_eq!(err.to_string(), "Connection failed");
        i18n::install(Locale::Ja);
        assert_eq!(err.to_string(), "接続に失敗");

        i18n::install(before);
    }

    #[test]
    fn japanese_falls_back_to_english_not_chinese() {
        let _guard = LOCALE.lock().unwrap_or_else(|err| err.into_inner());
        let before = i18n::current();
        // The 131 existing call sites supply no Japanese yet. English is the
        // closer fallback for a Japanese reader, and leaving Chinese in place
        // would hide which strings still need translating.
        let err = AppError::localized("t.key", "连接失败", "Connection failed");

        i18n::install(Locale::Ja);
        assert_eq!(err.to_string(), "Connection failed");

        i18n::install(before);
    }

    #[test]
    fn no_locale_renders_both_languages_concatenated() {
        let _guard = LOCALE.lock().unwrap_or_else(|err| err.into_inner());
        let before = i18n::current();
        let err = AppError::localized("t.key", "连接失败", "Connection failed");

        // Regression guard: the old Display was "{zh} ({en})", which showed
        // every reader one language they did not ask for.
        for locale in Locale::ALL {
            i18n::install(locale);
            let rendered = err.to_string();
            assert!(
                !(rendered.contains("连接失败") && rendered.contains("Connection failed")),
                "{locale:?} rendered both languages: {rendered}"
            );
        }

        i18n::install(before);
    }
}
