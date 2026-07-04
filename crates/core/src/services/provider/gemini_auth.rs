//! Gemini authentication type detection
//!
//! Detects whether a Gemini provider uses PackyCode API Key, Google OAuth, or generic API Key.

use crate::error::AppError;
use crate::model::Provider;

/// Gemini authentication type enumeration
///
/// Used to optimize performance by avoiding repeated provider type detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeminiAuthType {
    /// PackyCode provider (uses API Key)
    Packycode,
    /// Google Official (uses OAuth)
    GoogleOfficial,
    /// Generic Gemini provider (uses API Key)
    Generic,
}

// Partner Promotion Key constants
const PACKYCODE_PARTNER_KEY: &str = "packycode";
const GOOGLE_OFFICIAL_PARTNER_KEY: &str = "google-official";

// PackyCode keyword constants
const PACKYCODE_KEYWORDS: [&str; 3] = ["packycode", "packyapi", "packy"];

/// Detect Gemini provider authentication type
///
/// One-time detection to avoid repeated calls to `is_packycode_gemini` and `is_google_official_gemini`.
pub(crate) fn detect_gemini_auth_type(provider: &Provider) -> GeminiAuthType {
    // Priority 1: Check partner_promotion_key (most reliable)
    if let Some(key) = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.partner_promotion_key.as_deref())
    {
        if key.eq_ignore_ascii_case(GOOGLE_OFFICIAL_PARTNER_KEY) {
            return GeminiAuthType::GoogleOfficial;
        }
        if key.eq_ignore_ascii_case(PACKYCODE_PARTNER_KEY) {
            return GeminiAuthType::Packycode;
        }
    }

    // Priority 2: Check Google Official (name matching)
    let name_lower = provider.name.to_ascii_lowercase();
    if name_lower == "google" || name_lower.starts_with("google ") {
        return GeminiAuthType::GoogleOfficial;
    }

    // Priority 3: Check PackyCode keywords
    if contains_packycode_keyword(&provider.name) {
        return GeminiAuthType::Packycode;
    }

    if let Some(site) = provider.website_url.as_deref() {
        if contains_packycode_keyword(site) {
            return GeminiAuthType::Packycode;
        }
    }

    if let Some(base_url) = provider
        .settings_config
        .pointer("/env/GOOGLE_GEMINI_BASE_URL")
        .and_then(|v| v.as_str())
    {
        if contains_packycode_keyword(base_url) {
            return GeminiAuthType::Packycode;
        }
    }

    // No API key configured -> treat as OAuth (Google-official login). An API-key
    // (Generic) provider with no key cannot authenticate, so an empty-env provider
    // is the OAuth case the editor's "OAuth" mode produces.
    let has_api_key = provider
        .settings_config
        .pointer("/env/GEMINI_API_KEY")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_api_key {
        return GeminiAuthType::GoogleOfficial;
    }

    GeminiAuthType::Generic
}

/// Check if string contains PackyCode related keywords (case-insensitive)
fn contains_packycode_keyword(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    PACKYCODE_KEYWORDS
        .iter()
        .any(|keyword| lower.contains(keyword))
}

/// Detect if provider is Google Official Gemini (uses OAuth authentication)
///
/// This is a convenience wrapper around `detect_gemini_auth_type`.
pub(crate) fn is_google_official_gemini(provider: &Provider) -> bool {
    detect_gemini_auth_type(provider) == GeminiAuthType::GoogleOfficial
}

/// Ensure Google Official Gemini provider security flag is correctly set (OAuth mode).
///
/// Writes `security.auth.selectedType = "oauth-personal"` to `~/.gemini/settings.json`.
/// If the provider is not Google Official, returns `Ok(())` immediately.
pub(crate) fn ensure_google_oauth_security_flag(provider: &Provider) -> Result<(), AppError> {
    if !is_google_official_gemini(provider) {
        return Ok(());
    }

    // Write to Gemini directory settings.json (~/.gemini/settings.json)
    use crate::apps::gemini::write_google_oauth_settings;
    write_google_oauth_settings()?;

    Ok(())
}
