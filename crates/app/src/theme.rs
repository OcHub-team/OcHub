//! Visual design tokens for the RouteDeck UI.
//!
//! Modeled on Zed's light theme: a warm "sand" neutral ramp, hairline borders,
//! near-flat surfaces (shadows reserved for popovers/modals), a restrained blue
//! accent, and muted secondary text. Colors are exposed as `gpui::Rgba` via small
//! const helpers so call sites read like `theme::ACCENT`.

use gpui::{hsla, px, rgb, BoxShadow, Hsla, Rgba};

/// Window background (the canvas behind cards).
pub const BG: u32 = 0xfcfcfb;
/// Sidebar / secondary panel background.
pub const MANTLE: u32 = 0xf4f4f2;
/// Card / raised surface.
pub const SURFACE: u32 = 0xffffff;
/// Hovered / active surface (ghost overlay).
pub const SURFACE_HOVER: u32 = 0xeeeeea;
/// Quiet grouped-control panel.
#[allow(dead_code)]
pub const PANEL: u32 = 0xf8f8f6;
/// Subtle filled element background (buttons, insets).
pub const INSET: u32 = 0xf1f1ed;
/// Hairline separators.
pub const BORDER: u32 = 0xe7e6e2;
pub const BORDER_STRONG: u32 = 0xd6d5d0;

/// Primary text (warm near-black).
pub const TEXT: u32 = 0x222019;
/// Secondary / muted text.
pub const SUBTEXT: u32 = 0x6b6a64;
pub const MUTED: u32 = 0x91908a;

/// Accent (interactive / selected).
pub const ACCENT: u32 = 0x2563dd;
pub const ACCENT_HOVER: u32 = 0x1c54c2;
pub const ACCENT_SOFT: u32 = 0xe1ebfc;
pub const ACCENT_TEXT: u32 = 0xfbfcff;
pub const GREEN: u32 = 0x2e9d5f;
pub const GREEN_SOFT: u32 = 0xdef3e7;
pub const RED: u32 = 0xcf493f;
pub const RED_SOFT: u32 = 0xf6e1dd;
pub const YELLOW: u32 = 0xc98a1e;
pub const YELLOW_SOFT: u32 = 0xf5ecd4;
pub const MAUVE: u32 = 0x7a62c9;
pub const TEAL: u32 = 0x119a8f;
pub const PEACH: u32 = 0xcf6a3a;
pub const SIDEBAR_SELECTED: u32 = 0xe1ebfc;
pub const SIDEBAR_TEXT: u32 = 0x2c2a23;
pub const SIDEBAR_MUTED: u32 = 0x84837d;
pub const HEADER: u32 = 0xfbfbf9;

/// Authentic per-app brand colors for the sidebar app-switcher icons.
/// Each is the managed tool's official identity color (a white glyph sits on top),
/// sourced from the upstream brand assets rather than the generic accent palette.
pub const BRAND_CLAUDE: u32 = 0xd97757; // Anthropic coral
pub const BRAND_CLAUDE_DESKTOP: u32 = 0xbd5d3a; // deeper Anthropic terracotta (distinct from the CLI)
pub const BRAND_CODEX: u32 = 0x0d0d0d; // OpenAI near-black
pub const BRAND_GEMINI: u32 = 0x4285f4; // Google blue
pub const BRAND_OPENCODE: u32 = 0x211e1e; // OpenCode charcoal
pub const BRAND_OPENCLAW: u32 = 0xe23b3b; // OpenClaw red
pub const BRAND_HERMES: u32 = 0x2b2b33; // Hermes slate (monochrome brand, kept apart from the other darks)

#[inline]
pub fn c(hex: u32) -> Rgba {
    rgb(hex)
}

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
