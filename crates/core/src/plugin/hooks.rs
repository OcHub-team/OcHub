//! Typed hook registry for manifest apps.
//!
//! A manifest can name three kinds of hooks — a `live_validate` precondition, an
//! ordered list of `post_write` side effects, and a `decode` augmentation — but
//! the *implementations* are native Rust, registered here by name. Built-in
//! hooks ([`HookRegistry::builtin`]) reproduce the parts of the native Gemini
//! live-write that are not expressible declaratively (auth-type detection and
//! the `security.auth.selectedType` write).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::error::AppError;
use crate::model::{Provider, ProviderMeta};

/// Precondition run before any file is written; `Err` aborts the live write.
pub type LiveValidateHook = Arc<dyn Fn(&Provider) -> Result<(), AppError> + Send + Sync>;

/// Side effect run after all files are written; the path is the resolved config
/// dir.
pub type PostWriteHook = Arc<dyn Fn(&Provider, &Path) -> Result<(), AppError> + Send + Sync>;

/// Extra form values merged after the declarative decode mapping.
pub type DecodeHook =
    Arc<dyn Fn(&Value, Option<&ProviderMeta>) -> Map<String, Value> + Send + Sync>;

/// Named native hooks a manifest may reference.
#[derive(Clone, Default)]
pub struct HookRegistry {
    live_validate: BTreeMap<String, LiveValidateHook>,
    post_write: BTreeMap<String, PostWriteHook>,
    decode: BTreeMap<String, DecodeHook>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_live_validate(&mut self, name: impl Into<String>, hook: LiveValidateHook) {
        self.live_validate.insert(name.into(), hook);
    }

    pub fn register_post_write(&mut self, name: impl Into<String>, hook: PostWriteHook) {
        self.post_write.insert(name.into(), hook);
    }

    pub fn register_decode(&mut self, name: impl Into<String>, hook: DecodeHook) {
        self.decode.insert(name.into(), hook);
    }

    pub fn has_live_validate(&self, name: &str) -> bool {
        self.live_validate.contains_key(name)
    }

    pub fn has_post_write(&self, name: &str) -> bool {
        self.post_write.contains_key(name)
    }

    pub fn has_decode(&self, name: &str) -> bool {
        self.decode.contains_key(name)
    }

    pub fn live_validate(&self, name: &str) -> Option<&LiveValidateHook> {
        self.live_validate.get(name)
    }

    pub fn post_write(&self, name: &str) -> Option<&PostWriteHook> {
        self.post_write.get(name)
    }

    pub fn decode(&self, name: &str) -> Option<&DecodeHook> {
        self.decode.get(name)
    }

    /// The registry seeded with the built-in (Gemini) hooks.
    pub fn builtin() -> Self {
        use crate::apps::gemini::{validate_gemini_settings_strict, write_packycode_settings};
        use crate::services::provider::gemini_auth::{
            detect_gemini_auth_type, ensure_google_oauth_security_flag, GeminiAuthType,
        };

        let mut registry = Self::new();

        // Mirrors write_gemini_live's API-key precondition: OAuth (GoogleOfficial)
        // skips validation; every other auth type requires GEMINI_API_KEY.
        registry.register_live_validate(
            "gemini.strict_validate",
            Arc::new(|provider: &Provider| -> Result<(), AppError> {
                match detect_gemini_auth_type(provider) {
                    GeminiAuthType::GoogleOfficial => Ok(()),
                    GeminiAuthType::Packycode | GeminiAuthType::Generic => {
                        validate_gemini_settings_strict(&provider.settings_config)
                    }
                }
            }),
        );

        // Mirrors write_gemini_live's post-write selectedType branch.
        registry.register_post_write(
            "gemini.selected_type",
            Arc::new(|provider: &Provider, _dir: &Path| -> Result<(), AppError> {
                match detect_gemini_auth_type(provider) {
                    GeminiAuthType::GoogleOfficial => ensure_google_oauth_security_flag(provider),
                    GeminiAuthType::Packycode | GeminiAuthType::Generic => {
                        write_packycode_settings()
                    }
                }
            }),
        );

        registry
    }
}
