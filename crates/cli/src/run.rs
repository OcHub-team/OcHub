use std::io::{self, IsTerminal};
use std::path::PathBuf;

use chrono::{DateTime, Days, Local, NaiveDate, TimeZone};
use clap::CommandFactory;
use ochub_core::application::{
    Application, ApplicationError, ApplicationResult, GatewayStation, OpenOptions,
    ProviderSwitchPolicy, UsageFilter, parse_skill_repo_spec, parse_skill_source, redact_json,
};
use ochub_core::gateway::types::GatewayAppModelPolicy;
use ochub_core::gateway::{GatewayChannel, GatewayModelRule, GatewayRoute};
use ochub_core::{AppId, Provider, UsageScript};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::command::{
    AppCommand, AppPathCommand, AuthAccountCommand, AuthBindingCommand, AuthCommand, BackupCommand,
    BackupPolicyCommand, CcswitchCommand, ClaudeCommand, ClaudeDesktopCommand, ClaudeMcpCommand,
    ClaudeMcpConfigCommand, ClaudeMcpPathCommand, ClaudeMcpServerCommand, ClaudeOnboardingCommand,
    ClaudePluginCommand, Cli, CodexAuthCommand, CodexCommand, CodexHistoryCommand, Command,
    CommonConfigCommand, ConfigCommand, CopilotAuthCommand, DataDirCommand, DeclarativeApplyArgs,
    DeclarativePlanArgs, DeeplinkCommand, DesktopAutostartCommand, DesktopCommand, DriftPolicyArg,
    EnvCommand, GatewayChannelCommand, GatewayCommand, GatewayConfigCommand,
    GatewayEndpointCommand, GatewayKeyCommand, GatewayRouteCommand, GatewayRouteRuleCommand,
    GetSetCommand, HermesCommand, HermesMemoryCommand, LightweightCommand, McpCommand,
    MigrateCommand, OmoCommand, OpenclawCommand, OpenclawModelCommand, OpencodeCommand,
    OperationCommand, PluginCommand, PricingCommand, PricingDefaultsCommand,
    PricingOverrideCommand, ProviderCommand, ProviderEndpointCommand, ProviderUsageScriptCommand,
    QuotaCommand, RuntimeCommand, SessionCommand, SettingsCommand, SettingsProxyCommand,
    SkillCommand, SkillRepoCommand, StationCommand, SyncBackendCommand, SyncCommand, ThemeCommand,
    ToolCommand, UpdateCommand, UsageCommand, UsageIntervalArg, UsageQueryArgs,
};
use crate::error::CliError;
use crate::input::{parse_value, read_structured, read_text_limited, write_structured};
use crate::output::Output;

pub async fn execute(cli: Cli, output: &Output) -> Result<(), CliError> {
    if cli.show_secrets && !io::stdout().is_terminal() && !cli.yes {
        return Err(CliError::InvalidInput(
            "--show-secrets on a non-TTY output requires --yes".to_string(),
        ));
    }
    match &cli.command {
        Command::Version => {
            return output.success(
                &json!({
                    "name": "ochcli",
                    "version": env!("CARGO_PKG_VERSION"),
                    "coreVersion": env!("CARGO_PKG_VERSION"),
                    "schemaVersion": 1
                }),
                &[],
            );
        }
        Command::Completion(args) => {
            let mut command = Cli::command();
            clap_complete::generate(args.shell, &mut command, "ochcli", &mut io::stdout().lock());
            return Ok(());
        }
        Command::Man(args) => {
            let man = clap_mangen::Man::new(Cli::command());
            if let Some(path) = &args.output {
                let mut buffer = Vec::new();
                man.render(&mut buffer)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(path, buffer)?;
                return output.success(&json!({ "path": path }), &[]);
            }
            man.render(&mut io::stdout().lock())?;
            return Ok(());
        }
        Command::Daemon(args) => {
            return crate::daemon::execute(&cli, &args.command, output).await;
        }
        Command::Remote(args) => {
            return crate::remote::execute(&cli, &args.command, output).await;
        }
        Command::Paths => {
            ochub_core::app_store::refresh_app_config_dir_override();
            return output.success(
                &json!({
                    "home": ochub_core::paths::get_home_dir(),
                    "dataDir": ochub_core::paths::get_app_config_dir(),
                    "database": ochub_core::paths::get_database_path(),
                    "settings": ochub_core::paths::get_home_dir().join(".ochub/settings.json"),
                    "plugins": ochub_core::plugin::user_plugins_dir(),
                    "ccswitch": ochub_core::paths::get_legacy_ccswitch_dir(),
                }),
                &[],
            );
        }
        _ => {}
    }

    if matches!(
        &cli.command,
        Command::Gateway(crate::command::GatewayArgs {
            command: GatewayCommand::Serve
        })
    ) {
        return crate::daemon::run_foreground(cli.socket.clone(), Some(output), true).await;
    }
    if matches!(
        &cli.command,
        Command::Update(crate::command::UpdateArgs {
            command: UpdateCommand::Install
        })
    ) && let Some(owner) = ochub_core::runtime::active_owner()?
    {
        return Err(ApplicationError::OwnerConflict(format!(
            "runtime pid {} must be stopped before update install",
            owner.pid
        ))
        .into());
    }
    if crate::runtime_client::try_execute(&cli, output).await? {
        return Ok(());
    }
    if matches!(
        &cli.command,
        Command::Runtime(crate::command::RuntimeArgs {
            command: RuntimeCommand::Lightweight { .. }
        })
    ) {
        return Err(ApplicationError::CapabilityUnsupported {
            app: "runtime".to_string(),
            capability: "runtime.lightweight-without-owner",
        }
        .into());
    }
    match &cli.command {
        Command::Gateway(crate::command::GatewayArgs {
            command: GatewayCommand::Start | GatewayCommand::Restart,
        }) => {
            if cli.direct {
                return Err(CliError::InvalidInput(
                    "gateway start/restart requires a persistent owner; use `gateway serve` for direct foreground operation"
                        .to_string(),
                ));
            }
            crate::daemon::start_background(&cli).await?;
            if crate::runtime_client::try_execute(&cli, output).await? {
                return Ok(());
            }
            return Err(ApplicationError::RuntimeUnavailable(
                "daemon started but the gateway request could not be delivered".to_string(),
            )
            .into());
        }
        Command::Gateway(crate::command::GatewayArgs {
            command: GatewayCommand::Stop,
        }) => {
            return output.success(&json!({ "stopped": false, "running": false }), &[]);
        }
        _ => {}
    }
    let _mutation_guard = ochub_core::runtime::MutationGuard::acquire()?;
    let application = Application::open(OpenOptions::default())?;
    execute_with_application(&application, &cli, output).await
}

pub async fn execute_with_application(
    application: &Application,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    let mut journal = None;
    if !cli.dry_run
        && let Some(operation) = mutation_name(&cli.command)
    {
        let blocking = ochub_core::runtime::journal::blocking_operations()?;
        if !blocking.is_empty() {
            return Err(ApplicationError::RecoveryRequired(format!(
                "{} interrupted operation(s): {}",
                blocking.len(),
                blocking
                    .iter()
                    .map(|record| record.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .into());
        }
        journal = Some(if let Some(id) = cli.remote_operation_id.as_deref() {
            let planned = ochub_core::runtime::journal::inspect_operation(id)?;
            if planned.operation != operation || planned.actor != "remote-desktop" {
                return Err(ApplicationError::InvalidInput(format!(
                    "remote operation {id} does not match mutation {operation}"
                ))
                .into());
            }
            ochub_core::runtime::journal::OperationHandle::prepare(id)?
        } else {
            let actor = ochub_core::runtime::active_owner()?
                .filter(|owner| owner.pid == std::process::id())
                .map(|_| "owner")
                .unwrap_or("cli");
            ochub_core::runtime::journal::OperationHandle::begin(
                operation,
                actor,
                json!({
                    "operation": operation,
                    "dataDir": ochub_core::paths::get_app_config_dir()
                }),
            )?
        });
    }

    let result = dispatch(application, cli, output).await;
    match (journal, &result) {
        (Some(journal), Ok(())) => {
            journal.complete(json!({ "ok": true }))?;
        }
        (Some(journal), Err(error)) => {
            if let Err(journal_error) = journal.fail(error.to_string()) {
                tracing::warn!("failed to finish operation journal: {journal_error}");
            }
        }
        (None, _) => {}
    }
    result
}

async fn dispatch(application: &Application, cli: &Cli, output: &Output) -> Result<(), CliError> {
    match &cli.command {
        Command::Status => output.success(&application.status().await?, &[]),
        Command::Doctor { network } => {
            if *network {
                require_online(cli, "doctor network checks")?;
            }
            output.success(&application.doctor(*network).await?, &[])
        }
        Command::Runtime(args) => run_runtime(application, &args.command, cli, output),
        Command::Desktop(args) => run_desktop(application, &args.command, cli, output),
        Command::Operation(args) => run_operation(application, &args.command, cli, output),
        Command::App(args) => run_app(application, &args.command, cli, output).await,
        Command::Plugin(args) => run_plugin(application, &args.command, cli, output),
        Command::Settings(args) => run_settings(application, &args.command, cli, output).await,
        Command::Config(args) => run_config(application, &args.command, cli, output),
        Command::Plan(args) => run_declarative_plan(application, args, output),
        Command::Apply(args) => run_declarative_apply(application, args, cli, output).await,
        Command::Provider(args) => run_provider(application, &args.command, cli, output).await,
        Command::Auth(args) => run_auth(application, &args.command, cli, output).await,
        Command::Quota(args) => run_quota(application, &args.command, cli, output).await,
        Command::ClaudeDesktop(args) => run_claude_desktop(application, &args.command, cli, output),
        Command::Env(args) => run_env(application, &args.command, cli, output),
        Command::Claude(args) => run_claude(application, &args.command, cli, output),
        Command::Codex(args) => run_codex(application, &args.command, cli, output),
        Command::Opencode(args) => run_opencode(application, &args.command, cli, output),
        Command::Openclaw(args) => run_openclaw(application, &args.command, cli, output),
        Command::Hermes(args) => run_hermes(application, &args.command, cli, output),
        Command::Theme(args) => run_theme(application, &args.command, cli, output),
        Command::Deeplink(args) => run_deeplink(application, &args.command, cli, output),
        Command::Update(args) => run_update(application, &args.command, cli, output).await,
        Command::Mcp(args) => run_mcp(application, &args.command, cli, output),
        Command::Skill(args) => run_skill(application, &args.command, cli, output).await,
        Command::Session(args) => run_session(application, &args.command, cli, output).await,
        Command::Tool(args) => run_tool(application, &args.command, cli, output).await,
        Command::Usage(args) => run_usage(application, &args.command, cli, output),
        Command::Pricing(args) => run_pricing(application, &args.command, cli, output).await,
        Command::Sync(args) => run_sync(application, &args.command, cli, output).await,
        Command::DataDir(args) => run_data_dir(application, &args.command, cli, output),
        Command::Migrate(args) => run_migrate(application, &args.command, cli, output),
        Command::Backup(args) => run_backup(application, &args.command, cli, output).await,
        Command::Gateway(args) => run_gateway(application, &args.command, cli, output).await,
        Command::Station(args) => run_station(application, &args.command, cli, output).await,
        Command::Version
        | Command::Paths
        | Command::Completion(_)
        | Command::Man(_)
        | Command::Remote(_)
        | Command::Daemon(_) => Ok(()),
    }
}

fn run_operation(
    application: &Application,
    command: &OperationCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        OperationCommand::List => {
            output.success(&ochub_core::runtime::journal::list_operations()?, &[])
        }
        OperationCommand::Inspect { id } => {
            output.success(&ochub_core::runtime::journal::inspect_operation(id)?, &[])
        }
        OperationCommand::Recover { id } => {
            let record = ochub_core::runtime::journal::inspect_operation(id)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "recover-operation",
                        "operation": record,
                        "resolution": "accept-current-state",
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                require_yes(cli, "operation recovery")?;
                output.success(
                    &ochub_core::runtime::journal::recover_operation(id)?,
                    &["The current database and live configuration state was accepted; no external file rollback was performed.".to_string()],
                )
            }
        }
        OperationCommand::Rollback { id } => {
            let record = ochub_core::runtime::journal::inspect_operation(id)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "rollback-operation",
                        "operation": record,
                        "databaseBackup": record.database_backup,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                require_yes(cli, "operation rollback")?;
                output.success(
                    &ochub_core::runtime::journal::rollback_operation(
                        id,
                        application.state().db.as_ref(),
                    )?,
                    &[],
                )
            }
        }
    }
}

fn mutation_name(command: &Command) -> Option<&'static str> {
    match command {
        Command::Runtime(crate::command::RuntimeArgs {
            command:
                RuntimeCommand::Lightweight {
                    command: LightweightCommand::Enter,
                },
        }) => Some("runtime.lightweight.enter"),
        Command::Runtime(crate::command::RuntimeArgs {
            command:
                RuntimeCommand::Lightweight {
                    command: LightweightCommand::Exit,
                },
        }) => Some("runtime.lightweight.exit"),
        Command::Desktop(crate::command::DesktopArgs {
            command:
                DesktopCommand::Autostart {
                    command: DesktopAutostartCommand::Enable,
                },
        }) => Some("desktop.autostart.enable"),
        Command::Desktop(crate::command::DesktopArgs {
            command:
                DesktopCommand::Autostart {
                    command: DesktopAutostartCommand::Disable,
                },
        }) => Some("desktop.autostart.disable"),
        Command::App(args) => match &args.command {
            AppCommand::Enable { .. } => Some("app.enable"),
            AppCommand::Disable { .. } => Some("app.disable"),
            AppCommand::Path {
                command: AppPathCommand::Set { .. },
            } => Some("app.path.set"),
            AppCommand::Path {
                command: AppPathCommand::Reset { .. },
            } => Some("app.path.reset"),
            _ => None,
        },
        Command::Plugin(args) => match &args.command {
            PluginCommand::Install { .. } => Some("plugin.install"),
            PluginCommand::Reload => Some("plugin.reload"),
            PluginCommand::Remove { .. } => Some("plugin.remove"),
            _ => None,
        },
        Command::Settings(args) => match &args.command {
            SettingsCommand::Set { .. } => Some("settings.set"),
            SettingsCommand::Unset { .. } => Some("settings.unset"),
            SettingsCommand::Import { .. } => Some("settings.import"),
            SettingsCommand::Proxy {
                command: SettingsProxyCommand::Set { .. },
            } => Some("settings.proxy.set"),
            _ => None,
        },
        Command::Config(args) => match &args.command {
            ConfigCommand::Common {
                command: CommonConfigCommand::Set { .. },
            } => Some("config.common.set"),
            ConfigCommand::Common {
                command: CommonConfigCommand::Apply { .. },
            } => Some("config.common.apply"),
            _ => None,
        },
        Command::Apply(_) => Some("config.apply"),
        Command::Provider(args) => match &args.command {
            ProviderCommand::Add { .. } => Some("provider.add"),
            ProviderCommand::Edit { .. } => Some("provider.edit"),
            ProviderCommand::Delete { .. } => Some("provider.delete"),
            ProviderCommand::Duplicate { .. } => Some("provider.duplicate"),
            ProviderCommand::SeedOfficial { .. } => Some("provider.seed-official"),
            ProviderCommand::ImportLive { .. } => Some("provider.import-live"),
            ProviderCommand::SyncLive { .. } => Some("provider.sync-live"),
            ProviderCommand::Switch { .. } => Some("provider.switch"),
            ProviderCommand::AddToLive { .. } => Some("provider.add-to-live"),
            ProviderCommand::RemoveFromLive { .. } => Some("provider.remove-from-live"),
            ProviderCommand::Sort { .. } => Some("provider.sort"),
            ProviderCommand::Copy { .. } => Some("provider.copy"),
            ProviderCommand::Terminal { .. } => Some("provider.terminal"),
            ProviderCommand::Endpoint { command } => match command {
                ProviderEndpointCommand::Add { .. } => Some("provider.endpoint.add"),
                ProviderEndpointCommand::Remove { .. } => Some("provider.endpoint.remove"),
                ProviderEndpointCommand::List { .. } => None,
            },
            _ => None,
        },
        Command::Auth(args) => match &args.command {
            AuthCommand::Copilot { command } => match command {
                CopilotAuthCommand::Login { .. } => Some("auth.copilot.login"),
                CopilotAuthCommand::Poll { .. } => Some("auth.copilot.poll"),
                CopilotAuthCommand::Account { command } => match command {
                    AuthAccountCommand::SetDefault { .. } => Some("auth.copilot.account.default"),
                    AuthAccountCommand::Remove { .. } => Some("auth.copilot.account.remove"),
                    AuthAccountCommand::List => None,
                },
                _ => None,
            },
            AuthCommand::Codex { command } => match command {
                CodexAuthCommand::Login => Some("auth.codex.login"),
                CodexAuthCommand::Poll { .. } => Some("auth.codex.poll"),
                CodexAuthCommand::Logout { .. } => Some("auth.codex.logout"),
                CodexAuthCommand::Account { command } => match command {
                    AuthAccountCommand::SetDefault { .. } => Some("auth.codex.account.default"),
                    AuthAccountCommand::Remove { .. } => Some("auth.codex.account.remove"),
                    AuthAccountCommand::List => None,
                },
                _ => None,
            },
            AuthCommand::Binding { command } => match command {
                AuthBindingCommand::Set { .. } => Some("auth.binding.set"),
                AuthBindingCommand::Remove { .. } => Some("auth.binding.remove"),
                AuthBindingCommand::List => None,
            },
        },
        Command::ClaudeDesktop(args) => match &args.command {
            ClaudeDesktopCommand::EnsureOfficial => Some("claude-desktop.ensure-official"),
            ClaudeDesktopCommand::ImportFromClaude => Some("claude-desktop.import-from-claude"),
            ClaudeDesktopCommand::Status => None,
        },
        Command::Env(args) => match &args.command {
            EnvCommand::Clean { .. } => Some("env.clean"),
            EnvCommand::Restore { .. } => Some("env.restore"),
            EnvCommand::Scan => None,
        },
        Command::Claude(args) => match &args.command {
            ClaudeCommand::Plugin { command } => match command {
                ClaudePluginCommand::Apply { .. } => Some("claude.plugin.apply"),
                ClaudePluginCommand::Restore => Some("claude.plugin.restore"),
                _ => None,
            },
            ClaudeCommand::Mcp { command } => match command {
                ClaudeMcpCommand::Server { command } => match command {
                    ClaudeMcpServerCommand::Upsert { .. } => Some("claude.mcp.server.upsert"),
                    ClaudeMcpServerCommand::Delete { .. } => Some("claude.mcp.server.delete"),
                },
                ClaudeMcpCommand::Onboarding { command } => match command {
                    ClaudeOnboardingCommand::Skip => Some("claude.mcp.onboarding.skip"),
                    ClaudeOnboardingCommand::Clear => Some("claude.mcp.onboarding.clear"),
                    ClaudeOnboardingCommand::Status => None,
                },
                _ => None,
            },
        },
        Command::Codex(args) => match &args.command {
            CodexCommand::History { command } => match command {
                CodexHistoryCommand::Migrate => Some("codex.history.migrate"),
                CodexHistoryCommand::Restore => Some("codex.history.restore"),
                CodexHistoryCommand::Status => None,
            },
        },
        Command::Opencode(args) => match &args.command {
            OpencodeCommand::Omo { command } | OpencodeCommand::OmoSlim { command } => {
                matches!(command, OmoCommand::Disable).then_some("opencode.omo.disable")
            }
        },
        Command::Openclaw(args) => match &args.command {
            OpenclawCommand::Model {
                command:
                    OpenclawModelCommand::Default {
                        command: GetSetCommand::Set { .. },
                    },
            } => Some("openclaw.model.default.set"),
            OpenclawCommand::AgentDefaults {
                command: GetSetCommand::Set { .. },
            } => Some("openclaw.agent-defaults.set"),
            OpenclawCommand::Env {
                command: GetSetCommand::Set { .. },
            } => Some("openclaw.env.set"),
            OpenclawCommand::Tools {
                command: GetSetCommand::Set { .. },
            } => Some("openclaw.tools.set"),
            _ => None,
        },
        Command::Hermes(args) => match &args.command {
            HermesCommand::Models {
                command: GetSetCommand::Set { .. },
            } => Some("hermes.models.set"),
            HermesCommand::Memory { command } => match command {
                HermesMemoryCommand::Write { .. } => Some("hermes.memory.write"),
                HermesMemoryCommand::Enable { .. } => Some("hermes.memory.enable"),
                HermesMemoryCommand::Disable { .. } => Some("hermes.memory.disable"),
                _ => None,
            },
            _ => None,
        },
        Command::Theme(args) => match &args.command {
            ThemeCommand::Import { .. } => Some("theme.import"),
            ThemeCommand::Duplicate { .. } => Some("theme.duplicate"),
            ThemeCommand::Delete { .. } => Some("theme.delete"),
            ThemeCommand::Set { .. } => Some("theme.set"),
            ThemeCommand::Mode { .. } => Some("theme.mode"),
            _ => None,
        },
        Command::Deeplink(args) => match &args.command {
            DeeplinkCommand::Import { .. } => Some("deeplink.import"),
            DeeplinkCommand::Parse { .. } => None,
        },
        Command::Update(args) => match &args.command {
            UpdateCommand::Install => Some("update.install"),
            _ => None,
        },
        Command::Mcp(args) => match &args.command {
            McpCommand::Add { .. } => Some("mcp.add"),
            McpCommand::Edit { .. } => Some("mcp.edit"),
            McpCommand::Delete { .. } => Some("mcp.delete"),
            McpCommand::Import { .. } => Some("mcp.import"),
            McpCommand::Enable { .. } => Some("mcp.enable"),
            McpCommand::Disable { .. } => Some("mcp.disable"),
            McpCommand::Sync { .. } => Some("mcp.sync"),
            McpCommand::SyncAll => Some("mcp.sync-all"),
            _ => None,
        },
        Command::Skill(args) => match &args.command {
            SkillCommand::Install { .. } => Some("skill.install"),
            SkillCommand::Uninstall { .. } => Some("skill.uninstall"),
            SkillCommand::Update { .. } => Some("skill.update"),
            SkillCommand::UpdateAll => Some("skill.update-all"),
            SkillCommand::Enable { .. } => Some("skill.enable"),
            SkillCommand::Disable { .. } => Some("skill.disable"),
            SkillCommand::Repo { command } => match command {
                SkillRepoCommand::Add { .. } => Some("skill.repo.add"),
                SkillRepoCommand::Update { .. } => Some("skill.repo.update"),
                SkillRepoCommand::Remove { .. } => Some("skill.repo.remove"),
                _ => None,
            },
            _ => None,
        },
        Command::Session(args) => match &args.command {
            SessionCommand::Delete { .. } => Some("session.delete"),
            SessionCommand::DeleteBatch { .. } => Some("session.delete-batch"),
            SessionCommand::Resume { .. } => Some("session.resume"),
            SessionCommand::IndexBuild => Some("session.index-build"),
            SessionCommand::IndexMaintain { .. } => Some("session.index-maintain"),
            SessionCommand::IndexDelete => Some("session.index-delete"),
            _ => None,
        },
        Command::Tool(args) => match &args.command {
            ToolCommand::Install { .. } => Some("tool.install"),
            ToolCommand::Update { .. } => Some("tool.update"),
            ToolCommand::Terminal { .. } => Some("tool.terminal"),
            _ => None,
        },
        Command::Usage(args) => match &args.command {
            UsageCommand::Sync { .. } => Some("usage.sync"),
            _ => None,
        },
        Command::Pricing(args) => match &args.command {
            PricingCommand::Refresh { .. } => Some("pricing.refresh"),
            PricingCommand::Backfill => Some("pricing.backfill"),
            PricingCommand::Defaults {
                command: PricingDefaultsCommand::Set { .. },
            } => Some("pricing.defaults.set"),
            PricingCommand::Override { command } => match command {
                PricingOverrideCommand::Set { .. } => Some("pricing.override.set"),
                PricingOverrideCommand::Remove { .. } => Some("pricing.override.remove"),
                PricingOverrideCommand::List => None,
            },
            _ => None,
        },
        Command::Sync(args) => match &args.command {
            SyncCommand::Webdav { command } => match command {
                SyncBackendCommand::Configure { .. } => Some("sync.webdav.configure"),
                SyncBackendCommand::Upload => Some("sync.webdav.upload"),
                SyncBackendCommand::Download => Some("sync.webdav.download"),
                _ => None,
            },
            SyncCommand::S3 { command } => match command {
                SyncBackendCommand::Configure { .. } => Some("sync.s3.configure"),
                SyncBackendCommand::Upload => Some("sync.s3.upload"),
                SyncBackendCommand::Download => Some("sync.s3.download"),
                _ => None,
            },
        },
        Command::DataDir(args) => match &args.command {
            DataDirCommand::Set { .. } => Some("data-dir.set"),
            DataDirCommand::Reset => Some("data-dir.reset"),
            DataDirCommand::Show => None,
        },
        Command::Migrate(args) => match &args.command {
            MigrateCommand::Ccswitch {
                command: CcswitchCommand::Import,
            } => Some("migrate.ccswitch.import"),
            _ => None,
        },
        Command::Backup(args) => match &args.command {
            BackupCommand::List => None,
            BackupCommand::Create { .. } => Some("backup.create"),
            BackupCommand::Rename { .. } => Some("backup.rename"),
            BackupCommand::Restore { .. } => Some("backup.restore"),
            BackupCommand::Delete { .. } => Some("backup.delete"),
            BackupCommand::ExportSql { .. } => Some("backup.export-sql"),
            BackupCommand::ImportSql { .. } => Some("backup.import-sql"),
            BackupCommand::Policy { command } => match command {
                BackupPolicyCommand::Set { .. } => Some("backup.policy.set"),
                BackupPolicyCommand::Show => None,
            },
        },
        Command::Gateway(args) => match &args.command {
            GatewayCommand::Start => Some("gateway.start"),
            GatewayCommand::Stop => Some("gateway.stop"),
            GatewayCommand::Restart => Some("gateway.restart"),
            GatewayCommand::Config {
                command: GatewayConfigCommand::Set { .. },
            } => Some("gateway.config.set"),
            GatewayCommand::Channel { command } => match command {
                GatewayChannelCommand::Add { .. } => Some("gateway.channel.add"),
                GatewayChannelCommand::Edit { .. } => Some("gateway.channel.edit"),
                GatewayChannelCommand::Delete { .. } => Some("gateway.channel.delete"),
                GatewayChannelCommand::Enable { .. } => Some("gateway.channel.enable"),
                GatewayChannelCommand::Disable { .. } => Some("gateway.channel.disable"),
                GatewayChannelCommand::ImportProvider { .. } => {
                    Some("gateway.channel.import-provider")
                }
                _ => None,
            },
            GatewayCommand::Route { command } => match command {
                GatewayRouteCommand::Add { .. } => Some("gateway.route.add"),
                GatewayRouteCommand::Edit { .. } => Some("gateway.route.edit"),
                GatewayRouteCommand::Delete { .. } => Some("gateway.route.delete"),
                GatewayRouteCommand::Enable { .. } => Some("gateway.route.enable"),
                GatewayRouteCommand::Disable { .. } => Some("gateway.route.disable"),
                GatewayRouteCommand::Rule { command } => match command {
                    GatewayRouteRuleCommand::Add { .. } => Some("gateway.route.rule.add"),
                    GatewayRouteRuleCommand::Edit { .. } => Some("gateway.route.rule.edit"),
                    GatewayRouteRuleCommand::Delete { .. } => Some("gateway.route.rule.delete"),
                    GatewayRouteRuleCommand::Sort { .. } => Some("gateway.route.rule.sort"),
                    GatewayRouteRuleCommand::List { .. } => None,
                },
                _ => None,
            },
            GatewayCommand::Key { command } => match command {
                GatewayKeyCommand::Create { .. } => Some("gateway.key.create"),
                GatewayKeyCommand::Revoke { .. } => Some("gateway.key.revoke"),
                GatewayKeyCommand::Bind { .. } => Some("gateway.key.bind"),
                _ => None,
            },
            _ => None,
        },
        Command::Station(args) => match &args.command {
            StationCommand::Add { .. } => Some("station.add"),
            StationCommand::Edit { .. } => Some("station.edit"),
            StationCommand::Delete { .. } => Some("station.delete"),
            StationCommand::Enable { .. } => Some("station.enable"),
            StationCommand::Disable { .. } => Some("station.disable"),
            StationCommand::Select { .. } => Some("station.select"),
            StationCommand::Apply { .. } => Some("station.apply"),
            StationCommand::Disconnect { .. } => Some("station.disconnect"),
            _ => None,
        },
        _ => None,
    }
}

fn run_runtime(
    application: &Application,
    command: &RuntimeCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        RuntimeCommand::Portable => output.success(&application.portable_runtime_status()?, &[]),
        RuntimeCommand::Lightweight { command } => match command {
            LightweightCommand::Status => output.success(
                &json!({ "enabled": ochub_core::runtime::lightweight_mode() }),
                &[],
            ),
            LightweightCommand::Enter => {
                if cli.dry_run {
                    output.success(
                        &json!({ "action": "enter-lightweight-mode", "dryRun": true }),
                        &[],
                    )
                } else {
                    ochub_core::runtime::set_lightweight_mode(true);
                    output.success(&json!({ "enabled": true }), &[])
                }
            }
            LightweightCommand::Exit => {
                if cli.dry_run {
                    output.success(
                        &json!({ "action": "exit-lightweight-mode", "dryRun": true }),
                        &[],
                    )
                } else {
                    ochub_core::runtime::set_lightweight_mode(false);
                    output.success(&json!({ "enabled": false }), &[])
                }
            }
        },
    }
}

fn run_desktop(
    application: &Application,
    command: &DesktopCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        DesktopCommand::Autostart { command } => match command {
            DesktopAutostartCommand::Status => {
                output.success(&application.desktop_autostart_status()?, &[])
            }
            DesktopAutostartCommand::Enable | DesktopAutostartCommand::Disable => {
                let enabled = matches!(command, DesktopAutostartCommand::Enable);
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": if enabled { "enable-desktop-autostart" } else { "disable-desktop-autostart" },
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(&application.set_desktop_autostart(enabled)?, &[])
                }
            }
        },
    }
}

fn run_plugin(
    application: &Application,
    command: &PluginCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        PluginCommand::List => output.success(&application.list_user_plugins()?, &[]),
        PluginCommand::Show { app } => {
            output.success(&application.get_user_plugin(&app_id(app)?)?, &[])
        }
        PluginCommand::Validate { file } => {
            output.success(&application.validate_plugin_manifest(file)?, &[])
        }
        PluginCommand::Install { file } => {
            let validation = application.validate_plugin_manifest(file)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "install-plugin",
                        "source": file,
                        "validation": validation,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.install_plugin_manifest(file)?, &[])
            }
        }
        PluginCommand::Reload => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "reload-plugins",
                        "directory": ochub_core::plugin::user_plugins_dir(),
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.reload_plugins()?, &[])
            }
        }
        PluginCommand::Errors => output.success(&application.plugin_errors(), &[]),
        PluginCommand::Remove { app, purge_data } => {
            let target = application.get_user_plugin(&app_id(app)?)?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "remove-plugin",
                        "target": target.plugin,
                        "purgeData": purge_data,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "plugin remove")?;
            output.success(
                &application.remove_plugin_manifest(&app_id(app)?, *purge_data)?,
                &[],
            )
        }
    }
}

async fn run_sync(
    application: &Application,
    command: &SyncCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        SyncCommand::Webdav { command } => run_webdav_sync(application, command, cli, output).await,
        SyncCommand::S3 { command } => run_s3_sync(application, command, cli, output).await,
    }
}

async fn run_webdav_sync(
    application: &Application,
    command: &SyncBackendCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        SyncBackendCommand::Status => {
            output.success(&application.webdav_sync_status(cli.show_secrets)?, &[])
        }
        SyncBackendCommand::Configure { from, clear_secret } => {
            let settings: ochub_core::settings::WebDavSyncSettings = read_structured(from)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "configure-webdav-sync",
                        "settings": redact_json(&serde_json::to_value(settings)?),
                        "preserveEmptyPassword": !clear_secret,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &application.configure_webdav_sync(settings, !clear_secret)?,
                    &[],
                )
            }
        }
        SyncBackendCommand::Test { from } => {
            require_online(cli, "WebDAV connection test")?;
            let result = if let Some(path) = from {
                let settings: ochub_core::settings::WebDavSyncSettings = read_structured(path)?;
                application.test_webdav_sync_settings(settings).await?
            } else {
                application.test_webdav_sync().await?
            };
            output.success(&result, &[])
        }
        SyncBackendCommand::Upload => {
            require_online(cli, "WebDAV upload")?;
            if cli.dry_run {
                let remote = application.webdav_remote_info().await?;
                output.success(
                    &sync_plan("webdav", "upload", remote),
                    &sync_scope_warnings("upload"),
                )
            } else {
                output.success(
                    &application.upload_webdav_sync().await?,
                    &sync_scope_warnings("upload"),
                )
            }
        }
        SyncBackendCommand::Download => {
            require_online(cli, "WebDAV download")?;
            let remote = application.webdav_remote_info().await?;
            if cli.dry_run {
                return output.success(
                    &sync_plan("webdav", "download", remote),
                    &sync_scope_warnings("download"),
                );
            }
            require_yes(cli, "WebDAV snapshot download")?;
            let result = application.download_webdav_sync().await?;
            let mut warnings = sync_scope_warnings("download");
            warnings.extend(result.warnings);
            output.success(&result.data, &warnings)
        }
        SyncBackendCommand::RemoteInfo => {
            require_online(cli, "WebDAV remote info")?;
            output.success(&application.webdav_remote_info().await?, &[])
        }
    }
}

async fn run_s3_sync(
    application: &Application,
    command: &SyncBackendCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        SyncBackendCommand::Status => {
            output.success(&application.s3_sync_status(cli.show_secrets)?, &[])
        }
        SyncBackendCommand::Configure { from, clear_secret } => {
            let settings: ochub_core::settings::S3SyncSettings = read_structured(from)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "configure-s3-sync",
                        "settings": redact_json(&serde_json::to_value(settings)?),
                        "preserveEmptySecret": !clear_secret,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &application.configure_s3_sync(settings, !clear_secret)?,
                    &[],
                )
            }
        }
        SyncBackendCommand::Test { from } => {
            require_online(cli, "S3 connection test")?;
            let result = if let Some(path) = from {
                let settings: ochub_core::settings::S3SyncSettings = read_structured(path)?;
                application.test_s3_sync_settings(settings).await?
            } else {
                application.test_s3_sync().await?
            };
            output.success(&result, &[])
        }
        SyncBackendCommand::Upload => {
            require_online(cli, "S3 upload")?;
            if cli.dry_run {
                let remote = application.s3_remote_info().await?;
                output.success(
                    &sync_plan("s3", "upload", remote),
                    &sync_scope_warnings("upload"),
                )
            } else {
                output.success(
                    &application.upload_s3_sync().await?,
                    &sync_scope_warnings("upload"),
                )
            }
        }
        SyncBackendCommand::Download => {
            require_online(cli, "S3 download")?;
            let remote = application.s3_remote_info().await?;
            if cli.dry_run {
                return output.success(
                    &sync_plan("s3", "download", remote),
                    &sync_scope_warnings("download"),
                );
            }
            require_yes(cli, "S3 snapshot download")?;
            let result = application.download_s3_sync().await?;
            let mut warnings = sync_scope_warnings("download");
            warnings.extend(result.warnings);
            output.success(&result.data, &warnings)
        }
        SyncBackendCommand::RemoteInfo => {
            require_online(cli, "S3 remote info")?;
            output.success(&application.s3_remote_info().await?, &[])
        }
    }
}

fn sync_plan(backend: &str, direction: &str, remote: Value) -> Value {
    json!({
        "backend": backend,
        "direction": direction,
        "remote": remote,
        "scope": ["database", "managed-skills"],
        "strategy": "last-writer-wins",
        "createsSafetyBackup": direction == "download",
        "dryRun": true
    })
}

fn sync_scope_warnings(direction: &str) -> Vec<String> {
    vec![
        format!("{direction} uses last-writer-wins semantics; this is not a record-level merge."),
        "Snapshot scope is the OcHub database and OcHub-managed Skills; third-party live config files are not archived."
            .to_string(),
    ]
}

fn run_data_dir(
    _application: &Application,
    command: &DataDirCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        DataDirCommand::Show => output.success(
            &json!({
                "effective": ochub_core::paths::get_app_config_dir(),
                "persistentOverride": ochub_core::app_store::refresh_app_config_dir_override(),
                "processOverride": cli.data_dir
            }),
            &[],
        ),
        DataDirCommand::Set { path } => {
            if cli.data_dir.is_some() {
                return Err(CliError::InvalidInput(
                    "data-dir set cannot be combined with the process-local --data-dir override"
                        .to_string(),
                ));
            }
            let absolute = resolve_absolute_path(path)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "set-data-dir",
                        "path": absolute,
                        "takesEffectAfterRestart": true,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let absolute = ensure_absolute_directory(&absolute)?;
                ochub_core::app_store::set_app_config_dir_to_store(Some(
                    &absolute.to_string_lossy(),
                ))?;
                output.success(
                    &json!({
                        "path": absolute,
                        "takesEffectAfterRestart": true
                    }),
                    &["Restart the GUI, daemon, and CLI processes before writing to the new data directory.".to_string()],
                )
            }
        }
        DataDirCommand::Reset => {
            if cli.data_dir.is_some() {
                return Err(CliError::InvalidInput(
                    "data-dir reset cannot be combined with --data-dir".to_string(),
                ));
            }
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "reset-data-dir",
                        "default": ochub_core::paths::get_home_dir().join(".ochub"),
                        "takesEffectAfterRestart": true,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                ochub_core::app_store::set_app_config_dir_to_store(None)?;
                output.success(
                    &json!({
                        "path": ochub_core::paths::get_home_dir().join(".ochub"),
                        "takesEffectAfterRestart": true
                    }),
                    &["Restart the GUI, daemon, and CLI processes before writing to the reset data directory.".to_string()],
                )
            }
        }
    }
}

fn run_migrate(
    application: &Application,
    command: &MigrateCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        MigrateCommand::Ccswitch { command } => match command {
            CcswitchCommand::Detect => {
                let source = application.detect_ccswitch()?;
                output.success(
                    &json!({ "detected": source.is_some(), "source": source }),
                    &[],
                )
            }
            CcswitchCommand::Plan => output.success(&application.plan_ccswitch_import()?, &[]),
            CcswitchCommand::Import => {
                let plan = application.plan_ccswitch_import()?;
                if cli.dry_run {
                    return output.success(&plan, &[]);
                }
                require_yes(cli, "cc-switch import")?;
                output.success(&application.import_ccswitch()?, &[])
            }
        },
    }
}

fn resolve_absolute_path(path: &std::path::Path) -> Result<PathBuf, CliError> {
    if path.as_os_str().is_empty() {
        return Err(CliError::InvalidInput(
            "data directory cannot be empty".to_string(),
        ));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute.exists() && !absolute.is_dir() {
        return Err(CliError::InvalidInput(format!(
            "data directory is not a directory: {}",
            absolute.display()
        )));
    }
    Ok(absolute)
}

fn ensure_absolute_directory(path: &std::path::Path) -> Result<PathBuf, CliError> {
    std::fs::create_dir_all(path)?;
    let canonical = path.canonicalize()?;
    if !canonical.is_dir() {
        return Err(CliError::InvalidInput(format!(
            "data directory is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn run_usage(
    application: &Application,
    command: &UsageCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    let warnings = usage_warnings(application)?;
    match command {
        UsageCommand::Summary { query } => output.success(
            &application.usage_summary(&usage_filter(query)?)?,
            &warnings,
        ),
        UsageCommand::Sources => output.success(&application.usage_sources()?, &warnings),
        UsageCommand::ByApp { query } => {
            output.success(&application.usage_by_app(&usage_filter(query)?)?, &warnings)
        }
        UsageCommand::Trend { query, interval } => output.success(
            &application.usage_trend(
                &usage_filter(query)?,
                match interval {
                    UsageIntervalArg::Day => "day",
                    UsageIntervalArg::Week => "week",
                    UsageIntervalArg::Month => "month",
                },
            )?,
            &warnings,
        ),
        UsageCommand::Providers { query } => output.success(
            &application.usage_provider_stats(&usage_filter(query)?)?,
            &warnings,
        ),
        UsageCommand::Models { query } => output.success(
            &application.usage_model_stats(&usage_filter(query)?)?,
            &warnings,
        ),
        UsageCommand::Logs {
            query,
            status,
            page,
            page_size,
        } => {
            let filter = usage_filter(query)?;
            let logs = ochub_core::services::usage_stats::LogFilters {
                app_type: filter.app,
                provider_name: filter.provider,
                model: filter.model,
                status_code: *status,
                start_date: filter.start,
                end_date: filter.end,
            };
            output.success(
                &application.usage_logs(&logs, *page, *page_size)?,
                &warnings,
            )
        }
        UsageCommand::Show { request_id } => {
            output.success(&application.usage_request(request_id)?, &warnings)
        }
        UsageCommand::Sync { app } => {
            let apps = app
                .iter()
                .map(|raw| app_id(raw))
                .collect::<Result<Vec<_>, _>>()?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "sync-usage",
                        "apps": apps,
                        "dryRun": true
                    }),
                    &warnings,
                )
            } else {
                let result = application.sync_usage(&apps)?;
                if result.errors.is_empty() {
                    output.success(&result, &warnings)
                } else {
                    Err(CliError::Application(ApplicationError::PartialFailure {
                        message: "usage sync completed with errors".to_string(),
                        details: serde_json::to_value(result)?,
                    }))
                }
            }
        }
        UsageCommand::Limits { app, provider } => {
            let app = app.as_deref().map(app_id).transpose()?;
            output.success(
                &application.usage_limits(app.as_ref(), provider.as_deref())?,
                &warnings,
            )
        }
    }
}

async fn run_pricing(
    application: &Application,
    command: &PricingCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        PricingCommand::Status => output.success(&application.pricing_status()?, &[]),
        PricingCommand::Refresh { force } => {
            require_online(cli, "pricing refresh")?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "refresh-pricing",
                        "source": ochub_core::services::pricing_catalog::LITELLM_PRICING_SOURCE_URL,
                        "force": force,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.refresh_pricing(*force).await?, &[])
            }
        }
        PricingCommand::List {
            query,
            limit,
            offset,
        } => output.success(
            &application.list_pricing_catalog(query.as_deref(), *limit, *offset)?,
            &[],
        ),
        PricingCommand::Missing => {
            output.success(&application.pricing_status()?.missing_models, &[])
        }
        PricingCommand::Override { command } => match command {
            PricingOverrideCommand::List => {
                output.success(&application.list_pricing_overrides()?, &[])
            }
            PricingOverrideCommand::Set { model, from } => {
                let mut pricing: ochub_core::services::usage_stats::ModelPricingInfo =
                    read_structured(from)?;
                if pricing.model_id.trim().is_empty() {
                    pricing.model_id = model.clone();
                }
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "set-pricing-override",
                            "model": model,
                            "pricing": pricing,
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(&application.set_pricing_override(model, &pricing)?, &[])
                }
            }
            PricingOverrideCommand::Remove { model } => {
                let target = application
                    .list_pricing_overrides()?
                    .into_iter()
                    .find(|pricing| pricing.model_id == *model)
                    .ok_or_else(|| not_found("pricing-override", model))?;
                if cli.dry_run {
                    return output.success(
                        &json!({
                            "action": "remove-pricing-override",
                            "target": target,
                            "dryRun": true
                        }),
                        &[],
                    );
                }
                require_yes(cli, "pricing override remove")?;
                application.remove_pricing_override(model)?;
                output.success(&json!({ "model": model, "deleted": true }), &[])
            }
        },
        PricingCommand::Defaults { command } => match command {
            PricingDefaultsCommand::Get => {
                output.success(&application.pricing_defaults().await?, &[])
            }
            PricingDefaultsCommand::Set { from } => {
                let defaults: Vec<ochub_core::application::PricingDefault> = read_structured(from)?;
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "set-pricing-defaults",
                            "defaults": defaults,
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(&application.set_pricing_defaults(&defaults).await?, &[])
                }
            }
        },
        PricingCommand::Backfill => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "backfill-pricing",
                        "missingModels": application.pricing_status()?.missing_models,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &json!({ "backfilledRows": application.backfill_pricing()? }),
                    &[],
                )
            }
        }
    }
}

fn usage_filter(query: &UsageQueryArgs) -> Result<UsageFilter, CliError> {
    let start = query
        .from
        .as_deref()
        .map(|value| parse_time_bound(value, false))
        .transpose()?;
    let end = query
        .to
        .as_deref()
        .map(|value| parse_time_bound(value, true))
        .transpose()?;
    if start.zip(end).is_some_and(|(start, end)| start > end) {
        return Err(CliError::InvalidInput(
            "--from must not be later than --to".to_string(),
        ));
    }
    let app = query
        .app
        .as_deref()
        .map(app_id)
        .transpose()?
        .map(|app| app.to_string());
    Ok(UsageFilter {
        start,
        end,
        app,
        provider: query.provider.clone(),
        model: query.model.clone(),
    })
}

fn parse_time_bound(value: &str, end_of_day: bool) -> Result<i64, CliError> {
    if let Ok(timestamp) = value.parse::<i64>() {
        return Ok(timestamp);
    }
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.timestamp());
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
        CliError::InvalidInput(format!(
            "invalid date or timestamp: {value}; expected YYYY-MM-DD, RFC 3339, or Unix seconds"
        ))
    })?;
    let date = if end_of_day {
        date.checked_add_days(Days::new(1))
            .ok_or_else(|| CliError::InvalidInput(format!("date is out of range: {value}")))?
    } else {
        date
    };
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| CliError::InvalidInput(format!("date is out of range: {value}")))?;
    let timestamp = match Local.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(value) => value.timestamp(),
        chrono::LocalResult::Ambiguous(first, _) => first.timestamp(),
        chrono::LocalResult::None => {
            return Err(CliError::InvalidInput(format!(
                "date has no local midnight in the configured timezone: {value}"
            )));
        }
    };
    Ok(if end_of_day {
        timestamp.saturating_sub(1)
    } else {
        timestamp
    })
}

fn parse_duration(value: &str) -> Result<std::time::Duration, CliError> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .ok_or_else(|| {
            CliError::InvalidInput(format!(
                "duration requires a unit (ms, s, m, or h): {value}"
            ))
        })?;
    let amount = value[..split]
        .parse::<f64>()
        .map_err(|_| CliError::InvalidInput(format!("invalid duration: {value}")))?;
    if !amount.is_finite() || amount < 0.0 {
        return Err(CliError::InvalidInput(format!("invalid duration: {value}")));
    }
    let unit = &value[split..];
    let seconds = match unit {
        "ms" => amount / 1_000.0,
        "s" => amount,
        "m" => amount * 60.0,
        "h" => amount * 3_600.0,
        _ => {
            return Err(CliError::InvalidInput(format!(
                "unsupported duration unit in {value}; use ms, s, m, or h"
            )));
        }
    };
    std::time::Duration::try_from_secs_f64(seconds)
        .map_err(|_| CliError::InvalidInput(format!("duration is out of range: {value}")))
}

fn usage_warnings(application: &Application) -> Result<Vec<String>, CliError> {
    let status = application.pricing_status()?;
    let mut warnings = vec![
        "Cost values are estimates derived from the local pricing catalog, not invoices."
            .to_string(),
    ];
    if !status.missing_models.is_empty() {
        warnings.push(format!(
            "{} model(s) have missing pricing; their estimated cost may be zero.",
            status.missing_models.len()
        ));
    }
    Ok(warnings)
}

async fn run_session(
    application: &Application,
    command: &SessionCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        SessionCommand::List { app, query } => {
            let apps = app
                .as_deref()
                .map(app_id)
                .transpose()?
                .into_iter()
                .collect::<Vec<_>>();
            output.success(&application.list_sessions(&apps, query.as_deref())?, &[])
        }
        SessionCommand::Show { id, app } => {
            let (session, messages) = application.get_session_messages(&app_id(app)?, id)?;
            output.success(&json!({ "session": session, "messages": messages }), &[])
        }
        SessionCommand::Delete { id, app } => {
            let target = application.get_session(&app_id(app)?, id)?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "delete-session",
                        "target": target,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "session delete")?;
            output.success(&application.delete_session(&app_id(app)?, id)?, &[])
        }
        SessionCommand::DeleteBatch { from } => {
            let requests = read_session_delete_batch(from)?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "delete-sessions",
                        "targets": requests,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "session batch delete")?;
            output.success(&application.delete_sessions(&requests)?, &[])
        }
        SessionCommand::Resume { id, app, terminal } => {
            let session = application.get_session(&app_id(app)?, id)?;
            let command = session.resume_command.as_deref().ok_or_else(|| {
                CliError::Application(ApplicationError::InvalidInput(format!(
                    "session {app}/{id} has no resume command"
                )))
            })?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "resume-session",
                        "session": session,
                        "command": command,
                        "terminal": terminal,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            let target = terminal
                .clone()
                .or_else(|| ochub_core::settings::get_settings().preferred_terminal)
                .unwrap_or_else(|| "terminal".to_string());
            let command = command.to_string();
            let cwd = session.project_dir.clone();
            tokio::task::spawn_blocking(move || {
                ochub_core::session_manager::terminal::launch_terminal(
                    &target,
                    &command,
                    cwd.as_deref(),
                    None,
                )
            })
            .await
            .map_err(|error| {
                CliError::Application(ApplicationError::OperationFailed(error.to_string()))
            })?
            .map_err(|error| {
                let application_error = if error.contains("only supported") {
                    ApplicationError::PlatformUnsupported(error)
                } else {
                    ApplicationError::OperationFailed(error)
                };
                CliError::Application(application_error)
            })?;
            output.success(&json!({ "resumed": true, "app": app, "id": id }), &[])
        }
        SessionCommand::Scan { app } => {
            let apps = app
                .iter()
                .map(|raw| app_id(raw))
                .collect::<Result<Vec<_>, _>>()?;
            output.success(&application.list_sessions(&apps, None)?, &[])
        }
        SessionCommand::Search { query, limit } => {
            output.success(&application.search_session_index(query, *limit)?, &[])
        }
        SessionCommand::IndexStatus => output.success(
            &json!({
                "enabled": ochub_core::settings::get_settings().session_index_enabled,
                "stats": application.session_index_status()?
            }),
            &[],
        ),
        SessionCommand::IndexBuild => output.success(&application.sync_session_index()?, &[]),
        SessionCommand::IndexMaintain { budget_seconds } => output.success(
            &application.maintain_session_index(std::time::Duration::from_secs(*budget_seconds))?,
            &[],
        ),
        SessionCommand::IndexDelete => {
            require_yes(cli, "session index delete")?;
            output.success(&application.delete_session_index()?, &[])
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SessionDeleteBatchInput {
    Items {
        items: Vec<ochub_core::session_manager::DeleteSessionRequest>,
    },
    List(Vec<ochub_core::session_manager::DeleteSessionRequest>),
}

fn read_session_delete_batch(
    path: &std::path::Path,
) -> Result<Vec<ochub_core::session_manager::DeleteSessionRequest>, CliError> {
    Ok(match read_structured::<SessionDeleteBatchInput>(path)? {
        SessionDeleteBatchInput::Items { items } | SessionDeleteBatchInput::List(items) => items,
    })
}

async fn run_tool(
    application: &Application,
    command: &ToolCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        ToolCommand::Versions { tools } => {
            let tools = (!tools.is_empty()).then(|| tools.clone());
            output.success(&application.tool_versions(tools).await?, &[])
        }
        ToolCommand::Probe { tool } => {
            output.success(&application.probe_tools(vec![tool.clone()])?, &[])
        }
        ToolCommand::Install { tool } | ToolCommand::Update { tool } => {
            require_online(cli, "tool lifecycle")?;
            let action = if matches!(command, ToolCommand::Install { .. }) {
                "install"
            } else {
                "update"
            };
            if cli.dry_run {
                let probe = application.probe_tools(vec![tool.clone()])?;
                output.success(
                    &json!({
                        "action": action,
                        "tool": tool,
                        "probe": probe,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                application
                    .run_tool_lifecycle(vec![tool.clone()], action)
                    .await?;
                output.success(
                    &json!({ "action": action, "tool": tool, "completed": true }),
                    &[],
                )
            }
        }
        ToolCommand::Terminal { tool, terminal } => {
            // Probe first so a typo or missing executable never opens an empty
            // terminal window.
            let versions = application.tool_versions(Some(vec![tool.clone()])).await?;
            let installed = versions
                .first()
                .is_some_and(|version| version.version.is_some());
            if !installed {
                return Err(CliError::Application(ApplicationError::DependencyMissing(
                    format!("tool is not installed or executable: {tool}"),
                )));
            }
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "open-tool-terminal",
                        "tool": tool,
                        "terminal": terminal,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            let target = terminal
                .clone()
                .or_else(|| ochub_core::settings::get_settings().preferred_terminal)
                .unwrap_or_else(|| "terminal".to_string());
            let tool = tool.clone();
            let command = tool.clone();
            tokio::task::spawn_blocking(move || {
                ochub_core::session_manager::terminal::launch_terminal(
                    &target, &command, None, None,
                )
            })
            .await
            .map_err(|error| {
                CliError::Application(ApplicationError::OperationFailed(error.to_string()))
            })?
            .map_err(|error| {
                CliError::Application(if error.contains("only supported") {
                    ApplicationError::PlatformUnsupported(error)
                } else {
                    ApplicationError::OperationFailed(error)
                })
            })?;
            output.success(&json!({ "opened": true, "tool": tool }), &[])
        }
    }
}

async fn run_skill(
    application: &Application,
    command: &SkillCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    use ochub_core::services::skill::DiscoverableSkill;

    match command {
        SkillCommand::List => output.success(&application.list_installed_skills()?, &[]),
        SkillCommand::Show { id } => output.success(&application.get_installed_skill(id)?, &[]),
        SkillCommand::Search {
            query,
            limit,
            offset,
        } => {
            require_online(cli, "skill search")?;
            output.success(
                &application.search_skills(query, *limit, *offset).await?,
                &[],
            )
        }
        SkillCommand::Discover { repo } => {
            require_online(cli, "skill discovery")?;
            let repo = repo.as_deref().map(parse_skill_repo_spec).transpose()?;
            output.success(&application.discover_skills(repo).await?, &[])
        }
        SkillCommand::Install { source, app } => {
            require_online(cli, "skill install")?;
            let path = std::path::Path::new(source);
            let skill: DiscoverableSkill = if path.is_file() {
                read_structured(path)?
            } else {
                parse_skill_source(source)?
            };
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "install-skill",
                        "skill": skill,
                        "app": app,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &application.install_skill(&skill, &app_id(app)?).await?,
                    &[],
                )
            }
        }
        SkillCommand::Uninstall { id } => {
            let target = application.get_installed_skill(id)?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "uninstall-skill",
                        "target": target,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "skill uninstall")?;
            output.success(&application.uninstall_skill(id).await?, &[])
        }
        SkillCommand::Check { id } => {
            application.get_installed_skill(id)?;
            let update = application
                .check_skill_updates()
                .await?
                .into_iter()
                .find(|update| update.id == *id);
            output.success(
                &json!({ "id": id, "updateAvailable": update.is_some(), "update": update }),
                &[],
            )
        }
        SkillCommand::CheckAll => output.success(&application.check_skill_updates().await?, &[]),
        SkillCommand::Update { id } => {
            require_online(cli, "skill update")?;
            if cli.dry_run {
                application.get_installed_skill(id)?;
                output.success(
                    &json!({ "action": "update-skill", "id": id, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(&application.update_skill(id).await?, &[])
            }
        }
        SkillCommand::UpdateAll => {
            require_online(cli, "skill update")?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "update-all-skills",
                        "count": application.list_installed_skills()?.len(),
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.update_all_skills().await?, &[])
            }
        }
        SkillCommand::Enable { id, app } | SkillCommand::Disable { id, app } => {
            let enabled = matches!(command, SkillCommand::Enable { .. });
            if cli.dry_run {
                application.get_installed_skill(id)?;
                output.success(
                    &json!({
                        "action": if enabled { "enable-skill" } else { "disable-skill" },
                        "id": id,
                        "app": app,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &application
                        .set_skill_app_enabled(id, &app_id(app)?, enabled)
                        .await?,
                    &[],
                )
            }
        }
        SkillCommand::Repo { command } => run_skill_repo(application, command, cli, output).await,
    }
}

async fn run_skill_repo(
    application: &Application,
    command: &SkillRepoCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        SkillRepoCommand::List => output.success(&application.list_skill_repos()?, &[]),
        SkillRepoCommand::Add {
            url,
            branch,
            enabled,
        } => {
            let mut repo = parse_skill_repo_spec(url)?;
            if let Some(branch) = branch {
                repo.branch = branch.clone();
            }
            repo.enabled = *enabled;
            if cli.dry_run {
                output.success(
                    &json!({ "action": "add-skill-repo", "repo": repo, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(&application.save_skill_repo(repo)?, &[])
            }
        }
        SkillRepoCommand::Update {
            id,
            branch,
            enabled,
        } => {
            let (owner, name) = split_repo_id(id)?;
            let mut repo = application.get_skill_repo(owner, name)?;
            if let Some(branch) = branch {
                repo.branch = branch.clone();
            }
            if let Some(enabled) = enabled {
                repo.enabled = *enabled;
            }
            if cli.dry_run {
                output.success(
                    &json!({ "action": "update-skill-repo", "repo": repo, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(&application.save_skill_repo(repo)?, &[])
            }
        }
        SkillRepoCommand::Remove { id } => {
            let (owner, name) = split_repo_id(id)?;
            let target = application.get_skill_repo(owner, name)?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "remove-skill-repo",
                        "target": target,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "skill repository remove")?;
            application.delete_skill_repo(owner, name)?;
            output.success(&json!({ "id": id, "deleted": true }), &[])
        }
        SkillRepoCommand::Catalog { id } => {
            require_online(cli, "skill repository catalog")?;
            let (owner, name) = split_repo_id(id)?;
            let repo = application.get_skill_repo(owner, name)?;
            output.success(&application.skill_catalog(Some(repo)).await?, &[])
        }
    }
}

fn run_mcp(
    application: &Application,
    command: &McpCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    use ochub_core::db::McpServer;

    match command {
        McpCommand::List => output.success(&application.list_mcp_servers(cli.show_secrets)?, &[]),
        McpCommand::Show { id } => {
            output.success(&application.get_mcp_server(id, cli.show_secrets)?, &[])
        }
        McpCommand::Add { from } => {
            let server: McpServer = read_structured(from)?;
            if cli.dry_run {
                application.validate_mcp_server(&server)?;
                output.success(
                    &json!({
                        "action": "add-mcp-server",
                        "resource": redacted_value(server)?,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.upsert_mcp_server(server)?, &[])
            }
        }
        McpCommand::Edit { id, from, patch } => {
            let mut server: McpServer = if let Some(path) = patch {
                let mut value = application.get_mcp_server(id, true)?;
                let patch: serde_json::Value = read_structured(path)?;
                merge_json_patch(&mut value, &patch);
                serde_json::from_value(value)?
            } else {
                let path = from.as_ref().ok_or_else(|| {
                    CliError::InvalidInput(
                        "mcp edit requires either --from <file> or --patch <file>".to_string(),
                    )
                })?;
                read_structured(path)?
            };
            server.id = id.clone();
            if cli.dry_run {
                application.validate_mcp_server(&server)?;
                output.success(
                    &json!({
                        "action": "edit-mcp-server",
                        "id": id,
                        "resource": redacted_value(server)?,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.upsert_mcp_server(server)?, &[])
            }
        }
        McpCommand::Delete { id } => {
            application.get_mcp_server(id, false)?;
            if cli.dry_run {
                return output.success(
                    &json!({ "action": "delete-mcp-server", "id": id, "dryRun": true }),
                    &[],
                );
            }
            require_yes(cli, "MCP server delete")?;
            application.delete_mcp_server(id)?;
            output.success(&json!({ "id": id, "deleted": true }), &[])
        }
        McpCommand::Validate { id, from } => {
            let server = match (id, from) {
                (Some(id), None) => {
                    let value = application.get_mcp_server(id, true)?;
                    serde_json::from_value(value)?
                }
                (None, Some(path)) => read_structured(path)?,
                _ => {
                    return Err(CliError::InvalidInput(
                        "provide exactly one MCP id or --from <file>".to_string(),
                    ));
                }
            };
            application.validate_mcp_server(&server)?;
            output.success(&json!({ "valid": true, "id": server.id }), &[])
        }
        McpCommand::Import { app } => {
            if cli.dry_run {
                output.success(
                    &json!({ "action": "import-mcp", "app": app, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(
                    &json!({
                        "app": app,
                        "imported": application.import_mcp_from_app(&app_id(app)?)?
                    }),
                    &[],
                )
            }
        }
        McpCommand::Enable { id, app } | McpCommand::Disable { id, app } => {
            let enabled = matches!(command, McpCommand::Enable { .. });
            if cli.dry_run {
                application.get_mcp_server(id, false)?;
                output.success(
                    &json!({
                        "action": if enabled { "enable-mcp" } else { "disable-mcp" },
                        "id": id,
                        "app": app,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &application.set_mcp_app_enabled(id, &app_id(app)?, enabled)?,
                    &[],
                )
            }
        }
        McpCommand::Sync { id, app } => {
            let apps = app
                .iter()
                .map(|raw| app_id(raw))
                .collect::<Result<Vec<_>, _>>()?;
            if cli.dry_run {
                application.get_mcp_server(id, false)?;
                output.success(
                    &json!({
                        "action": "sync-mcp",
                        "id": id,
                        "apps": apps,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.sync_mcp_server(id, &apps)?, &[])
            }
        }
        McpCommand::SyncAll => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "sync-all-mcp",
                        "count": application.list_mcp_servers(false)?.len(),
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &json!({ "synced": application.sync_all_mcp_servers()? }),
                    &[],
                )
            }
        }
    }
}

async fn run_app(
    application: &Application,
    command: &AppCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        AppCommand::List => output.success(&application.list_apps()?, &[]),
        AppCommand::Show { app } | AppCommand::Status { app } => {
            output.success(&application.get_app(&app_id(app)?)?, &[])
        }
        AppCommand::Enable { app } => {
            let id = app_id(app)?;
            if cli.dry_run {
                output.success(
                    &json!({ "action": "enable", "app": id, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(&application.set_app_enabled(&id, true).await?, &[])
            }
        }
        AppCommand::Disable { app } => {
            let id = app_id(app)?;
            if cli.dry_run {
                output.success(
                    &json!({ "action": "disable", "app": id, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(&application.set_app_enabled(&id, false).await?, &[])
            }
        }
        AppCommand::Schema { app, resource } => {
            if resource != "provider" {
                return Err(CliError::Application(
                    ApplicationError::CapabilityUnsupported {
                        app: app.clone(),
                        capability: "schema.resource",
                    },
                ));
            }
            output.success(&application.app_schema(&app_id(app)?)?, &[])
        }
        AppCommand::Path { command } => match command {
            AppPathCommand::Get { app } => {
                let summary = application.get_app(&app_id(app)?)?;
                output.success(
                    &json!({
                        "app": summary.id,
                        "configDir": summary.config_dir,
                        "error": summary.config_error
                    }),
                    &[],
                )
            }
            AppPathCommand::Set { app, path } => {
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "set-app-path",
                            "app": app,
                            "path": path,
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(
                        &application.set_app_config_dir(
                            &app_id(app)?,
                            Some(path.to_string_lossy().into_owned()),
                        )?,
                        &[],
                    )
                }
            }
            AppPathCommand::Reset { app } => {
                if cli.dry_run {
                    output.success(
                        &json!({ "action": "reset-app-path", "app": app, "dryRun": true }),
                        &[],
                    )
                } else {
                    output.success(&application.set_app_config_dir(&app_id(app)?, None)?, &[])
                }
            }
        },
    }
}

async fn run_settings(
    application: &Application,
    command: &SettingsCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        SettingsCommand::List => output.success(&application.settings(cli.show_secrets)?, &[]),
        SettingsCommand::Get { path } => {
            output.success(&application.get_setting(path, cli.show_secrets)?, &[])
        }
        SettingsCommand::Set {
            path,
            value,
            string,
            from,
        } => {
            let value = match (value, from) {
                (Some(value), None) => parse_value(value, *string),
                (None, Some(path)) => read_structured(path)?,
                (None, None) => {
                    return Err(CliError::InvalidInput(
                        "settings set requires VALUE or --from".to_string(),
                    ));
                }
                (Some(_), Some(_)) => {
                    return Err(CliError::InvalidInput(
                        "settings set accepts either VALUE or --from, not both".to_string(),
                    ));
                }
            };
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "set-setting",
                        "path": path,
                        "value": redact_json(&value),
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.set_setting(path, value)?, &[])
            }
        }
        SettingsCommand::Unset { path } => {
            if cli.dry_run {
                output.success(
                    &json!({ "action": "unset-setting", "path": path, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(&application.unset_setting(path)?, &[])
            }
        }
        SettingsCommand::Export { to } => {
            let settings = application.settings(cli.show_secrets)?;
            if let Some(path) = to {
                write_structured(path, &settings)?;
                output.success(&json!({ "path": path }), &[])
            } else {
                output.success(&settings, &[])
            }
        }
        SettingsCommand::Import { file } => {
            let settings: ochub_core::settings::AppSettings = read_structured(file)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "import-settings",
                        "file": file,
                        "settings": redact_json(&serde_json::to_value(settings)?),
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                ochub_core::settings::update_settings(settings)?;
                output.success(&application.settings(false)?, &[])
            }
        }
        SettingsCommand::Proxy { command } => match command {
            SettingsProxyCommand::Show => {
                output.success(&application.proxy_settings(cli.show_secrets), &[])
            }
            SettingsProxyCommand::Set { from } => {
                let proxy: ochub_core::settings::ProxySettings = read_structured(from)?;
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "set-proxy-settings",
                            "proxy": redact_json(&serde_json::to_value(proxy)?),
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(&application.set_proxy_settings(proxy)?, &[])
                }
            }
            SettingsProxyCommand::Test { from } => {
                require_online(cli, "proxy connection test")?;
                let proxy: ochub_core::settings::ProxySettings = read_structured(from)?;
                output.success(&application.test_proxy_settings(proxy).await?, &[])
            }
        },
    }
}

async fn run_provider(
    application: &Application,
    command: &ProviderCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        ProviderCommand::List { app } => {
            output.success(&application.list_providers(&app_id(app)?)?, &[])
        }
        ProviderCommand::Show { id, app } => output.success(
            &application.get_provider(&app_id(app)?, id, cli.show_secrets)?,
            &[],
        ),
        ProviderCommand::Current { app } => {
            let providers = application.list_providers(&app_id(app)?)?;
            let current = providers.into_iter().find(|provider| provider.current);
            output.success(&current, &[])
        }
        ProviderCommand::Add {
            app,
            from,
            set_values,
            secret_values,
            add_to_live,
        } => {
            let provider =
                provider_from_input(None, from.as_deref(), None, set_values, secret_values)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "add-provider",
                        "app": app,
                        "provider": redacted_provider_value(&provider)?,
                        "addToLive": add_to_live,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &application.add_provider(&app_id(app)?, provider, *add_to_live)?,
                    &[],
                )
            }
        }
        ProviderCommand::Edit {
            id,
            app,
            patch,
            from,
            set_values,
            secret_values,
        } => {
            let existing = application.get_provider(&app_id(app)?, id, true)?.provider;
            let provider = provider_from_input(
                Some(&existing),
                from.as_deref(),
                patch.as_deref(),
                set_values,
                secret_values,
            )?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "edit-provider",
                        "app": app,
                        "originalId": id,
                        "provider": redacted_provider_value(&provider)?,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &application.update_provider(&app_id(app)?, id, provider)?,
                    &[],
                )
            }
        }
        ProviderCommand::Delete { id, app } => {
            let target = application.get_provider(&app_id(app)?, id, false)?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "delete-provider",
                        "target": target,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "provider delete")?;
            application.delete_provider(&app_id(app)?, id)?;
            output.success(&json!({ "deleted": true, "app": app, "id": id }), &[])
        }
        ProviderCommand::Duplicate { id, app } => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "duplicate-provider",
                        "app": app,
                        "providerId": id,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.duplicate_provider(&app_id(app)?, id)?, &[])
            }
        }
        ProviderCommand::Export { id, app, to } => {
            let details = application.get_provider(&app_id(app)?, id, cli.show_secrets)?;
            if let Some(path) = to {
                write_structured(path, &details.provider)?;
                output.success(&json!({ "path": path }), &[])
            } else {
                output.success(&details, &[])
            }
        }
        ProviderCommand::SeedOfficial { app } => {
            let result = if cli.dry_run {
                json!({ "action": "seed-official", "app": app, "dryRun": true })
            } else {
                json!({ "created": application.seed_official_provider(&app_id(app)?)? })
            };
            output.success(&result, &[])
        }
        ProviderCommand::ImportLive { app } => {
            if cli.dry_run {
                output.success(
                    &json!({ "action": "import-live", "app": app, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(
                    &json!({ "imported": application.import_live_providers(&app_id(app)?)? }),
                    &[],
                )
            }
        }
        ProviderCommand::SyncLive { app, all } => {
            if app.is_none() && !all {
                return Err(CliError::InvalidInput(
                    "provider sync-live requires --app or --all".to_string(),
                ));
            }
            let apps = if *all {
                application
                    .list_apps()?
                    .into_iter()
                    .filter(|item| item.enabled && item.supports_provider)
                    .map(|item| item.id)
                    .collect::<Vec<_>>()
            } else {
                vec![app.clone().expect("validated --app")]
            };
            if cli.dry_run {
                output.success(
                    &json!({ "action": "sync-live", "apps": apps, "dryRun": true }),
                    &[],
                )
            } else {
                for app in &apps {
                    application.sync_live_provider(&app_id(app)?)?;
                }
                output.success(&json!({ "synced": true, "apps": apps }), &[])
            }
        }
        ProviderCommand::Preview { id, app } => output.success(
            &application.preview_provider_switch(&app_id(app)?, id)?,
            &[],
        ),
        ProviderCommand::Switch { id, app, on_drift } => {
            let app_id = app_id(app)?;
            if cli.dry_run {
                return output.success(&application.preview_provider_switch(&app_id, id)?, &[]);
            }
            let result = application.switch_provider(&app_id, id, policy(*on_drift))?;
            output.success(
                &json!({
                    "app": app,
                    "providerId": id,
                    "switched": true,
                    "drift": result.drift
                }),
                &result.warnings,
            )
        }
        ProviderCommand::AddToLive { id, app } => {
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "add-to-live",
                        "app": app,
                        "providerId": id,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            let result = application.add_provider_to_live(&app_id(app)?, id)?;
            output.success(
                &json!({ "app": app, "providerId": id, "addedToLive": true }),
                &result.warnings,
            )
        }
        ProviderCommand::RemoveFromLive { id, app } => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "remove-from-live",
                        "app": app,
                        "providerId": id,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                application.remove_provider_from_live(&app_id(app)?, id)?;
                output.success(
                    &json!({ "app": app, "providerId": id, "removedFromLive": true }),
                    &[],
                )
            }
        }
        ProviderCommand::Sort { app, ids } => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "sort-providers",
                        "app": app,
                        "ids": ids,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.sort_providers(&app_id(app)?, ids)?, &[])
            }
        }
        ProviderCommand::Copy {
            id,
            from_app,
            to_app,
        } => {
            if cli.dry_run {
                let source = application.get_provider(&app_id(from_app)?, id, false)?;
                output.success(
                    &json!({
                        "action": "copy-provider",
                        "sourceApp": from_app,
                        "targetApp": to_app,
                        "provider": source,
                        "usesTargetCodec": true,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &application.copy_provider(&app_id(from_app)?, &app_id(to_app)?, id)?,
                    &[],
                )
            }
        }
        ProviderCommand::Test { id, app } => {
            require_online(cli, "provider test")?;
            output.success(
                &application
                    .provider_test(&app_id(app)?, id, Some(cli.timeout))
                    .await?,
                &[],
            )
        }
        ProviderCommand::SpeedTest { id, app } => {
            require_online(cli, "provider speed test")?;
            output.success(
                &application
                    .provider_speed_test(&app_id(app)?, id, Some(cli.timeout))
                    .await?,
                &[],
            )
        }
        ProviderCommand::Models { id, app } => {
            require_online(cli, "provider model discovery")?;
            output.success(&application.provider_models(&app_id(app)?, id).await?, &[])
        }
        ProviderCommand::Balance { id, app } => {
            require_online(cli, "provider balance")?;
            output.success(&application.provider_balance(&app_id(app)?, id).await?, &[])
        }
        ProviderCommand::Quota { id, app } => {
            require_online(cli, "provider quota")?;
            output.success(&application.provider_quota(&app_id(app)?, id).await?, &[])
        }
        ProviderCommand::UsageScript { command } => match command {
            ProviderUsageScriptCommand::Run { id, app } => {
                require_online(cli, "provider usage script")?;
                output.success(
                    &application
                        .run_provider_usage_script(&app_id(app)?, id)
                        .await?,
                    &[],
                )
            }
            ProviderUsageScriptCommand::Test {
                app,
                provider,
                from,
            } => {
                require_online(cli, "provider usage script test")?;
                let script: UsageScript = read_structured(from)?;
                output.success(
                    &application
                        .test_provider_usage_script(&app_id(app)?, provider, &script)
                        .await?,
                    &[],
                )
            }
        },
        ProviderCommand::Terminal { id, app, cwd } => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "open-provider-terminal",
                        "app": app,
                        "providerId": id,
                        "cwd": cwd,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                application.open_provider_terminal(
                    &app_id(app)?,
                    id,
                    cwd.as_ref().map(|path| path.to_string_lossy().into_owned()),
                )?;
                output.success(
                    &json!({ "launched": true, "app": app, "providerId": id }),
                    &[],
                )
            }
        }
        ProviderCommand::Endpoint { command } => match command {
            ProviderEndpointCommand::List { id, app } => {
                output.success(&application.provider_endpoints(&app_id(app)?, id)?, &[])
            }
            ProviderEndpointCommand::Add { id, url, app } => {
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "add-provider-endpoint",
                            "app": app,
                            "providerId": id,
                            "url": url,
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(
                        &application.add_provider_endpoint(&app_id(app)?, id, url)?,
                        &[],
                    )
                }
            }
            ProviderEndpointCommand::Remove { id, url, app } => {
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "remove-provider-endpoint",
                            "app": app,
                            "providerId": id,
                            "url": url,
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(
                        &application.remove_provider_endpoint(&app_id(app)?, id, url)?,
                        &[],
                    )
                }
            }
        },
    }
}

fn run_config(
    application: &Application,
    command: &ConfigCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        ConfigCommand::Validate { file } => {
            output.success(&application.validate_declarative_file(file)?, &[])
        }
        ConfigCommand::Common { command } => match command {
            CommonConfigCommand::Get { app } => {
                output.success(&application.common_config(&app_id(app)?)?, &[])
            }
            CommonConfigCommand::Set { app, from } => {
                let snippet = read_text_limited(from, 1024 * 1024)?;
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "set-common-config",
                            "app": app,
                            "bytes": snippet.len(),
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    application.set_common_config(&app_id(app)?, snippet)?;
                    output.success(&json!({ "app": app, "saved": true }), &[])
                }
            }
            CommonConfigCommand::Extract { app } => {
                output.success(&application.extract_common_config(&app_id(app)?)?, &[])
            }
            CommonConfigCommand::Apply { app, provider } => {
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "apply-common-config",
                            "app": app,
                            "providerIds": provider,
                            "allProviders": provider.is_empty(),
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(
                        &application.apply_common_config(&app_id(app)?, provider)?,
                        &[],
                    )
                }
            }
        },
    }
}

fn run_declarative_plan(
    application: &Application,
    args: &DeclarativePlanArgs,
    output: &Output,
) -> Result<(), CliError> {
    output.success(
        &application.plan_declarative_file(&args.file, args.adopt, args.prune)?,
        &[],
    )
}

async fn run_declarative_apply(
    application: &Application,
    args: &DeclarativeApplyArgs,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    let plan = application.plan_declarative_file(&args.file, args.adopt, args.prune)?;
    if cli.dry_run {
        return output.success(&plan, &[]);
    }
    if plan.actions.iter().any(|action| action.action == "delete") {
        require_yes(cli, "declarative config delete/prune")?;
    }
    output.success(
        &application
            .apply_declarative_file(&args.file, args.adopt, args.prune)
            .await?,
        &[],
    )
}

async fn run_auth(
    application: &Application,
    command: &AuthCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        AuthCommand::Copilot { command } => match command {
            CopilotAuthCommand::Status => output.success(
                &application.managed_auth_status("github_copilot").await?,
                &[],
            ),
            CopilotAuthCommand::Login { github_domain } => {
                require_online(cli, "Copilot login")?;
                output.success(
                    &application
                        .managed_auth_login("github_copilot", github_domain.as_deref())
                        .await?,
                    &[],
                )
            }
            CopilotAuthCommand::Poll {
                flow_id,
                github_domain,
            } => {
                require_online(cli, "Copilot login polling")?;
                output.success(
                    &application
                        .managed_auth_poll("github_copilot", flow_id, github_domain.as_deref())
                        .await?,
                    &[],
                )
            }
            CopilotAuthCommand::Account { command } => {
                run_auth_account(application, "github_copilot", command, cli, output).await
            }
            CopilotAuthCommand::Token { account } => {
                if !cli.show_secrets {
                    return Err(CliError::InvalidInput(
                        "auth copilot token requires --show-secrets".to_string(),
                    ));
                }
                output.success(
                    &json!({
                        "token": application.copilot_token(account.as_deref()).await?
                    }),
                    &[],
                )
            }
            CopilotAuthCommand::Models { account } => {
                require_online(cli, "Copilot model discovery")?;
                output.success(&application.copilot_models(account.as_deref()).await?, &[])
            }
            CopilotAuthCommand::Usage { account } => {
                require_online(cli, "Copilot usage query")?;
                output.success(&application.copilot_usage(account.as_deref()).await?, &[])
            }
        },
        AuthCommand::Codex { command } => match command {
            CodexAuthCommand::Status => {
                output.success(&application.managed_auth_status("codex_oauth").await?, &[])
            }
            CodexAuthCommand::Login => {
                require_online(cli, "Codex OAuth login")?;
                output.success(
                    &application.managed_auth_login("codex_oauth", None).await?,
                    &[],
                )
            }
            CodexAuthCommand::Poll { flow_id } => {
                require_online(cli, "Codex OAuth login polling")?;
                output.success(
                    &application
                        .managed_auth_poll("codex_oauth", flow_id, None)
                        .await?,
                    &[],
                )
            }
            CodexAuthCommand::Logout { account } => {
                if cli.dry_run {
                    return output.success(
                        &json!({
                            "action": "logout-codex-oauth",
                            "accountId": account,
                            "allAccounts": account.is_none(),
                            "dryRun": true
                        }),
                        &[],
                    );
                }
                require_yes(cli, "Codex OAuth logout")?;
                if let Some(account) = account {
                    application
                        .remove_managed_auth_account("codex_oauth", account)
                        .await?;
                } else {
                    application.logout_managed_auth("codex_oauth").await?;
                }
                output.success(&json!({ "loggedOut": true, "accountId": account }), &[])
            }
            CodexAuthCommand::Account { command } => {
                run_auth_account(application, "codex_oauth", command, cli, output).await
            }
            CodexAuthCommand::Models { account } => {
                require_online(cli, "Codex OAuth model discovery")?;
                output.success(&application.codex_oauth_models(account.clone()).await?, &[])
            }
            CodexAuthCommand::Quota { account } => {
                require_online(cli, "Codex OAuth quota")?;
                output.success(&application.codex_oauth_quota(account.clone()).await?, &[])
            }
        },
        AuthCommand::Binding { command } => match command {
            AuthBindingCommand::List => {
                output.success(&application.list_auth_bindings().await?, &[])
            }
            AuthBindingCommand::Set {
                app,
                provider,
                account,
            } => {
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "set-auth-binding",
                            "app": app,
                            "providerId": provider,
                            "accountId": account,
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(
                        &application
                            .set_auth_binding(&app_id(app)?, provider, account)
                            .await?,
                        &[],
                    )
                }
            }
            AuthBindingCommand::Remove { app, provider } => {
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "remove-auth-binding",
                            "app": app,
                            "providerId": provider,
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(
                        &application.remove_auth_binding(&app_id(app)?, provider)?,
                        &[],
                    )
                }
            }
        },
    }
}

async fn run_auth_account(
    application: &Application,
    provider: &str,
    command: &AuthAccountCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        AuthAccountCommand::List => {
            output.success(&application.managed_auth_accounts(provider).await?, &[])
        }
        AuthAccountCommand::SetDefault { id } => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "set-default-auth-account",
                        "authProvider": provider,
                        "accountId": id,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                application
                    .set_default_managed_auth_account(provider, id)
                    .await?;
                output.success(
                    &json!({ "authProvider": provider, "accountId": id, "default": true }),
                    &[],
                )
            }
        }
        AuthAccountCommand::Remove { id } => {
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "remove-auth-account",
                        "authProvider": provider,
                        "accountId": id,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "managed auth account remove")?;
            application
                .remove_managed_auth_account(provider, id)
                .await?;
            output.success(
                &json!({ "authProvider": provider, "accountId": id, "removed": true }),
                &[],
            )
        }
    }
}

async fn run_quota(
    application: &Application,
    command: &QuotaCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    require_online(cli, "quota query")?;
    match command {
        QuotaCommand::Subscription { provider_id, app } => {
            application.get_provider(&app_id(app)?, provider_id, false)?;
            output.success(&application.subscription_quota(app).await?, &[])
        }
        QuotaCommand::CodingPlan { provider_id, app } => output.success(
            &application
                .coding_plan_quota(&app_id(app)?, provider_id)
                .await?,
            &[],
        ),
    }
}

fn run_claude_desktop(
    application: &Application,
    command: &ClaudeDesktopCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        ClaudeDesktopCommand::Status => output.success(&application.claude_desktop_status()?, &[]),
        ClaudeDesktopCommand::EnsureOfficial => {
            if cli.dry_run {
                output.success(
                    &json!({ "action": "ensure-claude-desktop-official", "dryRun": true }),
                    &[],
                )
            } else {
                output.success(
                    &json!({ "created": application.ensure_claude_desktop_official()? }),
                    &[],
                )
            }
        }
        ClaudeDesktopCommand::ImportFromClaude => {
            if cli.dry_run {
                output.success(
                    &json!({ "action": "import-claude-desktop-from-claude", "dryRun": true }),
                    &[],
                )
            } else {
                output.success(
                    &json!({ "imported": application.import_claude_desktop_from_claude()? }),
                    &[],
                )
            }
        }
    }
}

fn run_env(
    application: &Application,
    command: &EnvCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        EnvCommand::Scan => output.success(
            &application.scan_environment_conflicts(cli.show_secrets)?,
            &[],
        ),
        EnvCommand::Clean { conflict_id } => {
            let target = application
                .scan_environment_conflicts(false)?
                .into_iter()
                .find(|conflict| conflict["id"] == conflict_id.as_str())
                .ok_or_else(|| ApplicationError::NotFound {
                    kind: "environment-conflict",
                    id: conflict_id.clone(),
                })?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "clean-environment-conflict",
                        "target": target,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "environment conflict cleanup")?;
            output.success(
                &application.clean_environment_conflict(conflict_id)?,
                &[
                    "Restart the affected shell or application so its environment is refreshed."
                        .to_string(),
                ],
            )
        }
        EnvCommand::Restore { backup_id } => {
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "restore-environment-backup",
                        "backupId": backup_id,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "environment backup restore")?;
            output.success(&application.restore_environment_backup(backup_id)?, &[])
        }
    }
}

fn run_claude(
    application: &Application,
    command: &ClaudeCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        ClaudeCommand::Plugin { command } => match command {
            ClaudePluginCommand::Status => {
                output.success(&application.claude_plugin_status()?, &[])
            }
            ClaudePluginCommand::Show => {
                output.success(&application.claude_plugin_config(cli.show_secrets)?, &[])
            }
            ClaudePluginCommand::Apply { from } => {
                let spec: Value = read_structured(from)?;
                let official = spec
                    .get("official")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "apply-claude-plugin-config",
                            "source": from,
                            "official": official,
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(&application.apply_claude_plugin(official)?, &[])
                }
            }
            ClaudePluginCommand::Restore => {
                if cli.dry_run {
                    return output.success(
                        &json!({ "action": "restore-claude-plugin-config", "dryRun": true }),
                        &[],
                    );
                }
                require_yes(cli, "Claude plugin config restore")?;
                output.success(&application.restore_claude_plugin()?, &[])
            }
        },
        ClaudeCommand::Mcp { command } => match command {
            ClaudeMcpCommand::Status => output.success(&application.claude_mcp_status()?, &[]),
            ClaudeMcpCommand::Config { command } => match command {
                ClaudeMcpConfigCommand::Show => {
                    output.success(&application.claude_mcp_config(cli.show_secrets)?, &[])
                }
            },
            ClaudeMcpCommand::Server { command } => match command {
                ClaudeMcpServerCommand::Upsert { id, from } => {
                    let spec: Value = read_structured(from)?;
                    if cli.dry_run {
                        output.success(
                            &json!({
                                "action": "upsert-claude-mcp-server",
                                "id": id,
                                "spec": redact_json(&spec),
                                "dryRun": true
                            }),
                            &[],
                        )
                    } else {
                        output.success(&application.upsert_claude_mcp_server(id, spec)?, &[])
                    }
                }
                ClaudeMcpServerCommand::Delete { id } => {
                    if cli.dry_run {
                        return output.success(
                            &json!({
                                "action": "delete-claude-mcp-server",
                                "id": id,
                                "dryRun": true
                            }),
                            &[],
                        );
                    }
                    require_yes(cli, "Claude MCP server delete")?;
                    output.success(&application.delete_claude_mcp_server(id)?, &[])
                }
            },
            ClaudeMcpCommand::Path { command } => match command {
                ClaudeMcpPathCommand::Validate => {
                    output.success(&application.validate_claude_mcp_paths()?, &[])
                }
                ClaudeMcpPathCommand::ValidateCommand { command } => output.success(
                    &json!({
                        "command": command,
                        "valid": ochub_core::mcp::validate_command_in_path(command)?
                    }),
                    &[],
                ),
            },
            ClaudeMcpCommand::Onboarding { command } => match command {
                ClaudeOnboardingCommand::Status => {
                    output.success(&application.claude_onboarding_status()?, &[])
                }
                ClaudeOnboardingCommand::Skip | ClaudeOnboardingCommand::Clear => {
                    let completed = matches!(command, ClaudeOnboardingCommand::Skip);
                    if cli.dry_run {
                        output.success(
                            &json!({
                                "action": if completed { "skip-claude-onboarding" } else { "clear-claude-onboarding" },
                                "dryRun": true
                            }),
                            &[],
                        )
                    } else {
                        output.success(&application.set_claude_onboarding(completed)?, &[])
                    }
                }
            },
        },
    }
}

fn run_codex(
    application: &Application,
    command: &CodexCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        CodexCommand::History { command } => match command {
            CodexHistoryCommand::Status => {
                output.success(&application.codex_history_status()?, &[])
            }
            CodexHistoryCommand::Migrate => {
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "migrate-codex-history",
                            "status": application.codex_history_status()?,
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(&application.migrate_codex_history()?, &[])
                }
            }
            CodexHistoryCommand::Restore => {
                if cli.dry_run {
                    return output.success(
                        &json!({
                            "action": "restore-codex-history",
                            "status": application.codex_history_status()?,
                            "dryRun": true
                        }),
                        &[],
                    );
                }
                require_yes(cli, "Codex history restore")?;
                output.success(&application.restore_codex_history()?, &[])
            }
        },
    }
}

fn run_opencode(
    application: &Application,
    command: &OpencodeCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    let (slim, command) = match command {
        OpencodeCommand::Omo { command } => (false, command),
        OpencodeCommand::OmoSlim { command } => (true, command),
    };
    match command {
        OmoCommand::Status => output.success(&application.omo_status(slim)?, &[]),
        OmoCommand::Current => output.success(&application.omo_current(slim)?, &[]),
        OmoCommand::LocalFile => output.success(&application.omo_local_file(slim)?, &[]),
        OmoCommand::Disable => {
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "disable-omo",
                        "variant": if slim { "omo-slim" } else { "omo" },
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "OMO disable")?;
            output.success(&application.disable_omo(slim)?, &[])
        }
    }
}

fn run_openclaw(
    application: &Application,
    command: &OpenclawCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        OpenclawCommand::Health => output.success(&application.openclaw_health()?, &[]),
        OpenclawCommand::Models => output.success(&application.openclaw_models()?, &[]),
        OpenclawCommand::Model { command } => match command {
            OpenclawModelCommand::Default { command } => run_get_set(
                command,
                cli,
                output,
                || application.openclaw_default_model(),
                |value| application.set_openclaw_default_model(value),
                "set-openclaw-default-model",
            ),
        },
        OpenclawCommand::AgentDefaults { command } => run_get_set(
            command,
            cli,
            output,
            || application.openclaw_agent_defaults(),
            |value| application.set_openclaw_agent_defaults(value),
            "set-openclaw-agent-defaults",
        ),
        OpenclawCommand::Env { command } => run_get_set(
            command,
            cli,
            output,
            || application.openclaw_env(cli.show_secrets),
            |value| application.set_openclaw_env(value),
            "set-openclaw-env",
        ),
        OpenclawCommand::Tools { command } => run_get_set(
            command,
            cli,
            output,
            || application.openclaw_tools(),
            |value| application.set_openclaw_tools(value),
            "set-openclaw-tools",
        ),
    }
}

fn run_hermes(
    application: &Application,
    command: &HermesCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        HermesCommand::Models { command } => run_get_set(
            command,
            cli,
            output,
            || application.hermes_models(),
            |value| application.set_hermes_models(value),
            "set-hermes-models",
        ),
        HermesCommand::Memory { command } => match command {
            HermesMemoryCommand::Status => {
                output.success(&application.hermes_memory_status()?, &[])
            }
            HermesMemoryCommand::Limits => {
                output.success(&application.hermes_memory_limits()?, &[])
            }
            HermesMemoryCommand::Read { kind } => {
                output.success(&application.read_hermes_memory(kind.as_str())?, &[])
            }
            HermesMemoryCommand::Write { kind, from } => {
                let content = read_text_limited(from, 1024 * 1024)?;
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "write-hermes-memory",
                            "kind": kind.as_str(),
                            "source": from,
                            "characters": content.chars().count(),
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(
                        &application.write_hermes_memory(kind.as_str(), &content)?,
                        &[],
                    )
                }
            }
            HermesMemoryCommand::Enable { kind } | HermesMemoryCommand::Disable { kind } => {
                let enabled = matches!(command, HermesMemoryCommand::Enable { .. });
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": if enabled { "enable-hermes-memory" } else { "disable-hermes-memory" },
                            "kind": kind.as_str(),
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    output.success(
                        &application.set_hermes_memory_enabled(kind.as_str(), enabled)?,
                        &[],
                    )
                }
            }
        },
    }
}

fn run_theme(
    application: &Application,
    command: &ThemeCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        ThemeCommand::List => output.success(&application.list_themes()?, &[]),
        ThemeCommand::Show { id } => output.success(&application.get_theme(id)?, &[]),
        ThemeCommand::Validate { file } => {
            let theme = application.validate_theme_file(file)?;
            output.success(
                &json!({
                    "valid": true,
                    "path": file,
                    "theme": theme
                }),
                &[],
            )
        }
        ThemeCommand::Import { file } => {
            let theme = application.validate_theme_file(file)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "import-theme",
                        "source": file,
                        "theme": theme,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.import_theme(file)?, &[])
            }
        }
        ThemeCommand::Export { id, to } => {
            if cli.dry_run {
                let theme = application.get_theme(id)?;
                output.success(
                    &json!({
                        "action": "export-theme",
                        "theme": theme,
                        "path": to,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.export_theme(id, to.as_deref())?, &[])
            }
        }
        ThemeCommand::Duplicate { id } => {
            let source = application.get_theme(id)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "duplicate-theme",
                        "source": source,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.duplicate_theme(id)?, &[])
            }
        }
        ThemeCommand::Delete { id } => {
            let target = application.get_theme(id)?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "delete-theme",
                        "target": target,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "theme delete")?;
            output.success(
                &json!({
                    "deleted": true,
                    "id": id,
                    "path": application.delete_theme(id)?
                }),
                &[],
            )
        }
        ThemeCommand::Set { id } => {
            let target = application.get_theme(id)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "select-theme",
                        "target": target,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.set_theme_family(id)?, &[])
            }
        }
        ThemeCommand::Mode { mode } => {
            let mode = (*mode).into();
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "set-theme-mode",
                        "mode": mode,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.set_theme_mode(mode)?, &[])
            }
        }
    }
}

fn run_deeplink(
    application: &Application,
    command: &DeeplinkCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        DeeplinkCommand::Parse { uri } => {
            output.success(&application.parse_deeplink(uri, cli.show_secrets)?, &[])
        }
        DeeplinkCommand::Import { uri } => {
            let request = ochub_core::parse_deeplink_url(uri)?;
            if request.config_url.is_some() {
                require_online(cli, "deep link remote config import")?;
            }
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "import-deeplink",
                        "request": application.parse_deeplink(uri, false)?,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.import_deeplink(uri)?, &[])
            }
        }
    }
}

async fn run_update(
    application: &Application,
    command: &UpdateCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        UpdateCommand::Status => output.success(&application.update_status()?, &[]),
        UpdateCommand::Check => {
            require_online(cli, "update check")?;
            output.success(&application.check_for_update().await?, &[])
        }
        UpdateCommand::Install => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "install-update",
                        "status": application.update_status()?,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                require_online(cli, "update install")?;
                require_yes(cli, "update install")?;
                output.success(
                    &application.install_update().await?,
                    &["Exit this process cleanly to complete the restart.".to_string()],
                )
            }
        }
    }
}

fn run_get_set(
    command: &GetSetCommand,
    cli: &Cli,
    output: &Output,
    get: impl FnOnce() -> ApplicationResult<Value>,
    set: impl FnOnce(Value) -> ApplicationResult<Value>,
    action: &'static str,
) -> Result<(), CliError> {
    match command {
        GetSetCommand::Get => output.success(&get()?, &[]),
        GetSetCommand::Set { from } => {
            let value: Value = read_structured(from)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": action,
                        "source": from,
                        "value": redact_json(&value),
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&set(value)?, &[])
            }
        }
    }
}

async fn run_backup(
    application: &Application,
    command: &BackupCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    let db = &application.state().db;
    match command {
        BackupCommand::List => output.success(&ochub_core::Database::list_backups()?, &[]),
        BackupCommand::Create { name } => {
            if cli.dry_run {
                return output.success(
                    &json!({ "action": "create-backup", "name": name, "dryRun": true }),
                    &[],
                );
            }
            let mut filename = db.create_backup_file()?;
            if let Some(name) = name {
                filename = ochub_core::Database::rename_backup(&filename, name)?;
            }
            output.success(&json!({ "filename": filename }), &[])
        }
        BackupCommand::Rename { id, name } => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "rename-backup",
                        "id": id,
                        "name": name,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let filename = ochub_core::Database::rename_backup(id, name)?;
                output.success(&json!({ "filename": filename }), &[])
            }
        }
        BackupCommand::Restore { id } => {
            if cli.dry_run {
                return output.success(
                    &json!({ "action": "restore-backup", "id": id, "dryRun": true }),
                    &[],
                );
            }
            require_yes(cli, "backup restore")?;
            let safety_backup = db.restore_from_backup(id)?;
            output.success(
                &json!({ "restored": id, "safetyBackup": safety_backup }),
                &[],
            )
        }
        BackupCommand::Delete { id } => {
            if cli.dry_run {
                return output.success(
                    &json!({ "action": "delete-backup", "id": id, "dryRun": true }),
                    &[],
                );
            }
            require_yes(cli, "backup delete")?;
            ochub_core::Database::delete_backup(id)?;
            output.success(&json!({ "deleted": true, "id": id }), &[])
        }
        BackupCommand::ExportSql { file } => {
            db.export_sql(file)?;
            output.success(&json!({ "path": file }), &[])
        }
        BackupCommand::ImportSql { file } => {
            if cli.dry_run {
                return output.success(
                    &json!({ "action": "import-sql", "file": file, "dryRun": true }),
                    &[],
                );
            }
            require_yes(cli, "SQL import")?;
            let safety_backup = db.import_sql(file)?;
            let sync_warning =
                ochub_core::services::ProviderService::sync_current_to_live(application.state())
                    .err()
                    .map(|error| error.to_string());
            output.success(
                &json!({
                    "imported": file,
                    "safetyBackup": safety_backup,
                    "syncWarning": sync_warning
                }),
                &[],
            )
        }
        BackupCommand::Policy { command } => match command {
            BackupPolicyCommand::Show => output.success(
                &json!({
                    "intervalHours": ochub_core::settings::effective_backup_interval_hours(),
                    "retain": ochub_core::settings::effective_backup_retain_count()
                }),
                &[],
            ),
            BackupPolicyCommand::Set { interval, retain } => {
                let duration = parse_duration(interval)?;
                if duration.as_secs() < 3600 || duration.as_secs() % 3600 != 0 {
                    return Err(CliError::InvalidInput(
                        "backup interval must be a whole number of hours and at least 1h"
                            .to_string(),
                    ));
                }
                let hours = u32::try_from(duration.as_secs() / 3600).map_err(|_| {
                    CliError::InvalidInput("backup interval is too large".to_string())
                })?;
                if *retain == 0 || *retain > 1_000 {
                    return Err(CliError::InvalidInput(
                        "backup retain count must be between 1 and 1000".to_string(),
                    ));
                }
                if cli.dry_run {
                    output.success(
                        &json!({
                            "action": "set-backup-policy",
                            "intervalHours": hours,
                            "retain": retain,
                            "dryRun": true
                        }),
                        &[],
                    )
                } else {
                    ochub_core::settings::mutate_settings(|settings| {
                        settings.backup_interval_hours = Some(hours);
                        settings.backup_retain_count = Some(*retain);
                    })?;
                    output.success(&json!({ "intervalHours": hours, "retain": retain }), &[])
                }
            }
        },
    }
}

async fn run_gateway(
    application: &Application,
    command: &GatewayCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        GatewayCommand::Status => output.success(&application.gateway_status().await?, &[]),
        GatewayCommand::Start => {
            let status = application.start_gateway().await?;
            output.success(&status, &[])
        }
        GatewayCommand::Serve => {
            let status = application.start_gateway().await?;
            output.success(
                &status,
                &["Gateway is running in the foreground; press Ctrl-C to stop.".to_string()],
            )?;
            tokio::signal::ctrl_c().await?;
            application.stop_gateway().await?;
            Ok(())
        }
        GatewayCommand::Stop => {
            application.stop_gateway().await?;
            output.success(&json!({ "stopped": true }), &[])
        }
        GatewayCommand::Restart => {
            application.stop_gateway().await?;
            output.success(&application.start_gateway().await?, &[])
        }
        GatewayCommand::Health => output.success(&application.gateway_health().await?, &[]),
        GatewayCommand::Models => output.success(&application.gateway_models()?, &[]),
        GatewayCommand::SupportedApps => output.success(&application.gateway_supported_apps(), &[]),
        GatewayCommand::ConnectionInfo { app } => {
            if cli.dry_run {
                let config = application.gateway_config()?;
                return output.success(
                    &json!({
                        "action": "create-gateway-connection-info",
                        "app": app,
                        "baseUrl": format!("http://127.0.0.1:{}", config.port),
                        "createsOrReusesKey": true,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            let app = app.as_deref().map(app_id).transpose()?;
            let value = application.gateway_connection_info(app.as_ref())?;
            output.success(&maybe_redacted_value(value, cli.show_secrets)?, &[])
        }
        GatewayCommand::ProbeDialect {
            url,
            station,
            api_key_file,
        } => {
            require_online(cli, "gateway dialect probe")?;
            let api_key = gateway_endpoint_key(
                application,
                url,
                station.as_deref(),
                api_key_file.as_deref(),
            )?;
            output.success(
                &application.probe_gateway_dialects(url, &api_key).await?,
                &[],
            )
        }
        GatewayCommand::Endpoint { command } => {
            let (url, station, api_key_file, operation) = match command {
                GatewayEndpointCommand::Models {
                    url,
                    station,
                    api_key_file,
                } => (
                    url,
                    station,
                    api_key_file,
                    "gateway endpoint model discovery",
                ),
                GatewayEndpointCommand::Test {
                    url,
                    station,
                    api_key_file,
                } => (url, station, api_key_file, "gateway endpoint test"),
            };
            require_online(cli, operation)?;
            let api_key = gateway_endpoint_key(
                application,
                url,
                station.as_deref(),
                api_key_file.as_deref(),
            )?;
            match command {
                GatewayEndpointCommand::Models { .. } => output.success(
                    &application.gateway_endpoint_models(url, &api_key).await?,
                    &[],
                ),
                GatewayEndpointCommand::Test { .. } => output.success(
                    &application.test_gateway_endpoint(url, &api_key).await?,
                    &[],
                ),
            }
        }
        GatewayCommand::Config { command } => match command {
            GatewayConfigCommand::Show => output.success(&application.gateway_config()?, &[]),
            GatewayConfigCommand::Set {
                port,
                require_key,
                enabled,
                health_interval,
            } => {
                let mut config = application.gateway_config()?;
                if let Some(port) = port {
                    config.port = *port;
                }
                if let Some(require_key) = require_key {
                    config.require_key = *require_key;
                }
                if let Some(enabled) = enabled {
                    config.enabled = *enabled;
                }
                if let Some(interval) = health_interval {
                    config.health_interval_secs = *interval;
                }
                if cli.dry_run {
                    output.success(
                        &json!({ "action": "set-gateway-config", "config": config, "dryRun": true }),
                        &[],
                    )
                } else {
                    output.success(&application.set_gateway_config(config).await?, &[])
                }
            }
        },
        GatewayCommand::Channel { command } => {
            run_gateway_channel(application, command, cli, output).await
        }
        GatewayCommand::Route { command } => run_gateway_route(application, command, cli, output),
        GatewayCommand::Key { command } => run_gateway_key(application, command, cli, output),
    }
}

fn gateway_endpoint_key(
    application: &Application,
    url: &str,
    station: Option<&str>,
    api_key_file: Option<&std::path::Path>,
) -> Result<String, CliError> {
    if let Some(path) = api_key_file {
        return read_secret_file(path);
    }
    let Some(station) = station else {
        return Ok(String::new());
    };
    application
        .get_gateway_station(station)?
        .channels
        .into_iter()
        .find(|channel| channel.base_url.trim_end_matches('/') == url.trim_end_matches('/'))
        .map(|channel| channel.api_key)
        .ok_or_else(|| {
            CliError::InvalidInput(format!("station {station} has no endpoint matching {url}"))
        })
}

async fn run_gateway_channel(
    application: &Application,
    command: &GatewayChannelCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        GatewayChannelCommand::List => {
            let channels = application
                .list_gateway_channels()?
                .into_iter()
                .map(|channel| maybe_redacted_value(channel, cli.show_secrets))
                .collect::<Result<Vec<_>, _>>()?;
            output.success(&channels, &[])
        }
        GatewayChannelCommand::Show { id } => {
            let channel = application.get_gateway_channel(id)?;
            output.success(&maybe_redacted_value(channel, cli.show_secrets)?, &[])
        }
        GatewayChannelCommand::Add { from } => {
            let channel: GatewayChannel = read_structured(from)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "add-gateway-channel",
                        "resource": maybe_redacted_value(channel, false)?,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let saved = application.save_gateway_channel(channel)?;
                output.success(&maybe_redacted_value(saved, cli.show_secrets)?, &[])
            }
        }
        GatewayChannelCommand::Edit { id, from } => {
            application.get_gateway_channel(id)?;
            let mut channel: GatewayChannel = read_structured(from)?;
            channel.id = id.clone();
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "edit-gateway-channel",
                        "id": id,
                        "resource": maybe_redacted_value(channel, false)?,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let saved = application.save_gateway_channel(channel)?;
                output.success(&maybe_redacted_value(saved, cli.show_secrets)?, &[])
            }
        }
        GatewayChannelCommand::Delete { id } => {
            let target = application.get_gateway_channel(id)?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "delete-gateway-channel",
                        "target": maybe_redacted_value(target, false)?,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "Gateway channel delete")?;
            application.delete_gateway_channel(id)?;
            output.success(&json!({ "id": id, "deleted": true }), &[])
        }
        GatewayChannelCommand::Enable { id } | GatewayChannelCommand::Disable { id } => {
            let enabled = matches!(command, GatewayChannelCommand::Enable { .. });
            if cli.dry_run {
                application.get_gateway_channel(id)?;
                output.success(
                    &json!({
                        "action": "set-gateway-channel-enabled",
                        "id": id,
                        "enabled": enabled,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let saved = application.set_gateway_channel_enabled(id, enabled)?;
                output.success(&maybe_redacted_value(saved, cli.show_secrets)?, &[])
            }
        }
        GatewayChannelCommand::Probe { id } => {
            require_online(cli, "gateway channel probe")?;
            if let Some(id) = id {
                output.success(&application.probe_gateway_channel(id).await?, &[])
            } else {
                output.success(&application.probe_gateway_channels().await?, &[])
            }
        }
        GatewayChannelCommand::Models { id } => {
            require_online(cli, "gateway channel model discovery")?;
            output.success(&application.gateway_channel_models(id).await?, &[])
        }
        GatewayChannelCommand::ImportProvider { provider_id, app } => {
            if cli.dry_run {
                application.get_provider(&app_id(app)?, provider_id, false)?;
                output.success(
                    &json!({
                        "action": "import-provider-as-gateway-channel",
                        "app": app,
                        "providerId": provider_id,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let channel =
                    application.import_provider_as_gateway_channel(&app_id(app)?, provider_id)?;
                output.success(&maybe_redacted_value(channel, cli.show_secrets)?, &[])
            }
        }
    }
}

fn run_gateway_route(
    application: &Application,
    command: &GatewayRouteCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        GatewayRouteCommand::List => output.success(&application.list_gateway_routes()?, &[]),
        GatewayRouteCommand::Show { id } => {
            output.success(&application.get_gateway_route(id)?, &[])
        }
        GatewayRouteCommand::Add { from } => {
            let route: GatewayRoute = read_structured(from)?;
            if cli.dry_run {
                output.success(
                    &json!({ "action": "add-gateway-route", "resource": route, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(&application.save_gateway_route(route)?, &[])
            }
        }
        GatewayRouteCommand::Edit { id, from } => {
            application.get_gateway_route(id)?;
            let mut route: GatewayRoute = read_structured(from)?;
            route.id = id.clone();
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "edit-gateway-route",
                        "id": id,
                        "resource": route,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.save_gateway_route(route)?, &[])
            }
        }
        GatewayRouteCommand::Delete { id } => {
            let target = application.get_gateway_route(id)?;
            if cli.dry_run {
                return output.success(
                    &json!({ "action": "delete-gateway-route", "target": target, "dryRun": true }),
                    &[],
                );
            }
            require_yes(cli, "Gateway route delete")?;
            application.delete_gateway_route(id)?;
            output.success(&json!({ "id": id, "deleted": true }), &[])
        }
        GatewayRouteCommand::Enable { id } | GatewayRouteCommand::Disable { id } => {
            let enabled = matches!(command, GatewayRouteCommand::Enable { .. });
            if cli.dry_run {
                application.get_gateway_route(id)?;
                output.success(
                    &json!({
                        "action": "set-gateway-route-enabled",
                        "id": id,
                        "enabled": enabled,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(&application.set_gateway_route_enabled(id, enabled)?, &[])
            }
        }
        GatewayRouteCommand::Rule { command } => {
            run_gateway_route_rule(application, command, cli, output)
        }
    }
}

fn run_gateway_route_rule(
    application: &Application,
    command: &GatewayRouteRuleCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        GatewayRouteRuleCommand::List { route } => {
            output.success(&application.list_gateway_route_rules(route)?, &[])
        }
        GatewayRouteRuleCommand::Add { route, from } => {
            let rule: GatewayModelRule = read_structured(from)?;
            if cli.dry_run {
                application.get_gateway_route(route)?;
                output.success(
                    &json!({ "action": "add-gateway-route-rule", "route": route, "rule": rule, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(&application.add_gateway_route_rule(route, rule)?, &[])
            }
        }
        GatewayRouteRuleCommand::Edit { model, route, from } => {
            let rule: GatewayModelRule = read_structured(from)?;
            if cli.dry_run {
                application.list_gateway_route_rules(route)?;
                output.success(
                    &json!({ "action": "edit-gateway-route-rule", "route": route, "model": model, "rule": rule, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(
                    &application.update_gateway_route_rule(route, model, rule)?,
                    &[],
                )
            }
        }
        GatewayRouteRuleCommand::Delete { model, route } => {
            application.list_gateway_route_rules(route)?;
            if cli.dry_run {
                return output.success(
                    &json!({ "action": "delete-gateway-route-rule", "route": route, "model": model, "dryRun": true }),
                    &[],
                );
            }
            require_yes(cli, "Gateway route rule delete")?;
            output.success(&application.delete_gateway_route_rule(route, model)?, &[])
        }
        GatewayRouteRuleCommand::Sort { route, models } => {
            if cli.dry_run {
                application.list_gateway_route_rules(route)?;
                output.success(
                    &json!({ "action": "sort-gateway-route-rules", "route": route, "models": models, "dryRun": true }),
                    &[],
                )
            } else {
                output.success(&application.sort_gateway_route_rules(route, models)?, &[])
            }
        }
    }
}

fn run_gateway_key(
    application: &Application,
    command: &GatewayKeyCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        GatewayKeyCommand::List => {
            let keys = application
                .list_gateway_keys()?
                .into_iter()
                .map(|key| maybe_redacted_value(key, cli.show_secrets))
                .collect::<Result<Vec<_>, _>>()?;
            output.success(&keys, &[])
        }
        GatewayKeyCommand::Show { id } => {
            let key = application.get_gateway_key(id)?;
            output.success(&maybe_redacted_value(key, cli.show_secrets)?, &[])
        }
        GatewayKeyCommand::Create { name, route } => {
            if cli.dry_run {
                if let Some(route) = route {
                    application.get_gateway_route(route)?;
                }
                output.success(
                    &json!({
                        "action": "create-gateway-key",
                        "name": name,
                        "routeId": route,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let key = application.create_gateway_key(name, route.as_deref())?;
                output.success(&maybe_redacted_value(key, cli.show_secrets)?, &[])
            }
        }
        GatewayKeyCommand::Revoke { id } => {
            let key = application.get_gateway_key(id)?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "revoke-gateway-key",
                        "target": maybe_redacted_value(key, false)?,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "Gateway key revoke")?;
            let key = application.revoke_gateway_key(id)?;
            output.success(&maybe_redacted_value(key, false)?, &[])
        }
        GatewayKeyCommand::Bind { id, route, clear } => {
            let route = if *clear { None } else { route.as_deref() };
            if !clear && route.is_none() {
                return Err(CliError::InvalidInput(
                    "gateway key bind requires --route or --clear".to_string(),
                ));
            }
            if cli.dry_run {
                application.get_gateway_key(id)?;
                if let Some(route) = route {
                    application.get_gateway_route(route)?;
                }
                output.success(
                    &json!({
                        "action": "bind-gateway-key",
                        "id": id,
                        "routeId": route,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let key = application.bind_gateway_key(id, route)?;
                output.success(&maybe_redacted_value(key, cli.show_secrets)?, &[])
            }
        }
    }
}

async fn run_station(
    application: &Application,
    command: &StationCommand,
    cli: &Cli,
    output: &Output,
) -> Result<(), CliError> {
    match command {
        StationCommand::List => {
            let stations = application
                .list_gateway_stations()?
                .into_iter()
                .map(|station| maybe_redacted_value(station, cli.show_secrets))
                .collect::<Result<Vec<_>, _>>()?;
            output.success(&stations, &[])
        }
        StationCommand::Show { id } => output.success(
            &maybe_redacted_value(application.get_gateway_station(id)?, cli.show_secrets)?,
            &[],
        ),
        StationCommand::Add { from } => {
            let station: GatewayStation = read_structured(from)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "add-gateway-station",
                        "resource": maybe_redacted_value(station, false)?,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let station = application.save_gateway_station(station)?;
                output.success(&maybe_redacted_value(station, cli.show_secrets)?, &[])
            }
        }
        StationCommand::Edit { id, patch } => {
            let current = application.get_gateway_station(id)?;
            let mut value = serde_json::to_value(current)?;
            let mut patch: Value = read_structured(patch)?;
            restore_redacted_secrets(&mut patch, &value);
            merge_json_patch(&mut value, &patch);
            value["id"] = Value::String(id.clone());
            let station: GatewayStation = serde_json::from_value(value)?;
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "edit-gateway-station",
                        "id": id,
                        "resource": maybe_redacted_value(station, false)?,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let station = application.save_gateway_station(station)?;
                output.success(&maybe_redacted_value(station, cli.show_secrets)?, &[])
            }
        }
        StationCommand::Delete { id } => {
            let target = application.get_gateway_station(id)?;
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "delete-gateway-station",
                        "target": maybe_redacted_value(target, false)?,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "Gateway station delete")?;
            application.delete_gateway_station(id)?;
            output.success(&json!({ "id": id, "deleted": true }), &[])
        }
        StationCommand::Enable { id } | StationCommand::Disable { id } => {
            let enabled = matches!(command, StationCommand::Enable { .. });
            if cli.dry_run {
                application.get_gateway_station(id)?;
                output.success(
                    &json!({
                        "action": "set-gateway-station-enabled",
                        "id": id,
                        "enabled": enabled,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let station = application.set_gateway_station_enabled(id, enabled)?;
                output.success(&maybe_redacted_value(station, cli.show_secrets)?, &[])
            }
        }
        StationCommand::Probe { id } => {
            require_online(cli, "gateway station probe")?;
            output.success(&application.probe_gateway_station(id).await?, &[])
        }
        StationCommand::Models { id } => {
            output.success(&application.gateway_station_models(id)?, &[])
        }
        StationCommand::Select { id, app } => {
            if cli.dry_run {
                application.get_gateway_station(id)?;
                output.success(
                    &json!({
                        "action": "select-gateway-station",
                        "id": id,
                        "app": app,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let key = application.select_gateway_station(id, &app_id(app)?)?;
                output.success(&maybe_redacted_value(key, cli.show_secrets)?, &[])
            }
        }
        StationCommand::Apply { id, app, from } => {
            let policy = from
                .as_deref()
                .map(read_structured::<GatewayAppModelPolicy>)
                .transpose()?;
            if cli.dry_run {
                let station = application.get_gateway_station(id)?;
                output.success(
                    &json!({
                        "action": "apply-gateway-station",
                        "id": id,
                        "app": app,
                        "station": maybe_redacted_value(station, false)?,
                        "modelPolicy": policy,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                let result = application.apply_gateway_station(id, &app_id(app)?, policy)?;
                output.success(&maybe_redacted_value(result, cli.show_secrets)?, &[])
            }
        }
        StationCommand::Disconnect { app } => {
            if cli.dry_run {
                output.success(
                    &json!({
                        "action": "disconnect-gateway-from-app",
                        "app": app,
                        "dryRun": true
                    }),
                    &[],
                )
            } else {
                output.success(
                    &application.disconnect_gateway_from_app(&app_id(app)?)?,
                    &[],
                )
            }
        }
        StationCommand::ConnectionInfo { id, app } => {
            if cli.dry_run {
                application.get_gateway_station(id)?;
                return output.success(
                    &json!({
                        "action": "create-station-connection-info",
                        "id": id,
                        "app": app,
                        "createsOrReusesKey": true,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            let value = application.gateway_station_connection_info(id, &app_id(app)?)?;
            output.success(&maybe_redacted_value(value, cli.show_secrets)?, &[])
        }
    }
}

fn app_id(raw: &str) -> Result<AppId, CliError> {
    AppId::parse(raw).map_err(CliError::Core)
}

fn policy(policy: DriftPolicyArg) -> ProviderSwitchPolicy {
    match policy {
        DriftPolicyArg::Abort => ProviderSwitchPolicy::Abort,
        DriftPolicyArg::Preserve => ProviderSwitchPolicy::Preserve,
        DriftPolicyArg::Discard => ProviderSwitchPolicy::Discard,
    }
}

fn require_yes(cli: &Cli, operation: &str) -> Result<(), CliError> {
    if cli.yes {
        Ok(())
    } else {
        Err(CliError::InvalidInput(format!(
            "{operation} is destructive; rerun with --yes after reviewing --dry-run"
        )))
    }
}

fn require_online(cli: &Cli, operation: &str) -> Result<(), CliError> {
    if cli.offline {
        Err(CliError::InvalidInput(format!(
            "{operation} requires network access and cannot run with --offline"
        )))
    } else {
        Ok(())
    }
}

fn split_repo_id(id: &str) -> Result<(&str, &str), CliError> {
    let (owner, name) = id
        .split_once('/')
        .ok_or_else(|| CliError::InvalidInput("repository id must use owner/name".to_string()))?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return Err(CliError::InvalidInput(
            "repository id must use owner/name".to_string(),
        ));
    }
    Ok((owner, name))
}

fn not_found(kind: &'static str, id: &str) -> CliError {
    CliError::Application(ApplicationError::NotFound {
        kind,
        id: id.to_string(),
    })
}

fn redacted_provider_value(provider: &Provider) -> Result<Value, CliError> {
    Ok(redact_json(&serde_json::to_value(provider)?))
}

fn redacted_value<T: Serialize>(value: T) -> Result<Value, CliError> {
    Ok(redact_json(&serde_json::to_value(value)?))
}

fn maybe_redacted_value<T: Serialize>(value: T, show_secrets: bool) -> Result<Value, CliError> {
    let value = serde_json::to_value(value)?;
    Ok(if show_secrets {
        value
    } else {
        redact_json(&value)
    })
}

fn provider_from_input(
    base: Option<&Provider>,
    from: Option<&std::path::Path>,
    patch: Option<&std::path::Path>,
    set_values: &[String],
    secret_values: &[String],
) -> Result<Provider, CliError> {
    if base.is_none()
        && from.is_none()
        && patch.is_none()
        && set_values.is_empty()
        && secret_values.is_empty()
    {
        return Err(CliError::InvalidInput(
            "provider add requires --from, --set, or --secret".to_string(),
        ));
    }
    let mut value = match from {
        Some(path) => read_structured::<Value>(path)?,
        None => base
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or_else(|| Value::Object(Default::default())),
    };
    if let Some(path) = patch {
        let patch = read_structured::<Value>(path)?;
        merge_json_patch(&mut value, &patch);
    }
    for assignment in set_values {
        let (path, raw) = split_assignment(assignment, "--set")?;
        set_dotted_value(&mut value, path, parse_value(raw, false))?;
    }
    for assignment in secret_values {
        let (path, source) = split_assignment(assignment, "--secret")?;
        let source = source.strip_prefix('@').ok_or_else(|| {
            CliError::InvalidInput(
                "--secret requires FIELD=@PATH so the value does not appear in argv".to_string(),
            )
        })?;
        if source.is_empty() || source == "-" {
            return Err(CliError::InvalidInput(
                "--secret stdin is not supported; provide a regular file path".to_string(),
            ));
        }
        set_dotted_value(
            &mut value,
            path,
            Value::String(read_secret_file(std::path::Path::new(source))?),
        )?;
    }
    serde_json::from_value(value)
        .map_err(|error| CliError::InvalidInput(format!("invalid Provider input: {error}")))
}

fn split_assignment<'a>(assignment: &'a str, option: &str) -> Result<(&'a str, &'a str), CliError> {
    let (path, value) = assignment
        .split_once('=')
        .ok_or_else(|| CliError::InvalidInput(format!("{option} requires FIELD=VALUE")))?;
    if path.trim().is_empty() {
        return Err(CliError::InvalidInput(format!(
            "{option} field cannot be empty"
        )));
    }
    Ok((path.trim(), value))
}

fn set_dotted_value(target: &mut Value, path: &str, replacement: Value) -> Result<(), CliError> {
    let parts = path
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let Some((last, parents)) = parts.split_last() else {
        return Err(CliError::InvalidInput(
            "Provider field path cannot be empty".to_string(),
        ));
    };
    let mut current = target;
    for part in parents {
        if current.is_null() {
            *current = Value::Object(Default::default());
        }
        let object = current.as_object_mut().ok_or_else(|| {
            CliError::InvalidInput(format!("Provider field parent is not an object: {path}"))
        })?;
        current = object
            .entry((*part).to_string())
            .or_insert_with(|| Value::Object(Default::default()));
    }
    if current.is_null() {
        *current = Value::Object(Default::default());
    }
    current
        .as_object_mut()
        .ok_or_else(|| {
            CliError::InvalidInput(format!("Provider field parent is not an object: {path}"))
        })?
        .insert((*last).to_string(), replacement);
    Ok(())
}

fn read_secret_file(path: &std::path::Path) -> Result<String, CliError> {
    const MAX_SECRET_SIZE: u64 = 64 * 1024;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(CliError::InvalidInput(format!(
            "secret source must be a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_SECRET_SIZE {
        return Err(CliError::InvalidInput(format!(
            "secret file exceeds {MAX_SECRET_SIZE} bytes: {}",
            path.display()
        )));
    }
    let value = std::fs::read_to_string(path)?;
    Ok(value.trim_end_matches(['\r', '\n']).to_string())
}

fn merge_json_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Default::default());
    }
    let target = target
        .as_object_mut()
        .expect("target initialized as JSON object");
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            merge_json_patch(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

fn restore_redacted_secrets(patch: &mut Value, current: &Value) {
    match (patch, current) {
        (Value::Object(patch), Value::Object(current)) => {
            for (key, value) in patch {
                let existing = current.get(key).unwrap_or(&Value::Null);
                if station_secret_key(key)
                    && value.as_str().is_some_and(|value| {
                        !value.is_empty() && value.chars().all(|character| character == '*')
                    })
                {
                    *value = existing.clone();
                } else {
                    restore_redacted_secrets(value, existing);
                }
            }
        }
        (Value::Array(patch), Value::Array(current)) => {
            for (index, value) in patch.iter_mut().enumerate() {
                let existing = value
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| {
                        current.iter().find(|candidate| {
                            candidate.get("id").and_then(Value::as_str) == Some(id)
                        })
                    })
                    .or_else(|| current.get(index))
                    .unwrap_or(&Value::Null);
                restore_redacted_secrets(value, existing);
            }
        }
        _ => {}
    }
}

fn station_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '.'], "_");
    key.contains("password")
        || key.contains("secret")
        || key.contains("token")
        || key == "api_key"
        || key == "apikey"
        || key.ends_with("_api_key")
}

#[allow(dead_code)]
fn _path_for_output(path: PathBuf) -> Value {
    Value::String(path.to_string_lossy().into_owned())
}

#[allow(dead_code)]
fn _application_result<T>(result: ApplicationResult<T>) -> Result<T, CliError> {
    result.map_err(CliError::from)
}

#[cfg(test)]
mod tests {
    use super::{merge_json_patch, restore_redacted_secrets};
    use serde_json::json;

    #[test]
    fn provider_merge_patch_preserves_omitted_secrets() {
        let mut provider = json!({
            "id": "team",
            "name": "Team",
            "settingsConfig": {
                "apiKey": "sk-existing",
                "baseUrl": "https://old.example.com"
            }
        });
        merge_json_patch(
            &mut provider,
            &json!({
                "name": "Team Updated",
                "settingsConfig": {
                    "baseUrl": "https://new.example.com"
                }
            }),
        );
        assert_eq!(provider["name"], "Team Updated");
        assert_eq!(provider["settingsConfig"]["apiKey"], "sk-existing");
        assert_eq!(
            provider["settingsConfig"]["baseUrl"],
            "https://new.example.com"
        );
    }

    #[test]
    fn station_patch_preserves_redacted_secrets_inside_channel_arrays() {
        let current = json!({
            "channels": [
                { "id": "chat", "api_key": "real-secret", "base_url": "https://old.example" }
            ]
        });
        let mut patch = json!({
            "channels": [
                { "id": "chat", "api_key": "******", "base_url": "https://new.example" }
            ]
        });
        restore_redacted_secrets(&mut patch, &current);
        assert_eq!(patch["channels"][0]["api_key"], "real-secret");
        assert_eq!(patch["channels"][0]["base_url"], "https://new.example");
    }
}
