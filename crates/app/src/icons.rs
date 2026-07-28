use gpui::{Rgba, prelude::*, px, svg};

/// The icon registry — variants are allowed to be unused at any given time;
/// they exist so any view/gallery can reach for them without adding assets.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum IconName {
    Add,
    AgentClaude,
    AgentClaudeCode,
    AgentCodex,
    AgentGrokBuild,
    AgentHermes,
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
    Folder,
    Key,
    Layers,
    Message,
    Palette,
    Pencil,
    Refresh,
    Search,
    Settings,
    Terminal,
    Tools,
    Trash,
    Wrench,
}

impl IconName {
    fn path(self) -> &'static str {
        match self {
            IconName::Add => "icons/add.svg",
            IconName::AgentClaude => "icons/agents/claude.svg",
            IconName::AgentClaudeCode => "icons/agents/claude-code.svg",
            IconName::AgentCodex => "icons/agents/openai.svg",
            IconName::AgentGrokBuild => "icons/agents/grok.svg",
            IconName::AgentHermes => "icons/agents/hermes.svg",
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
            IconName::Folder => "icons/folder.svg",
            IconName::Key => "icons/key.svg",
            IconName::Layers => "icons/layers.svg",
            IconName::Message => "icons/message.svg",
            IconName::Palette => "icons/palette.svg",
            IconName::Pencil => "icons/pencil.svg",
            IconName::Refresh => "icons/refresh.svg",
            IconName::Search => "icons/search.svg",
            IconName::Settings => "icons/settings.svg",
            IconName::Terminal => "icons/terminal.svg",
            IconName::Tools => "icons/tools.svg",
            IconName::Trash => "icons/trash.svg",
            IconName::Wrench => "icons/wrench.svg",
        }
    }
}

pub fn icon(name: IconName, color: Rgba, size: f32) -> impl IntoElement {
    svg()
        .path(name.path())
        .w(px(size))
        .h(px(size))
        .text_color(color)
}
