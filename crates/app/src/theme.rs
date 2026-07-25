//! Runtime theme tokens, built-in color families, and user-theme persistence.
//!
//! A theme family always contains both a light and dark palette. The selected
//! [`ThemeMode`] decides whether the family follows the native window appearance
//! or pins one variant. User families are portable `.ochub-theme.json` files in
//! the app data directory's `themes/` folder.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

use anyhow::{anyhow, Context as _, Result};
use gpui::{px, rgb, BoxShadow, Hsla, Rgba, Window, WindowAppearance, WindowBackgroundAppearance};
use ochub_core::settings::ThemeMode;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tempfile::NamedTempFile;

use crate::i18n::{k, raw, Key};

pub const THEME_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_THEME_FAMILY: &str = "ochub";
pub const EMBER_THEME_FAMILY: &str = "ember";
const MAX_THEME_FILE_BYTES: u64 = 256 * 1024;
pub const MIN_SURFACE_OPACITY_PERCENT: u8 = 0;
pub const MAX_SURFACE_OPACITY_PERCENT: u8 = 100;
pub const DEFAULT_SIDEBAR_OPACITY_PERCENT: u8 = 40;
pub const DEFAULT_CONTENT_OPACITY_PERCENT: u8 = 100;

/// A serialized RGB color. Theme files use readable `#RRGGBB` strings while
/// GPUI receives the packed integer through [`ThemeColor::rgba`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeColor(pub u32);

impl ThemeColor {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn rgba(self) -> Rgba {
        rgb(self.0)
    }

    pub fn hex(self) -> String {
        format!("#{:06X}", self.0)
    }

    pub fn parse(value: &str) -> Result<Self> {
        let hex = value.trim().strip_prefix('#').unwrap_or(value.trim());
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(anyhow!(raw(k::THEME_ERROR_COLOR_FORMAT)));
        }
        Ok(Self(u32::from_str_radix(hex, 16)?))
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

/// System-provided background treatment underneath the GPUI scene.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeWindowBackground {
    /// Render against a fully opaque native window.
    #[default]
    Opaque,
    /// Use the platform's background blur. The exact blur radius and material
    /// remain system-controlled.
    Blurred,
}

impl ThemeWindowBackground {
    pub const fn appearance(self) -> WindowBackgroundAppearance {
        match self {
            Self::Blurred => WindowBackgroundAppearance::Blurred,
            Self::Opaque => WindowBackgroundAppearance::Opaque,
        }
    }
}

/// Non-color visual treatment stored alongside each light/dark palette.
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

/// Complete semantic color palette consumed by every shared component.
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
    /// Accent used as foreground text, icons, chart lines, and focus rings.
    pub accent: ThemeColor,
    /// Accent used behind text, such as primary buttons and selected dates.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeToken {
    Bg,
    Mantle,
    Surface,
    Overlay,
    SurfaceHover,
    Panel,
    Inset,
    Border,
    BorderStrong,
    Text,
    Subtext,
    Muted,
    Accent,
    AccentFill,
    AccentHover,
    AccentSoft,
    AccentText,
    Green,
    GreenSoft,
    Red,
    RedSoft,
    RedHover,
    Yellow,
    YellowSoft,
    Mauve,
    Teal,
    Peach,
    SidebarSelected,
    SidebarText,
    SidebarMuted,
    Header,
    Selection,
    ErrorSurface,
    Scrim,
    Shadow,
}

/// One editable color token in the theme editor.
///
/// `key` is the identifier the theme file uses and the editor prints under the
/// label; `group` is matched against [`THEME_TOKENS`] with `==`. Both are
/// identities and stay untranslated — only `label` is prose, so it holds a
/// translation key that the editor resolves at render time.
pub struct ThemeTokenDescriptor {
    pub token: ThemeToken,
    pub key: &'static str,
    pub label: Key,
    pub group: &'static str,
}

pub const THEME_TOKENS: &[ThemeTokenDescriptor] = &[
    ThemeTokenDescriptor {
        token: ThemeToken::Bg,
        key: "bg",
        label: k::THEME_TOKEN_BG,
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Mantle,
        key: "mantle",
        label: k::THEME_TOKEN_MANTLE,
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Header,
        key: "header",
        label: k::THEME_TOKEN_HEADER,
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Surface,
        key: "surface",
        label: k::THEME_TOKEN_SURFACE,
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Overlay,
        key: "overlay",
        label: k::THEME_TOKEN_OVERLAY,
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Panel,
        key: "panel",
        label: k::THEME_TOKEN_PANEL,
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Inset,
        key: "inset",
        label: k::THEME_TOKEN_INSET,
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::SurfaceHover,
        key: "surfaceHover",
        label: k::THEME_TOKEN_SURFACEHOVER,
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Border,
        key: "border",
        label: k::THEME_TOKEN_BORDER,
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::BorderStrong,
        key: "borderStrong",
        label: k::THEME_TOKEN_BORDERSTRONG,
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Text,
        key: "text",
        label: k::THEME_TOKEN_TEXT,
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Subtext,
        key: "subtext",
        label: k::THEME_TOKEN_SUBTEXT,
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Muted,
        key: "muted",
        label: k::THEME_TOKEN_MUTED,
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::SidebarText,
        key: "sidebarText",
        label: k::THEME_TOKEN_SIDEBARTEXT,
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::SidebarMuted,
        key: "sidebarMuted",
        label: k::THEME_TOKEN_SIDEBARMUTED,
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Accent,
        key: "accent",
        label: k::THEME_TOKEN_ACCENT,
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::AccentFill,
        key: "accentFill",
        label: k::THEME_TOKEN_ACCENTFILL,
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::AccentHover,
        key: "accentHover",
        label: k::THEME_TOKEN_ACCENTHOVER,
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::AccentSoft,
        key: "accentSoft",
        label: k::THEME_TOKEN_ACCENTSOFT,
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::AccentText,
        key: "accentText",
        label: k::THEME_TOKEN_ACCENTTEXT,
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::SidebarSelected,
        key: "sidebarSelected",
        label: k::THEME_TOKEN_SIDEBARSELECTED,
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Selection,
        key: "selection",
        label: k::THEME_TOKEN_SELECTION,
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Green,
        key: "green",
        label: k::THEME_TOKEN_GREEN,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::GreenSoft,
        key: "greenSoft",
        label: k::THEME_TOKEN_GREENSOFT,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Red,
        key: "red",
        label: k::THEME_TOKEN_RED,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::RedSoft,
        key: "redSoft",
        label: k::THEME_TOKEN_REDSOFT,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::RedHover,
        key: "redHover",
        label: k::THEME_TOKEN_REDHOVER,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::ErrorSurface,
        key: "errorSurface",
        label: k::THEME_TOKEN_ERRORSURFACE,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Yellow,
        key: "yellow",
        label: k::THEME_TOKEN_YELLOW,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::YellowSoft,
        key: "yellowSoft",
        label: k::THEME_TOKEN_YELLOWSOFT,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Mauve,
        key: "mauve",
        label: k::THEME_TOKEN_MAUVE,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Teal,
        key: "teal",
        label: k::THEME_TOKEN_TEAL,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Peach,
        key: "peach",
        label: k::THEME_TOKEN_PEACH,
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Scrim,
        key: "scrim",
        label: k::THEME_TOKEN_SCRIM,
        group: "效果",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Shadow,
        key: "shadow",
        label: k::THEME_TOKEN_SHADOW,
        group: "效果",
    },
];

impl Theme {
    pub fn color(&self, token: ThemeToken) -> ThemeColor {
        match token {
            ThemeToken::Bg => self.bg,
            ThemeToken::Mantle => self.mantle,
            ThemeToken::Surface => self.surface,
            ThemeToken::Overlay => self.overlay,
            ThemeToken::SurfaceHover => self.surface_hover,
            ThemeToken::Panel => self.panel,
            ThemeToken::Inset => self.inset,
            ThemeToken::Border => self.border,
            ThemeToken::BorderStrong => self.border_strong,
            ThemeToken::Text => self.text,
            ThemeToken::Subtext => self.subtext,
            ThemeToken::Muted => self.muted,
            ThemeToken::Accent => self.accent,
            ThemeToken::AccentFill => self.accent_fill,
            ThemeToken::AccentHover => self.accent_hover,
            ThemeToken::AccentSoft => self.accent_soft,
            ThemeToken::AccentText => self.accent_text,
            ThemeToken::Green => self.green,
            ThemeToken::GreenSoft => self.green_soft,
            ThemeToken::Red => self.red,
            ThemeToken::RedSoft => self.red_soft,
            ThemeToken::RedHover => self.red_hover,
            ThemeToken::Yellow => self.yellow,
            ThemeToken::YellowSoft => self.yellow_soft,
            ThemeToken::Mauve => self.mauve,
            ThemeToken::Teal => self.teal,
            ThemeToken::Peach => self.peach,
            ThemeToken::SidebarSelected => self.sidebar_selected,
            ThemeToken::SidebarText => self.sidebar_text,
            ThemeToken::SidebarMuted => self.sidebar_muted,
            ThemeToken::Header => self.header,
            ThemeToken::Selection => self.selection,
            ThemeToken::ErrorSurface => self.error_surface,
            ThemeToken::Scrim => self.scrim,
            ThemeToken::Shadow => self.shadow,
        }
    }

    pub fn set_color(&mut self, token: ThemeToken, color: ThemeColor) {
        match token {
            ThemeToken::Bg => self.bg = color,
            ThemeToken::Mantle => self.mantle = color,
            ThemeToken::Surface => self.surface = color,
            ThemeToken::Overlay => self.overlay = color,
            ThemeToken::SurfaceHover => self.surface_hover = color,
            ThemeToken::Panel => self.panel = color,
            ThemeToken::Inset => self.inset = color,
            ThemeToken::Border => self.border = color,
            ThemeToken::BorderStrong => self.border_strong = color,
            ThemeToken::Text => self.text = color,
            ThemeToken::Subtext => self.subtext = color,
            ThemeToken::Muted => self.muted = color,
            ThemeToken::Accent => self.accent = color,
            ThemeToken::AccentFill => self.accent_fill = color,
            ThemeToken::AccentHover => self.accent_hover = color,
            ThemeToken::AccentSoft => self.accent_soft = color,
            ThemeToken::AccentText => self.accent_text = color,
            ThemeToken::Green => self.green = color,
            ThemeToken::GreenSoft => self.green_soft = color,
            ThemeToken::Red => self.red = color,
            ThemeToken::RedSoft => self.red_soft = color,
            ThemeToken::RedHover => self.red_hover = color,
            ThemeToken::Yellow => self.yellow = color,
            ThemeToken::YellowSoft => self.yellow_soft = color,
            ThemeToken::Mauve => self.mauve = color,
            ThemeToken::Teal => self.teal = color,
            ThemeToken::Peach => self.peach = color,
            ThemeToken::SidebarSelected => self.sidebar_selected = color,
            ThemeToken::SidebarText => self.sidebar_text = color,
            ThemeToken::SidebarMuted => self.sidebar_muted = color,
            ThemeToken::Header => self.header = color,
            ThemeToken::Selection => self.selection = color,
            ThemeToken::ErrorSurface => self.error_surface = color,
            ThemeToken::Scrim => self.scrim = color,
            ThemeToken::Shadow => self.shadow = color,
        }
    }
}

/// Portable theme package. Both variants are required by the schema.
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

#[derive(Clone, Debug)]
pub struct ThemeRecord {
    pub family: ThemeFamily,
    pub built_in: bool,
    pub path: Option<PathBuf>,
}

#[derive(Default)]
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
        description: "克制的暖灰中性色与清晰蓝色强调。".to_string(),
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
        description: "象牙白与暖石墨表面，配以克制的余烬橙。".to_string(),
        light: EMBER_LIGHT,
        dark: EMBER_DARK,
    }
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

fn themes_dir() -> PathBuf {
    ochub_core::paths::get_app_config_dir().join("themes")
}

fn is_theme_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.ends_with(".ochub-theme.json"))
        .unwrap_or(false)
}

fn read_theme_file(path: &Path) -> Result<ThemeFamily> {
    let metadata = fs::metadata(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    if metadata.len() > MAX_THEME_FILE_BYTES {
        return Err(anyhow!("主题文件超过 256 KiB"));
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("读取主题文件 {} 失败", path.display()))?;
    let family: ThemeFamily = serde_json::from_str(&content)
        .with_context(|| format!("解析主题文件 {} 失败", path.display()))?;
    validate_family(&family)?;
    Ok(family)
}

pub fn load_registry() -> ThemeRegistry {
    let mut registry = ThemeRegistry {
        themes: built_in_records(),
        diagnostics: Vec::new(),
    };
    let directory = themes_dir();
    if let Err(err) = fs::create_dir_all(&directory) {
        registry
            .diagnostics
            .push(format!("无法创建主题目录 {}: {err}", directory.display()));
        return registry;
    }

    let mut paths = match fs::read_dir(&directory) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_theme_file(path))
            .collect::<Vec<_>>(),
        Err(err) => {
            registry
                .diagnostics
                .push(format!("无法读取主题目录 {}: {err}", directory.display()));
            return registry;
        }
    };
    paths.sort();

    for path in paths {
        match read_theme_file(&path) {
            Ok(family) => {
                if registry
                    .themes
                    .iter()
                    .any(|record| record.family.id == family.id)
                {
                    registry.diagnostics.push(format!(
                        "主题 ID '{}' 重复，已忽略 {}",
                        family.id,
                        path.display()
                    ));
                } else {
                    registry.themes.push(ThemeRecord {
                        family,
                        built_in: false,
                        path: Some(path),
                    });
                }
            }
            Err(err) => registry.diagnostics.push(err.to_string()),
        }
    }
    registry.themes[2..].sort_by_key(|record| record.family.name.to_lowercase());
    registry
}

pub fn find_family(id: &str) -> Option<ThemeFamily> {
    load_registry()
        .themes
        .into_iter()
        .find(|record| record.family.id == id)
        .map(|record| record.family)
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
        let value = value as f32 / 255.;
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

fn validate_palette(label: &str, palette: &Theme) -> Result<()> {
    for (name, value) in [
        ("侧边栏不透明度", palette.effects.sidebar_opacity),
        ("主界面不透明度", palette.effects.content_opacity),
    ] {
        if !(MIN_SURFACE_OPACITY_PERCENT..=MAX_SURFACE_OPACITY_PERCENT).contains(&value) {
            return Err(anyhow!(
                "{label}的{name}必须在 {MIN_SURFACE_OPACITY_PERCENT}–{MAX_SURFACE_OPACITY_PERCENT}% 之间"
            ));
        }
    }

    for (pair, foreground, background) in [
        ("主文字/背景", palette.text, palette.bg),
        ("次级文字/表面", palette.subtext, palette.surface),
        ("弱文字/表面", palette.muted, palette.surface),
        ("强调文字/表面", palette.accent, palette.surface),
        (
            "强调按钮文字/填充",
            palette.accent_text,
            palette.accent_fill,
        ),
    ] {
        let ratio = contrast_ratio(foreground, background);
        if ratio < 4.5 {
            return Err(anyhow!(
                "{label} 的{pair}对比度仅 {ratio:.2}:1，至少需要 4.5:1"
            ));
        }
    }
    Ok(())
}

pub fn validate_family(family: &ThemeFamily) -> Result<()> {
    if family.schema_version != THEME_SCHEMA_VERSION {
        return Err(anyhow!(
            "不支持主题 schemaVersion {}，当前仅支持 {}",
            family.schema_version,
            THEME_SCHEMA_VERSION
        ));
    }
    if !valid_id(&family.id) {
        return Err(anyhow!(
            "主题 ID 只能包含小写字母、数字和连字符，最长 64 位"
        ));
    }
    let name = family.name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(anyhow!("主题名称不能为空且不能超过 80 个字符"));
    }
    if family.author.chars().count() > 80 {
        return Err(anyhow!("主题作者不能超过 80 个字符"));
    }
    if family.description.chars().count() > 400 {
        return Err(anyhow!("主题说明不能超过 400 个字符"));
    }
    validate_palette("浅色配色", &family.light)?;
    validate_palette("深色配色", &family.dark)?;
    Ok(())
}

fn unique_id(base: &str, registry: &ThemeRegistry) -> String {
    let candidate_for = |suffix: Option<usize>| {
        let suffix = suffix.map(|value| format!("-{value}")).unwrap_or_default();
        let stem_len = 64usize.saturating_sub(suffix.len());
        let mut stem = base.chars().take(stem_len).collect::<String>();
        while stem.ends_with('-') {
            stem.pop();
        }
        format!("{stem}{suffix}")
    };

    let mut suffix = None;
    loop {
        let candidate = candidate_for(suffix);
        if !registry
            .themes
            .iter()
            .any(|record| record.family.id == candidate)
        {
            return candidate;
        }
        suffix = Some(suffix.unwrap_or(1) + 1);
    }
}

fn write_family(path: &Path, family: &ThemeFamily) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("主题文件缺少父目录"))?;
    fs::create_dir_all(parent)?;
    let serialized = serde_json::to_string_pretty(family)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(serialized.as_bytes())?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|err| anyhow!("写入主题文件 {} 失败: {}", path.display(), err.error))?;
    Ok(())
}

pub fn save_user_family(family: &ThemeFamily) -> Result<PathBuf> {
    validate_family(family)?;
    if matches!(
        family.id.as_str(),
        DEFAULT_THEME_FAMILY | EMBER_THEME_FAMILY
    ) {
        return Err(anyhow!("内置主题不能被覆盖，请先创建副本"));
    }
    let path = themes_dir().join(format!("{}.ochub-theme.json", family.id));
    write_family(&path, family)?;
    Ok(path)
}

pub fn duplicate_family(source: &ThemeFamily) -> Result<ThemeFamily> {
    let registry = load_registry();
    let mut family = source.clone();
    family.id = unique_id(&format!("{}-copy", source.id), &registry);
    family.name = format!("{} 副本", source.name.chars().take(77).collect::<String>());
    family.author.clear();
    validate_family(&family)?;
    Ok(family)
}

pub fn import_family(path: &Path) -> Result<ThemeFamily> {
    let registry = load_registry();
    let mut family = read_theme_file(path)?;
    if registry
        .themes
        .iter()
        .any(|record| record.family.id == family.id)
    {
        family.id = unique_id(&format!("{}-imported", family.id), &registry);
        family.name = format!("{}（导入）", family.name);
    }
    save_user_family(&family)?;
    Ok(family)
}

pub fn export_family(family: &ThemeFamily, path: &Path) -> Result<()> {
    validate_family(family)?;
    write_family(path, family)
}

pub fn delete_user_family(record: &ThemeRecord) -> Result<()> {
    if record.built_in {
        return Err(anyhow!("内置主题不能删除"));
    }
    let path = record
        .path
        .as_ref()
        .ok_or_else(|| anyhow!("用户主题缺少文件路径"))?;
    fs::remove_file(path).with_context(|| format!("删除主题 {} 失败", path.display()))
}

static CURRENT: RwLock<Theme> = RwLock::new(OCHUB_LIGHT);
static CURRENT_DARK: AtomicBool = AtomicBool::new(false);
static PREVIEW_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn current() -> Theme {
    *CURRENT.read().expect("theme lock poisoned")
}

pub fn is_dark() -> bool {
    CURRENT_DARK.load(Ordering::Relaxed)
}

fn install(theme: Theme, dark: bool) {
    *CURRENT.write().expect("theme lock poisoned") = theme;
    CURRENT_DARK.store(dark, Ordering::Relaxed);
}

pub fn install_preview(theme: Theme, dark: bool) {
    PREVIEW_ACTIVE.store(true, Ordering::Relaxed);
    install(theme, dark);
}

pub fn is_previewing() -> bool {
    PREVIEW_ACTIVE.load(Ordering::Relaxed)
}

fn system_is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

pub fn install_family(family: &ThemeFamily, mode: ThemeMode, appearance: WindowAppearance) {
    let dark = match mode {
        ThemeMode::System => system_is_dark(appearance),
        ThemeMode::Light => false,
        ThemeMode::Dark => true,
    };
    install(if dark { family.dark } else { family.light }, dark);
    PREVIEW_ACTIVE.store(false, Ordering::Relaxed);
}

/// Install the requested family, falling back to OcHub when the user file is
/// missing or invalid. Returns the ID that was actually installed.
pub fn install_selected(id: &str, mode: ThemeMode, appearance: WindowAppearance) -> String {
    let family = find_family(id).unwrap_or_else(ochub_family);
    let installed_id = family.id.clone();
    install_family(&family, mode, appearance);
    installed_id
}

#[inline]
fn opacity_alpha(percent: u8) -> f32 {
    f32::from(percent) / f32::from(MAX_SURFACE_OPACITY_PERCENT)
}

/// Native window treatment for the currently installed palette.
#[inline]
pub fn window_background_appearance() -> WindowBackgroundAppearance {
    current().effects.window_background.appearance()
}

/// Keep the native window in sync after installing or previewing a palette.
#[inline]
pub fn apply_window_background(window: &Window) {
    window.set_background_appearance(window_background_appearance());
}

/// The root only supplies a fallback color for opaque windows. Blurred windows
/// must leave the root clear so translucent child surfaces can reveal the
/// platform backdrop.
#[inline]
pub fn window_base_background() -> Rgba {
    let palette = current();
    let background = palette.bg.rgba();
    match palette.effects.window_background {
        ThemeWindowBackground::Blurred => background.alpha(0.),
        ThemeWindowBackground::Opaque => background,
    }
}

#[inline]
pub fn sidebar_background() -> Rgba {
    let palette = current();
    palette
        .mantle
        .rgba()
        .alpha(opacity_alpha(palette.effects.sidebar_opacity))
}

#[inline]
pub fn content_background() -> Rgba {
    let palette = current();
    palette
        .bg
        .rgba()
        .alpha(opacity_alpha(palette.effects.content_opacity))
}

fn composite_color(foreground: ThemeColor, background: ThemeColor, alpha: f32) -> ThemeColor {
    fn channel(foreground: u32, background: u32, shift: u32, alpha: f32) -> u32 {
        let foreground = ((foreground >> shift) & 0xff) as f32;
        let background = ((background >> shift) & 0xff) as f32;
        (foreground * alpha + background * (1. - alpha)).round() as u32
    }

    let red = channel(foreground.0, background.0, 16, alpha);
    let green = channel(foreground.0, background.0, 8, alpha);
    let blue = channel(foreground.0, background.0, 0, alpha);
    ThemeColor::new((red << 16) | (green << 8) | blue)
}

fn adaptive_sidebar_foreground(
    palette: &Theme,
    preferred: ThemeColor,
    fallback: ThemeColor,
    appearance: WindowAppearance,
    minimum_contrast: f32,
) -> ThemeColor {
    let backdrops = if system_is_dark(appearance) {
        [ThemeColor::new(0x111111), ThemeColor::new(0xf7f7f7)]
    } else {
        [ThemeColor::new(0xf7f7f7), ThemeColor::new(0x111111)]
    };
    let alpha = if palette.effects.window_background == ThemeWindowBackground::Opaque {
        1.
    } else {
        opacity_alpha(palette.effects.sidebar_opacity)
    };
    let backgrounds = backdrops.map(|backdrop| composite_color(palette.mantle, backdrop, alpha));
    let minimum_ratio = |foreground| {
        backgrounds
            .iter()
            .map(|background| contrast_ratio(foreground, *background))
            .fold(f32::INFINITY, f32::min)
    };
    if minimum_ratio(preferred) >= minimum_contrast {
        return preferred;
    }

    [
        fallback,
        ThemeColor::new(0x000000),
        ThemeColor::new(0xffffff),
    ]
    .into_iter()
    .max_by(|left, right| {
        minimum_ratio(*left)
            .partial_cmp(&minimum_ratio(*right))
            .unwrap_or(std::cmp::Ordering::Equal)
    })
    .unwrap_or(preferred)
}

/// Sidebar foreground used over the native blurred backdrop. When a pinned or
/// previewed palette does not match the native window appearance, preserve the
/// configured color when possible and fall back to a readable neutral.
#[inline]
pub fn sidebar_glass_text(appearance: WindowAppearance) -> Rgba {
    let palette = current();
    adaptive_sidebar_foreground(
        &palette,
        palette.sidebar_text,
        palette.text,
        appearance,
        4.5,
    )
    .rgba()
}

#[inline]
pub fn sidebar_glass_muted(appearance: WindowAppearance) -> Rgba {
    let palette = current();
    adaptive_sidebar_foreground(
        &palette,
        palette.sidebar_muted,
        palette.sidebar_text,
        appearance,
        3.,
    )
    .rgba()
}

macro_rules! token {
    ($($name:ident),* $(,)?) => {
        $(
            #[inline]
            pub fn $name() -> Rgba {
                current().$name.rgba()
            }
        )*
    };
}

token!(
    surface,
    overlay,
    surface_hover,
    panel,
    inset,
    border,
    border_strong,
    text,
    subtext,
    muted,
    accent,
    accent_fill,
    accent_hover,
    accent_soft,
    accent_text,
    green,
    green_soft,
    red,
    red_soft,
    red_hover,
    yellow,
    yellow_soft,
    mauve,
    teal,
    peach,
    sidebar_selected,
    sidebar_text,
    selection,
    error_surface,
    scrim,
    shadow,
);

/// Convert a raw `0xRRGGBB` hex to `Rgba` for immutable brand colors.
#[inline]
pub fn c(hex: u32) -> Rgba {
    rgb(hex)
}

#[inline]
pub fn shadow_color(alpha: f32) -> Hsla {
    shadow().alpha(alpha).into()
}

pub fn shadow_panel() -> Vec<BoxShadow> {
    if is_dark() {
        Vec::new()
    } else {
        vec![BoxShadow::new(px(0.), px(1.), shadow_color(0.05)).blur_radius(px(2.))]
    }
}

pub fn shadow_hover() -> Vec<BoxShadow> {
    let alpha = if is_dark() { 0.16 } else { 0.08 };
    vec![BoxShadow::new(px(0.), px(2.), shadow_color(alpha)).blur_radius(px(6.))]
}

pub fn shadow_popover() -> Vec<BoxShadow> {
    let (wide, tight) = if is_dark() {
        (0.34, 0.20)
    } else {
        (0.14, 0.08)
    };
    vec![
        BoxShadow::new(px(0.), px(8.), shadow_color(wide)).blur_radius(px(24.)),
        BoxShadow::new(px(0.), px(2.), shadow_color(tight)).blur_radius(px(4.)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_families_are_valid_and_round_trip() {
        for family in [ochub_family(), ember_family()] {
            validate_family(&family).expect("built-in theme is valid");
            let json = serde_json::to_string(&family).expect("serialize theme");
            let decoded: ThemeFamily = serde_json::from_str(&json).expect("deserialize theme");
            assert_eq!(decoded, family);
        }
    }

    #[test]
    fn old_theme_files_receive_default_effects() {
        let mut value = serde_json::to_value(ochub_family()).expect("serialize theme value");
        for variant in ["light", "dark"] {
            value[variant]
                .as_object_mut()
                .expect("theme variant object")
                .remove("effects");
        }

        let decoded: ThemeFamily = serde_json::from_value(value).expect("deserialize legacy theme");
        assert_eq!(decoded.light.effects, ThemeEffects::DEFAULT);
        assert_eq!(decoded.dark.effects, ThemeEffects::DEFAULT);
    }

    #[test]
    fn surface_opacity_accepts_full_percentage_range() {
        let mut family = ochub_family();
        family.light.effects.sidebar_opacity = MIN_SURFACE_OPACITY_PERCENT;
        assert!(validate_family(&family).is_ok());

        family.dark.effects.content_opacity = MAX_SURFACE_OPACITY_PERCENT + 1;
        assert!(validate_family(&family).is_err());
    }

    #[test]
    fn glass_sidebar_foreground_stays_readable_across_backdrops() {
        let mut palette = OCHUB_DARK;
        palette.effects.window_background = ThemeWindowBackground::Blurred;
        palette.effects.sidebar_opacity = MIN_SURFACE_OPACITY_PERCENT;

        let dark = adaptive_sidebar_foreground(
            &palette,
            palette.sidebar_text,
            palette.text,
            WindowAppearance::Dark,
            4.5,
        );
        let light = adaptive_sidebar_foreground(
            &palette,
            palette.sidebar_text,
            palette.text,
            WindowAppearance::Light,
            4.5,
        );

        assert_eq!(dark, light);
        assert_ne!(dark, palette.sidebar_text);
    }

    #[test]
    fn sidebar_foreground_tracks_custom_opaque_background() {
        let mut palette = OCHUB_LIGHT;
        palette.effects.window_background = ThemeWindowBackground::Opaque;
        palette.mantle = ThemeColor::new(0xffffff);
        palette.sidebar_text = ThemeColor::new(0xffffff);

        let foreground = adaptive_sidebar_foreground(
            &palette,
            palette.sidebar_text,
            palette.text,
            WindowAppearance::Dark,
            4.5,
        );

        assert_ne!(foreground, palette.sidebar_text);
        assert!(contrast_ratio(foreground, palette.mantle) >= 4.5);
    }

    #[test]
    fn every_builtin_variant_meets_text_contrast() {
        for (name, palette) in [
            ("ochub light", OCHUB_LIGHT),
            ("ochub dark", OCHUB_DARK),
            ("ember light", EMBER_LIGHT),
            ("ember dark", EMBER_DARK),
        ] {
            validate_palette(name, &palette).expect("palette contrast");
        }
    }

    #[test]
    fn theme_color_requires_six_digit_hex() {
        assert_eq!(ThemeColor::parse("#A1B2C3").unwrap(), tc!(0xa1b2c3));
        assert!(ThemeColor::parse("#fff").is_err());
        assert!(ThemeColor::parse("orange").is_err());
    }

    #[test]
    fn atomic_theme_write_can_replace_an_existing_file() {
        let directory = tempfile::tempdir().expect("temporary theme directory");
        let path = directory.path().join("roundtrip.ochub-theme.json");
        let first = ochub_family();
        write_family(&path, &first).expect("initial theme write");

        let mut replacement = first.clone();
        replacement.name = "Replacement".to_string();
        write_family(&path, &replacement).expect("replacement theme write");

        let decoded = read_theme_file(&path).expect("read replaced theme");
        assert_eq!(decoded, replacement);
    }

    #[test]
    fn generated_theme_ids_remain_valid_at_the_length_limit() {
        let source_id = "a".repeat(64);
        let source = ThemeFamily {
            id: source_id.clone(),
            ..ochub_family()
        };
        let registry = ThemeRegistry {
            themes: vec![ThemeRecord {
                family: source,
                built_in: false,
                path: None,
            }],
            diagnostics: Vec::new(),
        };
        let generated = unique_id(&format!("{source_id}-copy"), &registry);
        assert!(valid_id(&generated));
        assert!(generated.len() <= 64);
    }
}
