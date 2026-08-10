use gpui::{Rgba, prelude::*, px, svg};

/// The icon registry — variants are allowed to be unused at any given time;
/// they exist so any view/gallery can reach for them without adding assets.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum IconName {
    Add,
    AgentClaude,
    AgentClaudeCode,
    AgentCherryStudio,
    AgentCodex,
    AgentGrokBuild,
    AgentHermes,
    AgentKimiCode,
    AgentOpenClaw,
    AgentOpenCode,
    Archive,
    Blocks,
    Calendar,
    Chart,
    Check,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    Clock,
    Close,
    Cloud,
    Code,
    Copy,
    Desktop,
    Diamond,
    DragHandle,
    Eye,
    EyeOff,
    Folder,
    Globe,
    Key,
    Layers,
    Message,
    Palette,
    Pencil,
    Refresh,
    Search,
    Settings,
    Spinner,
    Terminal,
    Tools,
    Trash,
    WindowMaximize,
    WindowMinimize,
    WindowRestore,
    Wrench,
}

impl IconName {
    fn path(self) -> &'static str {
        match self {
            IconName::Add => "icons/add.svg",
            IconName::AgentClaude => "icons/agents/claude.svg",
            IconName::AgentClaudeCode => "icons/agents/claude-code.svg",
            IconName::AgentCherryStudio => "icons/agents/cherry-studio.svg",
            IconName::AgentCodex => "icons/agents/openai.svg",
            IconName::AgentGrokBuild => "icons/agents/grok.svg",
            IconName::AgentHermes => "icons/agents/hermes.svg",
            IconName::AgentKimiCode => "icons/agents/kimi-code.svg",
            IconName::AgentOpenClaw => "icons/agents/openclaw.svg",
            IconName::AgentOpenCode => "icons/agents/opencode.svg",
            IconName::Archive => "icons/archive.svg",
            IconName::Blocks => "icons/blocks.svg",
            IconName::Calendar => "icons/calendar.svg",
            IconName::Chart => "icons/chart.svg",
            IconName::Check => "icons/check.svg",
            IconName::ChevronDown => "icons/chevron-down.svg",
            IconName::ChevronLeft => "icons/chevron-left.svg",
            IconName::ChevronRight => "icons/chevron-right.svg",
            IconName::Clock => "icons/clock.svg",
            IconName::Close => "icons/close.svg",
            IconName::Cloud => "icons/cloud.svg",
            IconName::Code => "icons/code.svg",
            IconName::Copy => "icons/copy.svg",
            IconName::Desktop => "icons/desktop.svg",
            IconName::Diamond => "icons/diamond.svg",
            IconName::DragHandle => "icons/drag-handle.svg",
            IconName::Eye => "icons/eye.svg",
            IconName::EyeOff => "icons/eye-off.svg",
            IconName::Folder => "icons/folder.svg",
            IconName::Globe => "icons/globe.svg",
            IconName::Key => "icons/key.svg",
            IconName::Layers => "icons/layers.svg",
            IconName::Message => "icons/message.svg",
            IconName::Palette => "icons/palette.svg",
            IconName::Pencil => "icons/pencil.svg",
            IconName::Refresh => "icons/refresh.svg",
            IconName::Search => "icons/search.svg",
            IconName::Settings => "icons/settings.svg",
            IconName::Spinner => "icons/spinner.svg",
            IconName::Terminal => "icons/terminal.svg",
            IconName::Tools => "icons/tools.svg",
            IconName::Trash => "icons/trash.svg",
            IconName::WindowMaximize => "icons/window-maximize.svg",
            IconName::WindowMinimize => "icons/window-minimize.svg",
            IconName::WindowRestore => "icons/window-restore.svg",
            IconName::Wrench => "icons/wrench.svg",
        }
    }
}

/// Returns the concrete `Svg` rather than `impl IntoElement` so callers can
/// still reach element-specific builders — notably `with_transformation`, the
/// only route to rotation in GPUI, which applies to sprites and not to `Div`.
pub fn icon(name: IconName, color: Rgba, size: f32) -> gpui::Svg {
    svg()
        .path(name.path())
        .w(px(size))
        .h(px(size))
        .text_color(color)
}
