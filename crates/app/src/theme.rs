//! Visual design tokens for the OCHUB UI.
//!
//! Modeled on Zed's light theme: a warm "sand" neutral ramp, hairline borders,
//! near-flat surfaces (shadows reserved for popovers/modals), a restrained blue
//! accent, and muted secondary text.
//!
//! The palette lives in a runtime [`Theme`] behind a global lock so a dark
//! palette can be installed later without touching call sites; only the light
//! palette exists today. Colors are exposed as `gpui::Rgba` via small accessor
//! functions so call sites read like `theme::accent()` did before — `theme::accent()`.

use std::sync::RwLock;

use gpui::{hsla, px, rgb, BoxShadow, Hsla, Rgba};

/// A complete color palette. Values are `0xRRGGBB` hex, converted to `Rgba`
/// by the accessor functions below.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Window background (the canvas behind cards).
    pub bg: u32,
    /// Sidebar / secondary panel background.
    pub mantle: u32,
    /// Card / raised surface.
    pub surface: u32,
    /// Hovered / active surface (ghost overlay).
    pub surface_hover: u32,
    /// Quiet grouped-control panel.
    pub panel: u32,
    /// Subtle filled element background (buttons, insets).
    pub inset: u32,
    /// Hairline separators.
    pub border: u32,
    pub border_strong: u32,
    /// Primary text (warm near-black).
    pub text: u32,
    /// Secondary / muted text.
    pub subtext: u32,
    pub muted: u32,
    /// Accent (interactive / selected).
    pub accent: u32,
    pub accent_hover: u32,
    pub accent_soft: u32,
    pub accent_text: u32,
    pub green: u32,
    pub green_soft: u32,
    pub red: u32,
    pub red_soft: u32,
    pub yellow: u32,
    pub yellow_soft: u32,
    pub mauve: u32,
    pub teal: u32,
    pub peach: u32,
    pub sidebar_selected: u32,
    pub sidebar_text: u32,
    pub sidebar_muted: u32,
    pub header: u32,
}

/// The Zed-style light palette — the only one installed today.
pub const LIGHT: Theme = Theme {
    bg: 0xfcfcfb,
    mantle: 0xf4f4f2,
    surface: 0xffffff,
    surface_hover: 0xeeeeea,
    panel: 0xf8f8f6,
    inset: 0xf1f1ed,
    border: 0xe7e6e2,
    border_strong: 0xd6d5d0,
    text: 0x222019,
    subtext: 0x6b6a64,
    muted: 0x91908a,
    accent: 0x2563dd,
    accent_hover: 0x1c54c2,
    accent_soft: 0xe1ebfc,
    accent_text: 0xfbfcff,
    green: 0x2e9d5f,
    green_soft: 0xdef3e7,
    red: 0xcf493f,
    red_soft: 0xf6e1dd,
    yellow: 0xc98a1e,
    yellow_soft: 0xf5ecd4,
    mauve: 0x7a62c9,
    teal: 0x119a8f,
    peach: 0xcf6a3a,
    sidebar_selected: 0xe1ebfc,
    sidebar_text: 0x2c2a23,
    sidebar_muted: 0x84837d,
    header: 0xfbfbf9,
};

static CURRENT: RwLock<Theme> = RwLock::new(LIGHT);

/// The active palette. Reads through a lock so [`install`] can swap palettes
/// at runtime (reserved for dark mode).
pub fn current() -> Theme {
    *CURRENT.read().expect("theme lock poisoned")
}

/// Install a new palette. Reserved for dark mode; the caller is responsible
/// for triggering a full repaint afterwards.
#[allow(dead_code)]
pub fn install(theme: Theme) {
    *CURRENT.write().expect("theme lock poisoned") = theme;
}

macro_rules! token {
    ($($name:ident),* $(,)?) => {
        $(
            #[inline]
            pub fn $name() -> Rgba {
                rgb(current().$name)
            }
        )*
    };
}

/// Quiet grouped-control panel — part of the token ramp, currently unused.
#[allow(dead_code)]
#[inline]
pub fn panel() -> Rgba {
    rgb(current().panel)
}

token!(
    bg,
    mantle,
    surface,
    surface_hover,
    inset,
    border,
    border_strong,
    text,
    subtext,
    muted,
    accent,
    accent_hover,
    accent_soft,
    accent_text,
    green,
    green_soft,
    red,
    red_soft,
    yellow,
    yellow_soft,
    mauve,
    teal,
    peach,
    sidebar_selected,
    sidebar_text,
    sidebar_muted,
    header,
);

/// Convert a raw `0xRRGGBB` hex to `Rgba` — for non-token colors (brand
/// accents, one-off literals). Theme tokens should go through the accessors.
#[inline]
pub fn c(hex: u32) -> Rgba {
    rgb(hex)
}

/// Alpha-adjusted variant of a raw hex color (non-token).
#[allow(dead_code)]
#[inline]
pub fn translucent(hex: u32, alpha: f32) -> Rgba {
    rgb(hex).alpha(alpha)
}

#[inline]
pub fn shadow_color(alpha: f32) -> Hsla {
    hsla(40.0 / 360.0, 0.10, 0.18, alpha)
}

/// Near-flat elevation for cards / raised panels (Zed keeps flat surfaces flat).
#[allow(dead_code)]
pub fn shadow_panel() -> Vec<BoxShadow> {
    vec![BoxShadow::new(px(0.), px(1.), shadow_color(0.05)).blur_radius(px(2.))]
}

/// Subtle lift used on hover.
pub fn shadow_hover() -> Vec<BoxShadow> {
    vec![BoxShadow::new(px(0.), px(2.), shadow_color(0.08)).blur_radius(px(6.))]
}

/// Soft elevation for popovers / floating menus / modals.
#[allow(dead_code)]
pub fn shadow_popover() -> Vec<BoxShadow> {
    vec![
        BoxShadow::new(px(0.), px(8.), shadow_color(0.14)).blur_radius(px(24.)),
        BoxShadow::new(px(0.), px(2.), shadow_color(0.08)).blur_radius(px(4.)),
    ]
}
