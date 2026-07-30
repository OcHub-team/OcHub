use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputMode {
    Human,
    Json,
    Jsonl,
}

#[derive(Debug, Parser)]
#[command(
    name = "ochcli",
    version,
    about = "Headless command-line interface for OcHub",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Output format.
    #[arg(
        long,
        global = true,
        value_enum,
        env = "OCHUB_OUTPUT",
        default_value = "human"
    )]
    pub output: OutputMode,

    /// Shortcut for --output json.
    #[arg(long, global = true)]
    pub json: bool,

    /// Only print the primary result.
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Disable ANSI color.
    #[arg(long, global = true, env = "OCHUB_NO_COLOR")]
    pub no_color: bool,

    /// Never prompt or read a choice from the terminal.
    #[arg(long, global = true)]
    pub non_interactive: bool,

    /// Confirm the exact destructive targets shown by the command.
    #[arg(long, global = true)]
    pub yes: bool,

    /// Build and print a plan without applying mutations.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Do not perform network requests.
    #[arg(long, global = true)]
    pub offline: bool,

    /// Include secrets in commands that explicitly support it.
    #[arg(long, global = true)]
    pub show_secrets: bool,

    /// Use an alternate OcHub data directory for this process.
    #[arg(long, global = true, env = "OCHUB_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Bypass owner RPC and fail if another process owns the data directory.
    #[arg(long, global = true)]
    pub direct: bool,

    /// Runtime request timeout; accepts ms, s, m, or h suffixes.
    #[arg(
        long,
        global = true,
        default_value = "30s",
        value_parser = parse_timeout_seconds
    )]
    pub timeout: u64,

    /// Override the local runtime socket for diagnostics and tests.
    #[arg(long, global = true, env = "OCHUB_SOCKET")]
    pub socket: Option<PathBuf>,

    /// Override the language used for human-readable messages.
    #[arg(
        long,
        global = true,
        env = "OCHUB_LANG",
        value_parser = parse_language
    )]
    pub lang: Option<String>,

    /// Use a caller-provided request/trace identifier in structured output.
    #[arg(long, global = true, value_parser = parse_trace_id)]
    pub trace_id: Option<String>,

    /// Resume a plan already recorded by the Remote Nodes bridge.
    #[arg(long, global = true, hide = true, value_parser = parse_operation_id)]
    pub remote_operation_id: Option<String>,

    /// Increase diagnostic logging.
    #[arg(short = 'v', long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

fn parse_timeout_seconds(raw: &str) -> Result<u64, String> {
    let duration = parse_duration(raw)?;
    if duration.is_zero() {
        return Err("timeout must be greater than zero".to_string());
    }
    Ok(duration.as_secs().max(1))
}

fn parse_language(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    ochub_core::i18n::Locale::from_tag(value)
        .map(|_| value.to_string())
        .ok_or_else(|| format!("unsupported language {raw}; use en, zh-CN, zh-TW, or ja"))
}

fn parse_trace_id(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() || value.len() > 128 {
        return Err("trace id must contain 1 to 128 ASCII characters".to_string());
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
    }) {
        return Err(
            "trace id may contain only letters, digits, dash, underscore, dot, colon, and slash"
                .to_string(),
        );
    }
    Ok(value.to_string())
}

fn parse_operation_id(raw: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(raw)
        .map(|id| id.to_string())
        .map_err(|_| "remote operation id must be a UUID".to_string())
}

fn parse_duration(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let split = raw
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(raw.len());
    let (number, unit) = raw.split_at(split);
    let value = number
        .parse::<f64>()
        .map_err(|_| format!("invalid duration: {raw}"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid duration: {raw}"));
    }
    let seconds = match unit {
        "" | "s" => value,
        "ms" => value / 1_000.0,
        "m" => value * 60.0,
        "h" => value * 3_600.0,
        _ => {
            return Err(format!(
                "unsupported duration unit in {raw}; use ms, s, m, or h"
            ));
        }
    };
    Ok(Duration::from_secs_f64(seconds))
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the CLI and core version.
    Version,
    /// Show local OcHub and Gateway status.
    Status,
    /// Run local integrity, dependency, and configuration diagnostics.
    Doctor {
        /// Also test configured cloud backends over the network.
        #[arg(long)]
        network: bool,
    },
    /// Print resolved OcHub paths.
    Paths,
    /// Inspect or change process-scoped runtime behavior.
    Runtime(RuntimeArgs),
    /// Manage the desktop application's login item.
    Desktop(DesktopArgs),
    /// Inspect and resolve interrupted mutation records.
    Operation(OperationArgs),
    /// Manage registered applications.
    App(AppArgs),
    /// Manage manifest-driven application plugins.
    Plugin(PluginArgs),
    /// Read and update device settings.
    Settings(SettingsArgs),
    /// Manage shared application configuration snippets.
    Config(ConfigArgs),
    /// Build a declarative configuration plan.
    Plan(DeclarativePlanArgs),
    /// Apply a declarative configuration file.
    Apply(DeclarativeApplyArgs),
    /// Manage direct providers.
    Provider(ProviderArgs),
    /// Manage OAuth accounts and provider bindings.
    Auth(AuthArgs),
    /// Query subscription and coding-plan quotas.
    Quota(QuotaArgs),
    /// Manage Claude Desktop-specific connections.
    ClaudeDesktop(ClaudeDesktopArgs),
    /// Inspect and clean conflicting shell environment variables.
    Env(EnvArgs),
    /// Manage Claude Code-specific integration features.
    Claude(ClaudeArgs),
    /// Manage Codex-specific history features.
    Codex(CodexArgs),
    /// Manage OpenCode extension integrations.
    Opencode(OpencodeArgs),
    /// Manage OpenClaw advanced configuration.
    Openclaw(OpenclawArgs),
    /// Manage Hermes models and memory.
    Hermes(HermesArgs),
    /// Inspect, import, export, and select themes.
    Theme(ThemeArgs),
    /// Parse and import ochub:// links.
    Deeplink(DeeplinkArgs),
    /// Inspect, check, and install OcHub updates.
    Update(UpdateArgs),
    /// Manage MCP servers shared across supported applications.
    Mcp(McpArgs),
    /// Discover, install, and synchronize agent Skills.
    Skill(SkillArgs),
    /// Inspect and manage local coding sessions.
    Session(SessionArgs),
    /// Probe and manage supported external CLI tools.
    Tool(ToolArgs),
    /// Query locally recorded token usage and estimated cost.
    Usage(UsageArgs),
    /// Inspect and maintain the local model pricing catalog.
    Pricing(PricingArgs),
    /// Configure and run cloud snapshot synchronization.
    Sync(SyncArgs),
    /// Persist the OcHub data-directory override.
    DataDir(DataDirArgs),
    /// Explicitly migrate data from supported legacy products.
    Migrate(MigrateArgs),
    /// Manage database snapshots and SQL exports.
    Backup(BackupArgs),
    /// Control the in-process relay Gateway.
    Gateway(GatewayArgs),
    /// Manage user-facing relay stations.
    Station(StationArgs),
    /// Control this OcHub node over an authenticated SSH stdio channel.
    Remote(RemoteArgs),
    /// Run and manage the local OcHub daemon.
    Daemon(DaemonArgs),
    /// Generate shell completion.
    Completion(CompletionArgs),
    /// Generate a manual page.
    Man(ManArgs),
}

#[derive(Debug, Args)]
pub struct RuntimeArgs {
    #[command(subcommand)]
    pub command: RuntimeCommand,
}

#[derive(Debug, Subcommand)]
pub enum RuntimeCommand {
    Portable,
    Lightweight {
        #[command(subcommand)]
        command: LightweightCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum LightweightCommand {
    Status,
    Enter,
    Exit,
}

#[derive(Debug, Args)]
pub struct DesktopArgs {
    #[command(subcommand)]
    pub command: DesktopCommand,
}

#[derive(Debug, Subcommand)]
pub enum DesktopCommand {
    Autostart {
        #[command(subcommand)]
        command: DesktopAutostartCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DesktopAutostartCommand {
    Status,
    Enable,
    Disable,
}

#[derive(Debug, Args)]
pub struct OperationArgs {
    #[command(subcommand)]
    pub command: OperationCommand,
}

#[derive(Debug, Subcommand)]
pub enum OperationCommand {
    List,
    Inspect { id: String },
    Recover { id: String },
    Rollback { id: String },
}

#[derive(Debug, Args)]
pub struct AppArgs {
    #[command(subcommand)]
    pub command: AppCommand,
}

#[derive(Debug, Subcommand)]
pub enum AppCommand {
    List,
    Show {
        app: String,
    },
    Enable {
        app: String,
    },
    Disable {
        app: String,
    },
    Status {
        app: String,
    },
    Schema {
        app: String,
        #[arg(long, default_value = "provider")]
        resource: String,
    },
    Path {
        #[command(subcommand)]
        command: AppPathCommand,
    },
}

#[derive(Debug, Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    List,
    Show {
        app: String,
    },
    Validate {
        file: PathBuf,
    },
    Install {
        file: PathBuf,
    },
    Reload,
    Errors,
    Remove {
        app: String,
        #[arg(long)]
        purge_data: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum AppPathCommand {
    Get { app: String },
    Set { app: String, path: PathBuf },
    Reset { app: String },
}

#[derive(Debug, Args)]
pub struct SettingsArgs {
    #[command(subcommand)]
    pub command: SettingsCommand,
}

#[derive(Debug, Subcommand)]
pub enum SettingsCommand {
    List,
    Get {
        path: String,
    },
    Set {
        path: String,
        value: String,
        /// Always treat VALUE as a string instead of parsing JSON scalars.
        #[arg(long)]
        string: bool,
    },
    Unset {
        path: String,
    },
    Export {
        #[arg(long)]
        to: Option<PathBuf>,
    },
    Import {
        file: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub command: ProviderCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    List {
        #[arg(long)]
        app: String,
    },
    Show {
        id: String,
        #[arg(long)]
        app: String,
    },
    Current {
        #[arg(long)]
        app: String,
    },
    Add {
        #[arg(long)]
        app: String,
        #[arg(long)]
        from: Option<PathBuf>,
        /// Set a dotted Provider field using FIELD=JSON_OR_STRING.
        #[arg(long = "set")]
        set_values: Vec<String>,
        /// Read a secret from a file using FIELD=@PATH.
        #[arg(long = "secret")]
        secret_values: Vec<String>,
        #[arg(long)]
        add_to_live: bool,
    },
    Edit {
        id: String,
        #[arg(long)]
        app: String,
        #[arg(long)]
        from: Option<PathBuf>,
        /// Set a dotted Provider field using FIELD=JSON_OR_STRING.
        #[arg(long = "set")]
        set_values: Vec<String>,
        /// Read a secret from a file using FIELD=@PATH.
        #[arg(long = "secret")]
        secret_values: Vec<String>,
    },
    Delete {
        id: String,
        #[arg(long)]
        app: String,
    },
    Export {
        id: String,
        #[arg(long)]
        app: String,
        #[arg(long)]
        to: Option<PathBuf>,
    },
    SeedOfficial {
        #[arg(long)]
        app: String,
    },
    ImportLive {
        #[arg(long)]
        app: String,
    },
    SyncLive {
        #[arg(long, conflicts_with = "all")]
        app: Option<String>,
        #[arg(long)]
        all: bool,
    },
    Preview {
        id: String,
        #[arg(long)]
        app: String,
    },
    Switch {
        id: String,
        #[arg(long)]
        app: String,
        #[arg(long, value_enum, default_value = "abort")]
        on_drift: DriftPolicyArg,
    },
    AddToLive {
        id: String,
        #[arg(long)]
        app: String,
    },
    RemoveFromLive {
        id: String,
        #[arg(long)]
        app: String,
    },
    Sort {
        #[arg(long)]
        app: String,
        ids: Vec<String>,
    },
    Copy {
        id: String,
        #[arg(long)]
        from_app: String,
        #[arg(long)]
        to_app: String,
    },
    Test {
        id: String,
        #[arg(long)]
        app: String,
    },
    SpeedTest {
        id: String,
        #[arg(long)]
        app: String,
    },
    Models {
        id: String,
        #[arg(long)]
        app: String,
    },
    Balance {
        id: String,
        #[arg(long)]
        app: String,
    },
    Quota {
        id: String,
        #[arg(long)]
        app: String,
    },
    UsageScript {
        #[command(subcommand)]
        command: ProviderUsageScriptCommand,
    },
    Terminal {
        id: String,
        #[arg(long)]
        app: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    Endpoint {
        #[command(subcommand)]
        command: ProviderEndpointCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProviderUsageScriptCommand {
    Run {
        id: String,
        #[arg(long)]
        app: String,
    },
    Test {
        #[arg(long)]
        app: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        from: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProviderEndpointCommand {
    List {
        id: String,
        #[arg(long)]
        app: String,
    },
    Add {
        id: String,
        url: String,
        #[arg(long)]
        app: String,
    },
    Remove {
        id: String,
        url: String,
        #[arg(long)]
        app: String,
    },
}

#[derive(Debug, Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    Copilot {
        #[command(subcommand)]
        command: CopilotAuthCommand,
    },
    Codex {
        #[command(subcommand)]
        command: CodexAuthCommand,
    },
    Binding {
        #[command(subcommand)]
        command: AuthBindingCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum CopilotAuthCommand {
    Status,
    Login {
        #[arg(long)]
        github_domain: Option<String>,
    },
    Poll {
        flow_id: String,
        #[arg(long)]
        github_domain: Option<String>,
    },
    Account {
        #[command(subcommand)]
        command: AuthAccountCommand,
    },
    Token {
        #[arg(long)]
        account: Option<String>,
    },
    Models {
        #[arg(long)]
        account: Option<String>,
    },
    Usage {
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum CodexAuthCommand {
    Status,
    Login,
    Poll {
        flow_id: String,
    },
    Logout {
        #[arg(long)]
        account: Option<String>,
    },
    Account {
        #[command(subcommand)]
        command: AuthAccountCommand,
    },
    Models {
        #[arg(long)]
        account: Option<String>,
    },
    Quota {
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthAccountCommand {
    List,
    SetDefault { id: String },
    Remove { id: String },
}

#[derive(Debug, Subcommand)]
pub enum AuthBindingCommand {
    List,
    Set {
        #[arg(long)]
        app: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        account: String,
    },
    Remove {
        #[arg(long)]
        app: String,
        #[arg(long)]
        provider: String,
    },
}

#[derive(Debug, Args)]
pub struct QuotaArgs {
    #[command(subcommand)]
    pub command: QuotaCommand,
}

#[derive(Debug, Subcommand)]
pub enum QuotaCommand {
    Subscription {
        provider_id: String,
        #[arg(long)]
        app: String,
    },
    CodingPlan {
        provider_id: String,
        #[arg(long)]
        app: String,
    },
}

#[derive(Debug, Args)]
pub struct ClaudeDesktopArgs {
    #[command(subcommand)]
    pub command: ClaudeDesktopCommand,
}

#[derive(Debug, Subcommand)]
pub enum ClaudeDesktopCommand {
    Status,
    EnsureOfficial,
    ImportFromClaude,
}

#[derive(Debug, Args)]
pub struct EnvArgs {
    #[command(subcommand)]
    pub command: EnvCommand,
}

#[derive(Debug, Subcommand)]
pub enum EnvCommand {
    Scan,
    Clean { conflict_id: String },
    Restore { backup_id: String },
}

#[derive(Debug, Args)]
pub struct ClaudeArgs {
    #[command(subcommand)]
    pub command: ClaudeCommand,
}

#[derive(Debug, Subcommand)]
pub enum ClaudeCommand {
    Plugin {
        #[command(subcommand)]
        command: ClaudePluginCommand,
    },
    Mcp {
        #[command(subcommand)]
        command: ClaudeMcpCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ClaudePluginCommand {
    Status,
    Show,
    Apply {
        #[arg(long)]
        from: PathBuf,
    },
    Restore,
}

#[derive(Debug, Subcommand)]
pub enum ClaudeMcpCommand {
    Status,
    Config {
        #[command(subcommand)]
        command: ClaudeMcpConfigCommand,
    },
    Server {
        #[command(subcommand)]
        command: ClaudeMcpServerCommand,
    },
    Path {
        #[command(subcommand)]
        command: ClaudeMcpPathCommand,
    },
    Onboarding {
        #[command(subcommand)]
        command: ClaudeOnboardingCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ClaudeMcpConfigCommand {
    Show,
}

#[derive(Debug, Subcommand)]
pub enum ClaudeMcpServerCommand {
    Upsert {
        id: String,
        #[arg(long)]
        from: PathBuf,
    },
    Delete {
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ClaudeMcpPathCommand {
    Validate,
}

#[derive(Debug, Subcommand)]
pub enum ClaudeOnboardingCommand {
    Status,
    Skip,
    Clear,
}

#[derive(Debug, Args)]
pub struct CodexArgs {
    #[command(subcommand)]
    pub command: CodexCommand,
}

#[derive(Debug, Subcommand)]
pub enum CodexCommand {
    History {
        #[command(subcommand)]
        command: CodexHistoryCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum CodexHistoryCommand {
    Status,
    Migrate,
    Restore,
}

#[derive(Debug, Args)]
pub struct OpencodeArgs {
    #[command(subcommand)]
    pub command: OpencodeCommand,
}

#[derive(Debug, Subcommand)]
pub enum OpencodeCommand {
    Omo {
        #[command(subcommand)]
        command: OmoCommand,
    },
    OmoSlim {
        #[command(subcommand)]
        command: OmoCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum OmoCommand {
    Status,
    Current,
    LocalFile,
    Disable,
}

#[derive(Debug, Args)]
pub struct OpenclawArgs {
    #[command(subcommand)]
    pub command: OpenclawCommand,
}

#[derive(Debug, Subcommand)]
pub enum OpenclawCommand {
    Health,
    Model {
        #[command(subcommand)]
        command: OpenclawModelCommand,
    },
    Models,
    AgentDefaults {
        #[command(subcommand)]
        command: GetSetCommand,
    },
    Env {
        #[command(subcommand)]
        command: GetSetCommand,
    },
    Tools {
        #[command(subcommand)]
        command: GetSetCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum OpenclawModelCommand {
    Default {
        #[command(subcommand)]
        command: GetSetCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum GetSetCommand {
    Get,
    Set {
        #[arg(long)]
        from: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct HermesArgs {
    #[command(subcommand)]
    pub command: HermesCommand,
}

#[derive(Debug, Subcommand)]
pub enum HermesCommand {
    Models {
        #[command(subcommand)]
        command: GetSetCommand,
    },
    Memory {
        #[command(subcommand)]
        command: HermesMemoryCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum HermesMemoryCommand {
    Status,
    Limits,
    Read {
        kind: MemoryKindArg,
    },
    Write {
        kind: MemoryKindArg,
        #[arg(long)]
        from: PathBuf,
    },
    Enable {
        kind: MemoryKindArg,
    },
    Disable {
        kind: MemoryKindArg,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum MemoryKindArg {
    Memory,
    User,
}

impl MemoryKindArg {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Args)]
pub struct ThemeArgs {
    #[command(subcommand)]
    pub command: ThemeCommand,
}

#[derive(Debug, Subcommand)]
pub enum ThemeCommand {
    List,
    Show {
        id: String,
    },
    Validate {
        file: PathBuf,
    },
    Import {
        file: PathBuf,
    },
    Export {
        id: String,
        /// Write the portable theme document to this path.
        #[arg(long)]
        to: Option<PathBuf>,
    },
    Duplicate {
        id: String,
    },
    Delete {
        id: String,
    },
    Set {
        id: String,
    },
    Mode {
        #[arg(value_enum)]
        mode: ThemeModeArg,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ThemeModeArg {
    System,
    Light,
    Dark,
}

impl From<ThemeModeArg> for ochub_core::settings::ThemeMode {
    fn from(value: ThemeModeArg) -> Self {
        match value {
            ThemeModeArg::System => Self::System,
            ThemeModeArg::Light => Self::Light,
            ThemeModeArg::Dark => Self::Dark,
        }
    }
}

#[derive(Debug, Args)]
pub struct DeeplinkArgs {
    #[command(subcommand)]
    pub command: DeeplinkCommand,
}

#[derive(Debug, Subcommand)]
pub enum DeeplinkCommand {
    Parse { uri: String },
    Import { uri: String },
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    #[command(subcommand)]
    pub command: UpdateCommand,
}

#[derive(Debug, Subcommand)]
pub enum UpdateCommand {
    Status,
    Check,
    Install,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Validate {
        #[arg(short = 'f', long)]
        file: PathBuf,
    },
    Common {
        #[command(subcommand)]
        command: CommonConfigCommand,
    },
}

#[derive(Debug, Args)]
pub struct DeclarativePlanArgs {
    #[arg(short = 'f', long)]
    pub file: PathBuf,
    /// Explicitly take ownership from another declarative manager.
    #[arg(long)]
    pub adopt: bool,
    /// Plan deletion of resources previously managed by this file but now absent.
    #[arg(long)]
    pub prune: bool,
}

#[derive(Debug, Args)]
pub struct DeclarativeApplyArgs {
    #[arg(short = 'f', long)]
    pub file: PathBuf,
    /// Explicitly take ownership from another declarative manager.
    #[arg(long)]
    pub adopt: bool,
    /// Delete resources previously managed by this file but now absent.
    #[arg(long)]
    pub prune: bool,
}

#[derive(Debug, Subcommand)]
pub enum CommonConfigCommand {
    Get {
        #[arg(long)]
        app: String,
    },
    Set {
        #[arg(long)]
        app: String,
        #[arg(long)]
        from: PathBuf,
    },
    Extract {
        #[arg(long)]
        app: String,
    },
    Apply {
        #[arg(long)]
        app: String,
        #[arg(long)]
        provider: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DriftPolicyArg {
    Abort,
    Preserve,
    Discard,
}

#[derive(Debug, Args)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommand,
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    List,
    Show {
        id: String,
    },
    Add {
        #[arg(long)]
        from: PathBuf,
    },
    Edit {
        id: String,
        #[arg(long)]
        from: PathBuf,
    },
    Delete {
        id: String,
    },
    Validate {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        from: Option<PathBuf>,
    },
    Import {
        #[arg(long)]
        app: String,
    },
    Enable {
        id: String,
        #[arg(long)]
        app: String,
    },
    Disable {
        id: String,
        #[arg(long)]
        app: String,
    },
    Sync {
        id: String,
        /// Application to synchronize. Repeat for multiple apps.
        #[arg(long)]
        app: Vec<String>,
    },
    SyncAll,
}

#[derive(Debug, Args)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub command: SkillCommand,
}

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// List installed skills without performing network discovery.
    List,
    Show {
        id: String,
    },
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Discover skills from one repository, or all enabled repositories.
    Discover {
        repo: Option<String>,
    },
    /// Install owner/repo:skill, a supported URL, or a skill descriptor file.
    Install {
        source: String,
        #[arg(long, default_value = "claude")]
        app: String,
    },
    Uninstall {
        id: String,
    },
    Check {
        id: String,
    },
    CheckAll,
    Update {
        id: String,
    },
    UpdateAll,
    Enable {
        id: String,
        #[arg(long)]
        app: String,
    },
    Disable {
        id: String,
        #[arg(long)]
        app: String,
    },
    Repo {
        #[command(subcommand)]
        command: SkillRepoCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum SkillRepoCommand {
    List,
    Add {
        url: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long, default_value_t = true)]
        enabled: bool,
    },
    Update {
        id: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        enabled: Option<bool>,
    },
    Remove {
        id: String,
    },
    Catalog {
        id: String,
    },
}

#[derive(Debug, Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCommand,
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    List {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        query: Option<String>,
    },
    Show {
        id: String,
        #[arg(long)]
        app: String,
    },
    Delete {
        id: String,
        #[arg(long)]
        app: String,
    },
    DeleteBatch {
        #[arg(long)]
        from: PathBuf,
    },
    Resume {
        id: String,
        #[arg(long)]
        app: String,
        /// Override the preferred terminal configured in OcHub settings.
        #[arg(long)]
        terminal: Option<String>,
    },
    Scan {
        /// Limit scanning output to these applications. Repeat as needed.
        #[arg(long)]
        app: Vec<String>,
    },
}

#[derive(Debug, Args)]
pub struct ToolArgs {
    #[command(subcommand)]
    pub command: ToolCommand,
}

#[derive(Debug, Subcommand)]
pub enum ToolCommand {
    Versions {
        /// Probe only these tools. Omit to probe all supported tools.
        tools: Vec<String>,
    },
    Probe {
        tool: String,
    },
    Install {
        tool: String,
    },
    Update {
        tool: String,
    },
    Terminal {
        tool: String,
        #[arg(long)]
        terminal: Option<String>,
    },
}

#[derive(Debug, Clone, Args, Default)]
pub struct UsageQueryArgs {
    /// Inclusive start date (YYYY-MM-DD), RFC 3339 timestamp, or Unix seconds.
    #[arg(long)]
    pub from: Option<String>,
    /// Inclusive end date (YYYY-MM-DD), RFC 3339 timestamp, or Unix seconds.
    #[arg(long)]
    pub to: Option<String>,
    #[arg(long)]
    pub app: Option<String>,
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
}

#[derive(Debug, Args)]
pub struct UsageArgs {
    #[command(subcommand)]
    pub command: UsageCommand,
}

#[derive(Debug, Subcommand)]
pub enum UsageCommand {
    Summary {
        #[command(flatten)]
        query: UsageQueryArgs,
    },
    Sources,
    ByApp {
        #[command(flatten)]
        query: UsageQueryArgs,
    },
    Trend {
        #[command(flatten)]
        query: UsageQueryArgs,
        #[arg(long, value_enum, default_value = "day")]
        interval: UsageIntervalArg,
    },
    Providers {
        #[command(flatten)]
        query: UsageQueryArgs,
    },
    Models {
        #[command(flatten)]
        query: UsageQueryArgs,
    },
    Logs {
        #[command(flatten)]
        query: UsageQueryArgs,
        #[arg(long)]
        status: Option<u16>,
        /// Zero-based page index.
        #[arg(long, default_value_t = 0)]
        page: u32,
        #[arg(long, default_value_t = 50)]
        page_size: u32,
    },
    Show {
        request_id: String,
    },
    Sync {
        #[arg(long)]
        app: Vec<String>,
    },
    Limits {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum UsageIntervalArg {
    Day,
    Week,
    Month,
}

#[derive(Debug, Args)]
pub struct PricingArgs {
    #[command(subcommand)]
    pub command: PricingCommand,
}

#[derive(Debug, Subcommand)]
pub enum PricingCommand {
    Status,
    Refresh {
        #[arg(long)]
        force: bool,
    },
    List {
        #[arg(long)]
        query: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    Missing,
    Override {
        #[command(subcommand)]
        command: PricingOverrideCommand,
    },
    Backfill,
}

#[derive(Debug, Subcommand)]
pub enum PricingOverrideCommand {
    List,
    Set {
        #[arg(long)]
        model: String,
        #[arg(long)]
        from: PathBuf,
    },
    Remove {
        #[arg(long)]
        model: String,
    },
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    #[command(subcommand)]
    pub command: SyncCommand,
}

#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    Webdav {
        #[command(subcommand)]
        command: SyncBackendCommand,
    },
    S3 {
        #[command(subcommand)]
        command: SyncBackendCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum SyncBackendCommand {
    Status,
    Configure {
        #[arg(long)]
        from: PathBuf,
        /// Allow an empty credential in the file to replace the saved secret.
        #[arg(long)]
        clear_secret: bool,
    },
    Test,
    Upload,
    Download,
    RemoteInfo,
}

#[derive(Debug, Args)]
pub struct DataDirArgs {
    #[command(subcommand)]
    pub command: DataDirCommand,
}

#[derive(Debug, Subcommand)]
pub enum DataDirCommand {
    Show,
    Set { path: PathBuf },
    Reset,
}

#[derive(Debug, Args)]
pub struct MigrateArgs {
    #[command(subcommand)]
    pub command: MigrateCommand,
}

#[derive(Debug, Subcommand)]
pub enum MigrateCommand {
    Ccswitch {
        #[command(subcommand)]
        command: CcswitchCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum CcswitchCommand {
    Detect,
    Plan,
    Import,
}

#[derive(Debug, Args)]
pub struct BackupArgs {
    #[command(subcommand)]
    pub command: BackupCommand,
}

#[derive(Debug, Subcommand)]
pub enum BackupCommand {
    List,
    Create {
        #[arg(long)]
        name: Option<String>,
    },
    Rename {
        id: String,
        name: String,
    },
    Restore {
        id: String,
    },
    Delete {
        id: String,
    },
    ExportSql {
        file: PathBuf,
    },
    ImportSql {
        file: PathBuf,
    },
    Policy {
        #[command(subcommand)]
        command: BackupPolicyCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum BackupPolicyCommand {
    Show,
    Set {
        /// Backup interval, for example 24h.
        #[arg(long)]
        interval: String,
        #[arg(long)]
        retain: u32,
    },
}

#[derive(Debug, Args)]
pub struct GatewayArgs {
    #[command(subcommand)]
    pub command: GatewayCommand,
}

#[derive(Debug, Subcommand)]
pub enum GatewayCommand {
    Status,
    Start,
    /// Run the Gateway in the foreground until Ctrl-C.
    Serve,
    Stop,
    Restart,
    Health,
    Models,
    SupportedApps,
    ConnectionInfo {
        #[arg(long)]
        app: Option<String>,
    },
    ProbeDialect {
        #[arg(long)]
        url: String,
        /// Read the upstream credential from a file.
        #[arg(long)]
        api_key_file: Option<PathBuf>,
    },
    Config {
        #[command(subcommand)]
        command: GatewayConfigCommand,
    },
    Channel {
        #[command(subcommand)]
        command: GatewayChannelCommand,
    },
    Route {
        #[command(subcommand)]
        command: GatewayRouteCommand,
    },
    Key {
        #[command(subcommand)]
        command: GatewayKeyCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum GatewayConfigCommand {
    Show,
    Set {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        require_key: Option<bool>,
        #[arg(long)]
        enabled: Option<bool>,
        #[arg(long)]
        health_interval: Option<u64>,
    },
}

#[derive(Debug, Subcommand)]
pub enum GatewayChannelCommand {
    List,
    Show {
        id: String,
    },
    Add {
        #[arg(long)]
        from: PathBuf,
    },
    Edit {
        id: String,
        #[arg(long)]
        from: PathBuf,
    },
    Delete {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Probe {
        id: Option<String>,
    },
    Models {
        id: String,
    },
    ImportProvider {
        provider_id: String,
        #[arg(long)]
        app: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum GatewayRouteCommand {
    List,
    Show {
        id: String,
    },
    Add {
        #[arg(long)]
        from: PathBuf,
    },
    Edit {
        id: String,
        #[arg(long)]
        from: PathBuf,
    },
    Delete {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Rule {
        #[command(subcommand)]
        command: GatewayRouteRuleCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum GatewayRouteRuleCommand {
    List {
        #[arg(long)]
        route: String,
    },
    Add {
        #[arg(long)]
        route: String,
        #[arg(long)]
        from: PathBuf,
    },
    Edit {
        model: String,
        #[arg(long)]
        route: String,
        #[arg(long)]
        from: PathBuf,
    },
    Delete {
        model: String,
        #[arg(long)]
        route: String,
    },
    Sort {
        #[arg(long)]
        route: String,
        models: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum GatewayKeyCommand {
    List,
    Show {
        id: String,
    },
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        route: Option<String>,
    },
    Revoke {
        id: String,
    },
    Bind {
        id: String,
        #[arg(long, conflicts_with = "clear")]
        route: Option<String>,
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Debug, Args)]
pub struct StationArgs {
    #[command(subcommand)]
    pub command: StationCommand,
}

#[derive(Debug, Subcommand)]
pub enum StationCommand {
    List,
    Show {
        id: String,
    },
    Add {
        #[arg(long)]
        from: PathBuf,
    },
    Edit {
        id: String,
        #[arg(long)]
        patch: PathBuf,
    },
    Delete {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Probe {
        id: String,
    },
    Models {
        id: String,
    },
    Select {
        id: String,
        #[arg(long)]
        app: String,
    },
    Apply {
        id: String,
        #[arg(long)]
        app: String,
        /// Optional per-application model policy.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    Disconnect {
        #[arg(long)]
        app: String,
    },
    ConnectionInfo {
        id: String,
        #[arg(long)]
        app: String,
    },
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Debug, Args)]
pub struct RemoteArgs {
    #[command(subcommand)]
    pub command: RemoteCommand,
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Print node identity, protocol versions, runtime state, and capabilities.
    Probe,
    /// Serve the Remote Nodes protocol on stdin/stdout.
    Serve {
        /// Confirm that protocol frames use stdin/stdout.
        #[arg(long)]
        stdio: bool,
        /// Own the data directory only for this SSH session instead of starting a daemon.
        #[arg(long)]
        ephemeral: bool,
    },
    /// Inspect the device-local remote access policy.
    Policy {
        #[command(subcommand)]
        command: RemotePolicyCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum RemotePolicyCommand {
    Show,
    Validate,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground.
    Run,
    Status,
    Install,
    Start,
    Stop,
    Restart,
    Logs {
        #[arg(long, default_value_t = 200)]
        lines: usize,
        #[arg(long)]
        follow: bool,
    },
    Uninstall,
}

#[derive(Debug, Args)]
pub struct CompletionArgs {
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Debug, Args)]
pub struct ManArgs {
    /// Write the man page to this file instead of stdout.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser as _;

    #[test]
    fn global_language_and_trace_id_are_validated() {
        assert!(Cli::try_parse_from(["ochcli", "--lang", "xx", "version"]).is_err());
        assert!(
            Cli::try_parse_from(["ochcli", "--trace-id", "contains space", "version"]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "ochcli",
                "--lang",
                "zh-CN",
                "--trace-id",
                "ci/run-1",
                "version"
            ])
            .is_ok()
        );
    }
}
