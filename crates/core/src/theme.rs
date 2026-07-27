//! Headless theme schema, validation, and persistence.
//!
//! Rendering remains a frontend concern; this module owns the portable
//! `.ochub-theme.json` contract so GUI and CLI operate on the same files.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::AppError;

pub const THEME_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_THEME_FAMILY: &str = "ochub";
pub const EMBER_THEME_FAMILY: &str = "ember";
pub const MIN_SURFACE_OPACITY_PERCENT: u8 = 0;
pub const MAX_SURFACE_OPACITY_PERCENT: u8 = 100;
pub const DEFAULT_SIDEBAR_OPACITY_PERCENT: u8 = 40;
pub const DEFAULT_CONTENT_OPACITY_PERCENT: u8 = 100;
const MAX_THEME_FILE_BYTES: u64 = 256 * 1024;
const MAX_THEME_NAME_CHARS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeColor(pub u32);

impl ThemeColor {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn hex(self) -> String {
        format!("#{:06X}", self.0)
    }

    pub fn parse(value: &str) -> Result<Self, AppError> {
        let hex = value.trim().strip_prefix('#').unwrap_or(value.trim());
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(AppError::InvalidInput(
                "theme colors must use #RRGGBB".to_string(),
            ));
        }
        u32::from_str_radix(hex, 16)
            .map(Self)
            .map_err(|error| AppError::InvalidInput(error.to_string()))
    }
}

impl Serialize for ThemeColor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeWindowBackground {
    #[default]
    Opaque,
    Blurred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeEffects {
    #[serde(default)]
    pub window_background: ThemeWindowBackground,
    #[serde(default = "default_sidebar_opacity_percent")]
    pub sidebar_opacity: u8,
    #[serde(default = "default_content_opacity_percent")]
    pub content_opacity: u8,
}

impl ThemeEffects {
    pub const DEFAULT: Self = Self {
        window_background: ThemeWindowBackground::Opaque,
        sidebar_opacity: DEFAULT_SIDEBAR_OPACITY_PERCENT,
        content_opacity: DEFAULT_CONTENT_OPACITY_PERCENT,
    };
}

impl Default for ThemeEffects {
    fn default() -> Self {
        Self::DEFAULT
    }
}

const fn default_sidebar_opacity_percent() -> u8 {
    DEFAULT_SIDEBAR_OPACITY_PERCENT
}

const fn default_content_opacity_percent() -> u8 {
    DEFAULT_CONTENT_OPACITY_PERCENT
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Theme {
    #[serde(default)]
    pub effects: ThemeEffects,
    pub bg: ThemeColor,
    pub mantle: ThemeColor,
    pub surface: ThemeColor,
    pub overlay: ThemeColor,
    pub surface_hover: ThemeColor,
    pub panel: ThemeColor,
    pub inset: ThemeColor,
    pub border: ThemeColor,
    pub border_strong: ThemeColor,
    pub text: ThemeColor,
    pub subtext: ThemeColor,
    pub muted: ThemeColor,
    pub accent: ThemeColor,
    pub accent_fill: ThemeColor,
    pub accent_hover: ThemeColor,
    pub accent_soft: ThemeColor,
    pub accent_text: ThemeColor,
    pub green: ThemeColor,
    pub green_soft: ThemeColor,
    pub red: ThemeColor,
    pub red_soft: ThemeColor,
    pub red_hover: ThemeColor,
    pub yellow: ThemeColor,
    pub yellow_soft: ThemeColor,
    pub mauve: ThemeColor,
    pub teal: ThemeColor,
    pub peach: ThemeColor,
    pub sidebar_selected: ThemeColor,
    pub sidebar_text: ThemeColor,
    pub sidebar_muted: ThemeColor,
    pub header: ThemeColor,
    pub selection: ThemeColor,
    pub error_surface: ThemeColor,
    pub scrim: ThemeColor,
    pub shadow: ThemeColor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeFamily {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub light: Theme,
    pub dark: Theme,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeRecord {
    pub family: ThemeFamily,
    pub built_in: bool,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeRegistry {
    pub themes: Vec<ThemeRecord>,
    pub diagnostics: Vec<String>,
}

macro_rules! tc {
    ($value:expr) => {
        ThemeColor::new($value)
    };
}

pub const OCHUB_LIGHT: Theme = Theme {
    effects: ThemeEffects::DEFAULT,
    bg: tc!(0xfcfcfb),
    mantle: tc!(0xf4f4f2),
    surface: tc!(0xfffefc),
    overlay: tc!(0xfffefa),
    surface_hover: tc!(0xeeeeea),
    panel: tc!(0xf8f8f6),
    inset: tc!(0xf1f1ed),
    border: tc!(0xe7e6e2),
    border_strong: tc!(0xd6d5d0),
    text: tc!(0x222019),
    subtext: tc!(0x6b6a64),
    muted: tc!(0x76756f),
    accent: tc!(0x2563dd),
    accent_fill: tc!(0x2563dd),
    accent_hover: tc!(0x1c54c2),
    accent_soft: tc!(0xe1ebfc),
    accent_text: tc!(0xfbfcff),
    green: tc!(0x237a49),
    green_soft: tc!(0xdff2e6),
    red: tc!(0xb73e36),
    red_soft: tc!(0xf6e1dd),
    red_hover: tc!(0xeac7c1),
    yellow: tc!(0x8a5a09),
    yellow_soft: tc!(0xf5ecd4),
    mauve: tc!(0x6c53b8),
    teal: tc!(0x087d74),
    peach: tc!(0xb94f26),
    sidebar_selected: tc!(0xe1ebfc),
    sidebar_text: tc!(0x2c2a23),
    sidebar_muted: tc!(0x76756f),
    header: tc!(0xfbfbf9),
    selection: tc!(0xbcd2f9),
    error_surface: tc!(0xffeef0),
    scrim: tc!(0x0b0c0a),
    shadow: tc!(0x2b2820),
};

pub const OCHUB_DARK: Theme = Theme {
    effects: ThemeEffects::DEFAULT,
    bg: tc!(0x151613),
    mantle: tc!(0x1b1c18),
    surface: tc!(0x22231f),
    overlay: tc!(0x292a25),
    surface_hover: tc!(0x2d2e29),
    panel: tc!(0x1c1d19),
    inset: tc!(0x191a17),
    border: tc!(0x383933),
    border_strong: tc!(0x6e6f64),
    text: tc!(0xf1f0e8),
    subtext: tc!(0xbfbeb5),
    muted: tc!(0x96958d),
    accent: tc!(0x75a7ff),
    accent_fill: tc!(0x3568c8),
    accent_hover: tc!(0x2d5ab0),
    accent_soft: tc!(0x203455),
    accent_text: tc!(0xfcfdff),
    green: tc!(0x69c98c),
    green_soft: tc!(0x183829),
    red: tc!(0xff8078),
    red_soft: tc!(0x442422),
    red_hover: tc!(0x5b2b28),
    yellow: tc!(0xe8b65a),
    yellow_soft: tc!(0x3c301b),
    mauve: tc!(0xb9a0ff),
    teal: tc!(0x59c7bc),
    peach: tc!(0xf09a70),
    sidebar_selected: tc!(0x243854),
    sidebar_text: tc!(0xe7e6de),
    sidebar_muted: tc!(0x96958d),
    header: tc!(0x181915),
    selection: tc!(0x31517d),
    error_surface: tc!(0x442422),
    scrim: tc!(0x080907),
    shadow: tc!(0x070806),
};

pub const EMBER_LIGHT: Theme = Theme {
    effects: ThemeEffects::DEFAULT,
    bg: tc!(0xfbf7f0),
    mantle: tc!(0xf2e8da),
    surface: tc!(0xfffcf7),
    overlay: tc!(0xfffaf4),
    surface_hover: tc!(0xf1e5d8),
    panel: tc!(0xf7efe5),
    inset: tc!(0xf0e4d6),
    border: tc!(0xe4d4c4),
    border_strong: tc!(0xccb59f),
    text: tc!(0x2b2118),
    subtext: tc!(0x6c5849),
    muted: tc!(0x77665a),
    accent: tc!(0xb94f12),
    accent_fill: tc!(0xb94f12),
    accent_hover: tc!(0x9f3f0b),
    accent_soft: tc!(0xfff4ec),
    accent_text: tc!(0xfff8f2),
    green: tc!(0x267548),
    green_soft: tc!(0xe0f0e5),
    red: tc!(0xb33b36),
    red_soft: tc!(0xf6dedb),
    red_hover: tc!(0xebc2be),
    yellow: tc!(0x8b5b09),
    yellow_soft: tc!(0xf5ead1),
    mauve: tc!(0x6f53ac),
    teal: tc!(0x0c7a70),
    peach: tc!(0xb94f12),
    sidebar_selected: tc!(0xf6dfcc),
    sidebar_text: tc!(0x33251a),
    sidebar_muted: tc!(0x7a6758),
    header: tc!(0xfaf3ea),
    selection: tc!(0xf5c7a4),
    error_surface: tc!(0xfce9e6),
    scrim: tc!(0x1a1008),
    shadow: tc!(0x4a2a16),
};

pub const EMBER_DARK: Theme = Theme {
    effects: ThemeEffects::DEFAULT,
    bg: tc!(0x18130f),
    mantle: tc!(0x201711),
    surface: tc!(0x291d15),
    overlay: tc!(0x33231a),
    surface_hover: tc!(0x3a2a20),
    panel: tc!(0x241a14),
    inset: tc!(0x15100d),
    border: tc!(0x493429),
    border_strong: tc!(0x74513d),
    text: tc!(0xf7eee5),
    subtext: tc!(0xccb9a6),
    muted: tc!(0x9e8876),
    accent: tc!(0xff9b52),
    accent_fill: tc!(0xb94f12),
    accent_hover: tc!(0x9f3f0b),
    accent_soft: tc!(0x4a2816),
    accent_text: tc!(0xfff8f2),
    green: tc!(0x78c68b),
    green_soft: tc!(0x193a29),
    red: tc!(0xf28078),
    red_soft: tc!(0x472522),
    red_hover: tc!(0x5a2e29),
    yellow: tc!(0xe6b55c),
    yellow_soft: tc!(0x3d301b),
    mauve: tc!(0xb69bdb),
    teal: tc!(0x62c6b7),
    peach: tc!(0xff9b52),
    sidebar_selected: tc!(0x4a2816),
    sidebar_text: tc!(0xf3e5d7),
    sidebar_muted: tc!(0xa88e79),
    header: tc!(0x1d1510),
    selection: tc!(0x6b3b20),
    error_surface: tc!(0x472522),
    scrim: tc!(0x090704),
    shadow: tc!(0x080604),
};

pub fn ochub_family() -> ThemeFamily {
    ThemeFamily {
        schema_version: THEME_SCHEMA_VERSION,
        id: DEFAULT_THEME_FAMILY.to_string(),
        name: "OcHub".to_string(),
        author: "OcHub".to_string(),
        description: "OcHub default theme".to_string(),
        light: OCHUB_LIGHT,
        dark: OCHUB_DARK,
    }
}

pub fn ember_family() -> ThemeFamily {
    ThemeFamily {
        schema_version: THEME_SCHEMA_VERSION,
        id: EMBER_THEME_FAMILY.to_string(),
        name: "Ember Orange".to_string(),
        author: "OcHub".to_string(),
        description: "Warm orange OcHub theme".to_string(),
        light: EMBER_LIGHT,
        dark: EMBER_DARK,
    }
}

pub fn themes_dir() -> PathBuf {
    crate::paths::get_app_config_dir().join("themes")
}

fn built_in_records() -> Vec<ThemeRecord> {
    [ochub_family(), ember_family()]
        .into_iter()
        .map(|family| ThemeRecord {
            family,
            built_in: true,
            path: None,
        })
        .collect()
}

fn is_theme_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".ochub-theme.json"))
}

pub fn read_theme_file(path: &Path) -> Result<ThemeFamily, AppError> {
    let metadata = fs::metadata(path).map_err(|error| AppError::io(path, error))?;
    if !metadata.is_file() {
        return Err(AppError::InvalidInput(format!(
            "theme is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_THEME_FILE_BYTES {
        return Err(AppError::InvalidInput(format!(
            "theme file exceeds {MAX_THEME_FILE_BYTES} bytes"
        )));
    }
    let content = fs::read_to_string(path).map_err(|error| AppError::io(path, error))?;
    let family = serde_json::from_str(&content).map_err(|source| AppError::json(path, source))?;
    validate_family(&family)?;
    Ok(family)
}

static REGISTRY_CACHE: LazyLock<RwLock<Option<Arc<ThemeRegistry>>>> =
    LazyLock::new(|| RwLock::new(None));

fn scan_registry() -> ThemeRegistry {
    let mut registry = ThemeRegistry {
        themes: built_in_records(),
        diagnostics: Vec::new(),
    };
    let directory = themes_dir();
    if let Err(error) = fs::create_dir_all(&directory) {
        registry.diagnostics.push(format!(
            "cannot create theme directory {}: {error}",
            directory.display()
        ));
        return registry;
    }
    let mut paths = match fs::read_dir(&directory) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_theme_file(path))
            .collect::<Vec<_>>(),
        Err(error) => {
            registry.diagnostics.push(format!(
                "cannot read theme directory {}: {error}",
                directory.display()
            ));
            return registry;
        }
    };
    paths.sort();
    for path in paths {
        match read_theme_file(&path) {
            Ok(family)
                if registry
                    .themes
                    .iter()
                    .any(|record| record.family.id == family.id) =>
            {
                registry.diagnostics.push(format!(
                    "duplicate theme id {} in {}",
                    family.id,
                    path.display()
                ));
            }
            Ok(family) => registry.themes.push(ThemeRecord {
                family,
                built_in: false,
                path: Some(path),
            }),
            Err(error) => registry.diagnostics.push(error.to_string()),
        }
    }
    registry.themes[2..].sort_by(|left, right| {
        left.family
            .name
            .to_lowercase()
            .cmp(&right.family.name.to_lowercase())
    });
    registry
}

pub fn load_registry() -> Arc<ThemeRegistry> {
    if let Ok(cache) = REGISTRY_CACHE.read() {
        if let Some(registry) = cache.as_ref() {
            return registry.clone();
        }
    }
    reload_registry()
}

pub fn reload_registry() -> Arc<ThemeRegistry> {
    let registry = Arc::new(scan_registry());
    if let Ok(mut cache) = REGISTRY_CACHE.write() {
        *cache = Some(registry.clone());
    }
    registry
}

fn invalidate_registry() {
    if let Ok(mut cache) = REGISTRY_CACHE.write() {
        *cache = None;
    }
}

pub fn find_family(id: &str) -> Option<ThemeFamily> {
    load_registry()
        .themes
        .iter()
        .find(|record| record.family.id == id)
        .map(|record| record.family.clone())
}

fn valid_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && id.len() <= 64
}

fn relative_luminance(color: ThemeColor) -> f32 {
    fn channel(value: u32) -> f32 {
        let value = value as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    let red = channel((color.0 >> 16) & 0xff);
    let green = channel((color.0 >> 8) & 0xff);
    let blue = channel(color.0 & 0xff);
    red * 0.2126 + green * 0.7152 + blue * 0.0722
}

pub fn contrast_ratio(foreground: ThemeColor, background: ThemeColor) -> f32 {
    let foreground = relative_luminance(foreground);
    let background = relative_luminance(background);
    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
}

fn validate_palette(variant: &str, palette: &Theme) -> Result<(), AppError> {
    for (field, value) in [
        ("sidebarOpacity", palette.effects.sidebar_opacity),
        ("contentOpacity", palette.effects.content_opacity),
    ] {
        if !(MIN_SURFACE_OPACITY_PERCENT..=MAX_SURFACE_OPACITY_PERCENT).contains(&value) {
            return Err(AppError::InvalidInput(format!(
                "{variant}.{field} must be between 0 and 100"
            )));
        }
    }
    for (field, foreground, background) in [
        ("text/bg", palette.text, palette.bg),
        ("subtext/surface", palette.subtext, palette.surface),
        ("muted/surface", palette.muted, palette.surface),
        ("accent/surface", palette.accent, palette.surface),
        (
            "accentText/accentFill",
            palette.accent_text,
            palette.accent_fill,
        ),
    ] {
        let ratio = contrast_ratio(foreground, background);
        if ratio < 4.5 {
            return Err(AppError::InvalidInput(format!(
                "{variant} theme contrast for {field} is {ratio:.2}; at least 4.5 is required"
            )));
        }
    }
    Ok(())
}

pub fn validate_family(family: &ThemeFamily) -> Result<(), AppError> {
    if family.schema_version != THEME_SCHEMA_VERSION {
        return Err(AppError::InvalidInput(format!(
            "unsupported theme schema {}; expected {}",
            family.schema_version, THEME_SCHEMA_VERSION
        )));
    }
    if !valid_id(&family.id) {
        return Err(AppError::InvalidInput(
            "theme id must be lowercase kebab-case and at most 64 bytes".to_string(),
        ));
    }
    let name = family.name.trim();
    if name.is_empty() || name.chars().count() > MAX_THEME_NAME_CHARS {
        return Err(AppError::InvalidInput(
            "theme name must contain 1 to 80 characters".to_string(),
        ));
    }
    if family.author.chars().count() > 80 {
        return Err(AppError::InvalidInput(
            "theme author must not exceed 80 characters".to_string(),
        ));
    }
    if family.description.chars().count() > 400 {
        return Err(AppError::InvalidInput(
            "theme description must not exceed 400 characters".to_string(),
        ));
    }
    validate_palette("light", &family.light)?;
    validate_palette("dark", &family.dark)
}

fn unique_id(base: &str, registry: &ThemeRegistry) -> String {
    let sanitized = base
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-' {
                character
            } else if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let base = sanitized.trim_matches('-');
    let base = if base.is_empty() { "theme" } else { base };
    for suffix in 1usize.. {
        let tail = if suffix == 1 {
            String::new()
        } else {
            format!("-{suffix}")
        };
        let stem_len = 64usize.saturating_sub(tail.len());
        let stem = base
            .chars()
            .take(stem_len)
            .collect::<String>()
            .trim_end_matches('-')
            .to_string();
        let candidate = format!("{stem}{tail}");
        if !registry
            .themes
            .iter()
            .any(|record| record.family.id == candidate)
        {
            return candidate;
        }
    }
    unreachable!()
}

fn write_family(path: &Path, family: &ThemeFamily) -> Result<(), AppError> {
    validate_family(family)?;
    let bytes =
        serde_json::to_vec_pretty(family).map_err(|source| AppError::JsonSerialize { source })?;
    crate::paths::atomic_write(path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| AppError::io(path, error))?;
    }
    Ok(())
}

pub fn save_user_family(family: &ThemeFamily) -> Result<PathBuf, AppError> {
    if matches!(
        family.id.as_str(),
        DEFAULT_THEME_FAMILY | EMBER_THEME_FAMILY
    ) {
        return Err(AppError::InvalidInput(
            "built-in themes are read-only".to_string(),
        ));
    }
    let path = themes_dir().join(format!("{}.ochub-theme.json", family.id));
    write_family(&path, family)?;
    invalidate_registry();
    Ok(path)
}

pub fn duplicate_family(source: &ThemeFamily) -> Result<ThemeFamily, AppError> {
    let registry = load_registry();
    let mut family = source.clone();
    family.id = unique_id(&format!("{}-copy", source.id), &registry);
    family.name = format!("{} Copy", source.name)
        .chars()
        .take(MAX_THEME_NAME_CHARS)
        .collect();
    family.author.clear();
    validate_family(&family)?;
    Ok(family)
}

pub fn import_family(path: &Path) -> Result<ThemeFamily, AppError> {
    let registry = load_registry();
    let mut family = read_theme_file(path)?;
    if registry
        .themes
        .iter()
        .any(|record| record.family.id == family.id)
    {
        family.id = unique_id(&format!("{}-imported", family.id), &registry);
        family.name = format!("{} Imported", family.name)
            .chars()
            .take(MAX_THEME_NAME_CHARS)
            .collect();
    }
    save_user_family(&family)?;
    Ok(family)
}

pub fn export_family(family: &ThemeFamily, path: &Path) -> Result<(), AppError> {
    write_family(path, family)
}

pub fn delete_user_family(id: &str) -> Result<PathBuf, AppError> {
    let registry = load_registry();
    let record = registry
        .themes
        .iter()
        .find(|record| record.family.id == id)
        .ok_or_else(|| AppError::InvalidInput(format!("theme not found: {id}")))?;
    if record.built_in {
        return Err(AppError::InvalidInput(
            "built-in themes cannot be deleted".to_string(),
        ));
    }
    let path = record
        .path
        .clone()
        .ok_or_else(|| AppError::Config("theme record has no source path".to_string()))?;
    fs::remove_file(&path).map_err(|error| AppError::io(&path, error))?;
    invalidate_registry();
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_validate_and_round_trip() {
        for family in [ochub_family(), ember_family()] {
            validate_family(&family).unwrap();
            let encoded = serde_json::to_string(&family).unwrap();
            let decoded: ThemeFamily = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, family);
        }
    }

    #[test]
    fn invalid_contrast_is_rejected() {
        let mut family = ochub_family();
        family.light.text = family.light.bg;
        assert!(validate_family(&family).is_err());
    }

    #[test]
    fn colors_require_six_hex_digits() {
        assert_eq!(ThemeColor::parse("#A1B2C3").unwrap(), tc!(0xa1b2c3));
        assert!(ThemeColor::parse("#fff").is_err());
    }
}
