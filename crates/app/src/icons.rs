use gpui::{prelude::*, px, svg, Rgba};

#[derive(Clone, Copy)]
pub enum IconName {
    Add,
    AgentClaude,
    AgentClaudeCode,
    AgentCodex,
    AgentGemini,
    AgentHermes,
    AgentOpenClaw,
    AgentOpenCode,
    Archive,
    Blocks,
    Chart,
    Check,
    Clock,
    Close,
    Cloud,
    Code,
    Desktop,
    Diamond,
    Folder,
    Key,
    Layers,
    Message,
    Proxy,
    Refresh,
    Settings,
    Terminal,
    Tools,
    Wrench,
}

impl IconName {
    fn path(self) -> &'static str {
        match self {
            IconName::Add => "icons/add.svg",
            IconName::AgentClaude => "icons/agents/claude.svg",
            IconName::AgentClaudeCode => "icons/agents/claude-code.svg",
            IconName::AgentCodex => "icons/agents/openai.svg",
            IconName::AgentGemini => "icons/agents/gemini.svg",
            IconName::AgentHermes => "icons/agents/hermes.svg",
            IconName::AgentOpenClaw => "icons/agents/openclaw.svg",
            IconName::AgentOpenCode => "icons/agents/opencode.svg",
            IconName::Archive => "icons/archive.svg",
            IconName::Blocks => "icons/blocks.svg",
            IconName::Chart => "icons/chart.svg",
            IconName::Check => "icons/check.svg",
            IconName::Clock => "icons/clock.svg",
            IconName::Close => "icons/close.svg",
            IconName::Cloud => "icons/cloud.svg",
            IconName::Code => "icons/code.svg",
            IconName::Desktop => "icons/desktop.svg",
            IconName::Diamond => "icons/diamond.svg",
            IconName::Folder => "icons/folder.svg",
            IconName::Key => "icons/key.svg",
            IconName::Layers => "icons/layers.svg",
            IconName::Message => "icons/message.svg",
            IconName::Proxy => "icons/proxy.svg",
            IconName::Refresh => "icons/refresh.svg",
            IconName::Settings => "icons/settings.svg",
            IconName::Terminal => "icons/terminal.svg",
            IconName::Tools => "icons/tools.svg",
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
