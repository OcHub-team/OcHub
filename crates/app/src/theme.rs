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
use gpui::{px, rgb, BoxShadow, Hsla, Rgba, WindowAppearance};
use ochub_core::settings::ThemeMode;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tempfile::NamedTempFile;

pub const THEME_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_THEME_FAMILY: &str = "ochub";
pub const EMBER_THEME_FAMILY: &str = "ember";
const MAX_THEME_FILE_BYTES: u64 = 256 * 1024;

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
            return Err(anyhow!("颜色必须使用 #RRGGBB 格式"));
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

/// Complete semantic color palette consumed by every shared component.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Theme {
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

pub struct ThemeTokenDescriptor {
    pub token: ThemeToken,
    pub key: &'static str,
    pub label: &'static str,
    pub group: &'static str,
}

pub const THEME_TOKENS: &[ThemeTokenDescriptor] = &[
    ThemeTokenDescriptor {
        token: ThemeToken::Bg,
        key: "bg",
        label: "窗口背景",
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Mantle,
        key: "mantle",
        label: "侧栏背景",
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Header,
        key: "header",
        label: "标题栏",
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Surface,
        key: "surface",
        label: "卡片表面",
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Overlay,
        key: "overlay",
        label: "浮层表面",
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Panel,
        key: "panel",
        label: "分组面板",
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Inset,
        key: "inset",
        label: "内凹控件",
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::SurfaceHover,
        key: "surfaceHover",
        label: "悬停表面",
        group: "表面",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Border,
        key: "border",
        label: "普通边框",
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::BorderStrong,
        key: "borderStrong",
        label: "强调边框",
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Text,
        key: "text",
        label: "主文字",
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Subtext,
        key: "subtext",
        label: "次级文字",
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Muted,
        key: "muted",
        label: "弱文字",
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::SidebarText,
        key: "sidebarText",
        label: "侧栏文字",
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::SidebarMuted,
        key: "sidebarMuted",
        label: "侧栏弱文字",
        group: "文字与边框",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Accent,
        key: "accent",
        label: "强调前景",
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::AccentFill,
        key: "accentFill",
        label: "强调填充",
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::AccentHover,
        key: "accentHover",
        label: "强调悬停",
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::AccentSoft,
        key: "accentSoft",
        label: "强调柔和背景",
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::AccentText,
        key: "accentText",
        label: "强调填充文字",
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::SidebarSelected,
        key: "sidebarSelected",
        label: "侧栏选中",
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Selection,
        key: "selection",
        label: "文本选区",
        group: "强调与选中",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Green,
        key: "green",
        label: "成功前景",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::GreenSoft,
        key: "greenSoft",
        label: "成功背景",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Red,
        key: "red",
        label: "危险前景",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::RedSoft,
        key: "redSoft",
        label: "危险背景",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::RedHover,
        key: "redHover",
        label: "危险悬停",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::ErrorSurface,
        key: "errorSurface",
        label: "错误详情背景",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Yellow,
        key: "yellow",
        label: "警告前景",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::YellowSoft,
        key: "yellowSoft",
        label: "警告背景",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Mauve,
        key: "mauve",
        label: "紫色数据",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Teal,
        key: "teal",
        label: "青色数据",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Peach,
        key: "peach",
        label: "橙色数据",
        group: "状态",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Scrim,
        key: "scrim",
        label: "模态遮罩",
        group: "效果",
    },
    ThemeTokenDescriptor {
        token: ThemeToken::Shadow,
        key: "shadow",
        label: "投影颜色",
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
        name: "OCHUB".to_string(),
        author: "OCHUB".to_string(),
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
        author: "OCHUB".to_string(),
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

pub fn current() -> Theme {
    *CURRENT.read().expect("theme lock poisoned")
}

pub fn is_dark() -> bool {
    CURRENT_DARK.load(Ordering::Relaxed)
}

pub fn install(theme: Theme, dark: bool) {
    *CURRENT.write().expect("theme lock poisoned") = theme;
    CURRENT_DARK.store(dark, Ordering::Relaxed);
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
}

/// Install the requested family, falling back to OCHUB when the user file is
/// missing or invalid. Returns the ID that was actually installed.
pub fn install_selected(id: &str, mode: ThemeMode, appearance: WindowAppearance) -> String {
    let family = find_family(id).unwrap_or_else(ochub_family);
    let installed_id = family.id.clone();
    install_family(&family, mode, appearance);
    installed_id
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
    bg,
    mantle,
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
    sidebar_muted,
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
