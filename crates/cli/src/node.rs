//! Managed installation and signed self-update for headless OcHub nodes.
//!
//! `ochcli` is intentionally the only required executable. The daemon service
//! runs `ochcli daemon run` through a stable `current` symlink, so a release
//! cannot leave the command and its background owner at different versions.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use clap::Parser as _;
use fs2::FileExt as _;
use ochub_core::application::ApplicationError;
use ochub_core::runtime::{self, OwnerKind};
use ochub_core::services::update::headless::{
    self, HeadlessPlatformEntry, HeadlessUpdateCheck, MAX_PAYLOAD_BYTES,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;

use crate::command::{Cli, NodeCommand, NodeUpdateCommand};
use crate::daemon::ServiceResumeMode;
use crate::error::CliError;
use crate::output::Output;

const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub(crate) struct RemoteNodeOptions {
    socket: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    timeout: u64,
}

impl RemoteNodeOptions {
    pub(crate) fn from_cli(cli: &Cli) -> Self {
        Self {
            socket: cli.socket.clone(),
            data_dir: cli.data_dir.clone(),
            timeout: cli.timeout,
        }
    }

    fn cli(&self) -> Result<Cli, CliError> {
        let mut cli = Cli::try_parse_from(["ochcli", "--yes", "node", "update", "install"])
            .map_err(|error| CliError::InvalidInput(error.to_string()))?;
        cli.socket = self.socket.clone();
        cli.data_dir = self.data_dir.clone();
        cli.timeout = self.timeout;
        Ok(cli)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ManagedState {
    schema_version: u32,
    active_version: String,
    previous_version: Option<String>,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeInstallStatus {
    pub managed: bool,
    pub current_version: String,
    pub active_version: Option<String>,
    pub previous_version: Option<String>,
    pub target: Option<String>,
    pub managed_root: PathBuf,
    pub executable: PathBuf,
    pub command_link: PathBuf,
    pub service_mode: String,
    pub service_definition: Option<PathBuf>,
    pub daemon: Value,
    pub can_self_update: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeUpdateReport {
    pub installation: NodeInstallStatus,
    pub update: HeadlessUpdateCheck,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NodeUpdateInstallResult {
    pub updated: bool,
    pub strategy: String,
    pub from_version: String,
    pub version: String,
    pub target: String,
    pub executable: PathBuf,
    pub rolled_back: bool,
    pub daemon: Value,
}

pub async fn execute(cli: &Cli, command: &NodeCommand, output: &Output) -> Result<(), CliError> {
    match command {
        NodeCommand::Install => {
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "install-managed-node",
                        "source": std::env::current_exe()?,
                        "managedRoot": managed_root(),
                        "entrypoint": managed_entrypoint_path(),
                        "service": crate::daemon::service_plan()?,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            let (status, warnings) = install_current(cli).await?;
            output.success(&status, &warnings)
        }
        NodeCommand::Status => output.success(&status().await?, &[]),
        NodeCommand::Update { command } => match command {
            NodeUpdateCommand::Check => {
                if cli.offline {
                    return Err(CliError::InvalidInput(
                        "node update check is unavailable with --offline".to_string(),
                    ));
                }
                output.success(&check_for_update(true).await?, &[])
            }
            NodeUpdateCommand::Install => {
                if cli.dry_run {
                    return output.success(
                        &json!({
                            "action": "install-node-update",
                            "check": check_for_update(true).await?,
                            "strategy": "direct",
                            "dryRun": true
                        }),
                        &[],
                    );
                }
                require_yes(cli, "node update install")?;
                if cli.offline {
                    return Err(CliError::InvalidInput(
                        "direct node update is unavailable with --offline".to_string(),
                    ));
                }
                output.success(&install_direct(cli).await?, &[])
            }
            NodeUpdateCommand::Receive {
                version,
                target,
                signature,
                sha256,
                size,
                expected_node_id,
            } => {
                require_yes(cli, "relayed node update")?;
                output.success(
                    &receive_update(
                        cli,
                        version,
                        target,
                        signature,
                        sha256,
                        *size,
                        expected_node_id,
                    )
                    .await?,
                    &[],
                )
            }
        },
        NodeCommand::Rollback => {
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "rollback-node-update",
                        "installation": status().await?,
                        "dryRun": true
                    }),
                    &[],
                );
            }
            require_yes(cli, "node rollback")?;
            output.success(&rollback(cli).await?, &[])
        }
    }
}

pub(crate) fn managed_root() -> PathBuf {
    managed_root_for(&ochub_core::paths::get_home_dir())
}

fn managed_root_for(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/OcHub/cli")
    }
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("XDG_DATA_HOME").map_or_else(
            || home.join(".local/share/ochub/cli"),
            |path| PathBuf::from(path).join("ochub/cli"),
        )
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map_or_else(
            || home.join("AppData/Local/OcHub/cli"),
            |path| PathBuf::from(path).join("OcHub/cli"),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        home.join(".ochub/cli")
    }
}

fn executable_name() -> &'static str {
    if cfg!(windows) {
        "ochcli.exe"
    } else {
        "ochcli"
    }
}

fn managed_entrypoint_path() -> PathBuf {
    managed_root().join("current").join(executable_name())
}

pub(crate) fn managed_entrypoint() -> Option<PathBuf> {
    let path = managed_entrypoint_path();
    path.is_file().then_some(path)
}

fn command_link() -> PathBuf {
    let home = ochub_core::paths::get_home_dir();
    if cfg!(windows) {
        managed_entrypoint_path()
    } else {
        home.join(".local/bin/ochcli")
    }
}

fn state_path(root: &Path) -> PathBuf {
    root.join("state.json")
}

fn read_state(root: &Path) -> Result<Option<ManagedState>, CliError> {
    let path = state_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let state: ManagedState = serde_json::from_slice(&fs::read(&path)?)?;
    if state.schema_version != STATE_SCHEMA_VERSION {
        return Err(CliError::InvalidInput(format!(
            "managed node state schema {} is unsupported",
            state.schema_version
        )));
    }
    Ok(Some(state))
}

fn write_state(root: &Path, state: &ManagedState) -> Result<(), CliError> {
    let path = state_path(root);
    ochub_core::paths::atomic_write(&path, &serde_json::to_vec_pretty(state)?)
        .map_err(CliError::Core)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub(crate) async fn status() -> Result<NodeInstallStatus, CliError> {
    let root = managed_root();
    let state = read_state(&root)?;
    let entrypoint = root.join("current").join(executable_name());
    let service_definition = crate::daemon::service_definition_path().ok();
    let service_mode = if service_definition
        .as_ref()
        .is_some_and(|path| path.exists())
        && crate::daemon::user_service_available()
    {
        if cfg!(target_os = "macos") {
            "launchd"
        } else {
            "systemd-user"
        }
    } else if runtime::active_owner()?.is_some_and(|owner| owner.kind == OwnerKind::Daemon) {
        "background"
    } else {
        "not-installed"
    };
    let daemon = match runtime::active_owner()? {
        Some(owner) => match crate::runtime_client::ping(None, 2).await {
            Ok(response) => json!({
                "running": true,
                "owner": owner,
                "runtime": response.data
            }),
            Err(error) => json!({
                "running": false,
                "owner": owner,
                "error": error.to_string()
            }),
        },
        None => json!({ "running": false }),
    };
    Ok(NodeInstallStatus {
        managed: entrypoint.is_file(),
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        active_version: state.as_ref().map(|state| state.active_version.clone()),
        previous_version: state.and_then(|state| state.previous_version),
        target: headless::current_target_key(),
        managed_root: root,
        executable: entrypoint,
        command_link: command_link(),
        service_mode: service_mode.to_string(),
        service_definition,
        daemon,
        can_self_update: managed_updates_supported()
            && ochub_core::services::update::manifest::signing_configured(),
    })
}

pub(crate) async fn check_for_update(probe_direct: bool) -> Result<NodeUpdateReport, CliError> {
    let installation = status().await?;
    let update = headless::check(
        None,
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        probe_direct,
    )
    .await?;
    Ok(NodeUpdateReport {
        installation,
        update,
    })
}

async fn install_current(cli: &Cli) -> Result<(NodeInstallStatus, Vec<String>), CliError> {
    ensure_supported_platform()?;
    let _update_lock = acquire_update_lock(&managed_root())?;
    let _ = garbage_collect_managed_root(&managed_root());
    let current = std::env::current_exe()?;
    let version = env!("CARGO_PKG_VERSION");
    let payload = fs::read(&current)?;
    stage_version(&managed_root(), version, &payload, true)?;

    let _resume = crate::daemon::suspend_for_update(cli).await?;
    activate_version(&managed_root(), version)?;
    install_command_link(&managed_root())?;
    let _ = garbage_collect_managed_root(&managed_root());

    let mut warnings = Vec::new();
    if crate::daemon::user_service_available() {
        if let Err(error) =
            crate::daemon::resume_after_update(cli, ServiceResumeMode::InstalledService).await
        {
            let _ = crate::daemon::resume_after_update(cli, ServiceResumeMode::Background).await?;
            warnings.push(format!(
                "could not install the user service ({error}); started a background daemon instead"
            ));
        }
    } else {
        let _ = crate::daemon::resume_after_update(cli, ServiceResumeMode::Background).await?;
        warnings.push(
            "no user service manager is available; the daemon will be started again by the next SSH session after the environment restarts"
                .to_string(),
        );
    }
    Ok((status().await?, warnings))
}

pub(crate) async fn install_direct(cli: &Cli) -> Result<NodeUpdateInstallResult, CliError> {
    ensure_supported_platform()?;
    let manifest = headless::fetch_manifest(None).await?;
    let (target, entry) = manifest.entry_for_current_target().ok_or_else(|| {
        ApplicationError::PlatformUnsupported(format!(
            "release {} has no executable for {}-{}",
            manifest.version,
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    })?;
    let target = target.to_string();
    let current = env!("CARGO_PKG_VERSION");
    if !ochub_core::services::update::is_newer_version(current, &manifest.version) {
        return Ok(NodeUpdateInstallResult {
            updated: false,
            strategy: "direct".to_string(),
            from_version: current.to_string(),
            version: manifest.version.clone(),
            target,
            executable: managed_entrypoint_path(),
            rolled_back: false,
            daemon: status().await?.daemon,
        });
    }
    let payload = headless::download(entry).await?;
    install_verified_payload(cli, &manifest.version, &target, &payload, "direct").await
}

pub(crate) async fn install_direct_remote(
    options: &RemoteNodeOptions,
) -> Result<NodeUpdateInstallResult, CliError> {
    let cli = options.cli()?;
    install_direct(&cli).await
}

async fn receive_update(
    cli: &Cli,
    version: &str,
    target: &str,
    signature: &str,
    sha256: &str,
    size: u64,
    expected_node_id: &str,
) -> Result<NodeUpdateInstallResult, CliError> {
    let policy = ochub_core::remote_policy::load()?;
    if !policy.enabled || !policy.allow_update_install {
        return Err(CliError::Remote {
            code: "PERMISSION_DENIED".to_string(),
            message: "remote policy does not allow relayed node updates".to_string(),
            retryable: false,
            details: json!({ "capability": "node.update.relay" }),
            exit_code: 5,
        });
    }
    let identity = ochub_core::node_identity::load_or_create()?;
    if identity.node_id != expected_node_id {
        return Err(CliError::InvalidInput(format!(
            "update was prepared for node {expected_node_id}, but this node is {}",
            identity.node_id
        )));
    }
    validate_token(version, "version")?;
    validate_token(target, "target")?;
    if size == 0 || size > MAX_PAYLOAD_BYTES {
        return Err(CliError::InvalidInput(
            "relayed update size is outside the allowed range".to_string(),
        ));
    }
    if headless::current_target_key().as_deref() != Some(target) {
        return Err(ApplicationError::PlatformUnsupported(format!(
            "relayed target {target} does not match this node"
        ))
        .into());
    }
    let mut stdin = tokio::io::stdin().take(size.saturating_add(1));
    let mut payload = Vec::with_capacity(size as usize);
    stdin.read_to_end(&mut payload).await?;
    if payload.len() as u64 != size {
        return Err(CliError::InvalidInput(format!(
            "relayed update expected {size} bytes, received {}",
            payload.len()
        )));
    }
    let entry = HeadlessPlatformEntry {
        url: format!(
            "https://github.com/OcHub-team/OcHub/releases/download/v{version}/relayed-{target}"
        ),
        signature: signature.to_string(),
        sha256: sha256.to_string(),
        size,
    };
    headless::verify_payload(&payload, &entry)?;
    install_verified_payload(cli, version, target, &payload, "relay").await
}

pub(crate) async fn install_verified_payload(
    cli: &Cli,
    version: &str,
    target: &str,
    payload: &[u8],
    strategy: &str,
) -> Result<NodeUpdateInstallResult, CliError> {
    ensure_supported_platform()?;
    let _update_lock = acquire_update_lock(&managed_root())?;
    let _ = garbage_collect_managed_root(&managed_root());
    validate_token(version, "version")?;
    if headless::current_target_key().as_deref() != Some(target) {
        return Err(ApplicationError::PlatformUnsupported(format!(
            "update target {target} does not match this node"
        ))
        .into());
    }
    let from_version = env!("CARGO_PKG_VERSION").to_string();
    if !ochub_core::services::update::is_newer_version(&from_version, version) {
        return Err(CliError::InvalidInput(format!(
            "node update {version} is not newer than {from_version}"
        )));
    }

    let was_managed = managed_entrypoint_path().is_file();
    ensure_current_is_managed()?;
    stage_version(&managed_root(), version, payload, true)?;
    let prior_state = read_state(&managed_root())?.ok_or_else(|| {
        CliError::InvalidInput("managed node state was not created before update".to_string())
    })?;
    let mut resume = crate::daemon::suspend_for_update(cli).await?;
    if !was_managed && crate::daemon::user_service_available() {
        // A first one-click update is also a managed installation. This keeps
        // users who skipped `node install` from ending up with an atomic
        // binary but no persistent owner service.
        resume = ServiceResumeMode::InstalledService;
    }
    activate_version(&managed_root(), version)?;
    install_command_link(&managed_root())?;

    let restart = crate::daemon::resume_after_update(cli, resume).await;
    let verified = match restart {
        Ok(_) => verify_runtime_version(version).await,
        Err(error) => Err(error),
    };
    if let Err(update_error) = verified {
        let _ = crate::daemon::suspend_for_update(cli).await;
        let rollback_result = match restore_managed_state(&managed_root(), &prior_state) {
            Ok(()) => match crate::daemon::resume_after_update(cli, resume).await {
                Ok(_) => verify_runtime_version(&prior_state.active_version).await,
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        let _ = garbage_collect_managed_root(&managed_root());
        return match rollback_result {
            Ok(_) => Err(ApplicationError::PartialFailure {
                message: format!(
                    "updated daemon failed health verification and was rolled back: {update_error}"
                ),
                details: json!({ "rolledBack": true, "updateError": update_error.to_string() }),
            }
            .into()),
            Err(rollback_error) => Err(ApplicationError::PartialFailure {
                message: format!(
                    "updated daemon failed ({update_error}) and rollback restart also failed ({rollback_error})"
                ),
                details: json!({
                    "rolledBack": false,
                    "updateError": update_error.to_string(),
                    "rollbackError": rollback_error.to_string()
                }),
            }
            .into()),
        };
    }

    let _ = garbage_collect_managed_root(&managed_root());

    Ok(NodeUpdateInstallResult {
        updated: true,
        strategy: strategy.to_string(),
        from_version,
        version: version.to_string(),
        target: target.to_string(),
        executable: managed_entrypoint_path(),
        rolled_back: false,
        daemon: status().await?.daemon,
    })
}

async fn rollback(cli: &Cli) -> Result<NodeUpdateInstallResult, CliError> {
    let _update_lock = acquire_update_lock(&managed_root())?;
    let state = read_state(&managed_root())?
        .ok_or_else(|| CliError::InvalidInput("managed node state does not exist".to_string()))?;
    let previous = state.previous_version.clone().ok_or_else(|| {
        CliError::InvalidInput("no previous managed node version is retained".to_string())
    })?;
    let current = state.active_version.clone();
    let resume = crate::daemon::suspend_for_update(cli).await?;
    activate_version(&managed_root(), &previous)?;
    let switched = match crate::daemon::resume_after_update(cli, resume).await {
        Ok(_) => verify_runtime_version(&previous).await,
        Err(error) => Err(error),
    };
    if let Err(rollback_error) = switched {
        let _ = crate::daemon::suspend_for_update(cli).await;
        restore_managed_state(&managed_root(), &state)?;
        crate::daemon::resume_after_update(cli, resume).await?;
        verify_runtime_version(&current).await?;
        let _ = garbage_collect_managed_root(&managed_root());
        return Err(ApplicationError::PartialFailure {
            message: format!(
                "rollback target failed health verification and the original version was restored: {rollback_error}"
            ),
            details: json!({
                "rolledBack": false,
                "restoredVersion": current,
                "error": rollback_error.to_string()
            }),
        }
        .into());
    }
    let _ = garbage_collect_managed_root(&managed_root());
    Ok(NodeUpdateInstallResult {
        updated: true,
        strategy: "rollback".to_string(),
        from_version: current,
        version: previous,
        target: headless::current_target_key().unwrap_or_else(|| "unsupported".to_string()),
        executable: managed_entrypoint_path(),
        rolled_back: true,
        daemon: status().await?.daemon,
    })
}

fn ensure_current_is_managed() -> Result<(), CliError> {
    if managed_entrypoint_path().is_file() {
        return Ok(());
    }
    let current = std::env::current_exe()?;
    let payload = fs::read(current)?;
    stage_version(&managed_root(), env!("CARGO_PKG_VERSION"), &payload, true)?;
    activate_version(&managed_root(), env!("CARGO_PKG_VERSION"))?;
    install_command_link(&managed_root())
}

fn stage_version(
    root: &Path,
    version: &str,
    payload: &[u8],
    verify_executable: bool,
) -> Result<PathBuf, CliError> {
    validate_token(version, "version")?;
    let versions = root.join("versions");
    let directory = versions.join(version);
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&versions, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    let destination = directory.join(executable_name());
    let staging = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&staging, payload)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))?;
    }
    if verify_executable && let Err(error) = verify_staged_executable(&staging, version) {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    if let Err(error) = fs::rename(&staging, &destination) {
        let _ = fs::remove_file(&staging);
        return Err(error.into());
    }
    Ok(destination)
}

/// Retain only the active and rollback versions and remove artifacts left by
/// interrupted atomic writes. Every candidate is resolved beneath the managed
/// root and symlinks are removed as links rather than followed.
fn garbage_collect_managed_root(root: &Path) -> Result<(), CliError> {
    if !root.exists() {
        return Ok(());
    }
    let state = read_state(root)?;
    let active = state.as_ref().map(|state| state.active_version.as_str());
    let previous = state
        .as_ref()
        .and_then(|state| state.previous_version.as_deref());

    let versions = root.join("versions");
    if versions.is_dir() {
        for entry in fs::read_dir(&versions)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if validate_token(name, "version").is_err() {
                continue;
            }
            let retained = active == Some(name) || previous == Some(name);
            let metadata = fs::symlink_metadata(entry.path())?;
            if !retained {
                if metadata.file_type().is_symlink() || metadata.is_file() {
                    fs::remove_file(entry.path())?;
                } else if metadata.is_dir() {
                    fs::remove_dir_all(entry.path())?;
                }
                continue;
            }
            if metadata.is_dir() {
                for staged in fs::read_dir(entry.path())? {
                    let staged = staged?;
                    let staged_name = staged.file_name();
                    if staged_name
                        .to_str()
                        .is_some_and(|name| name.starts_with('.') && name.ends_with(".tmp"))
                    {
                        let staged_metadata = fs::symlink_metadata(staged.path())?;
                        if staged_metadata.file_type().is_symlink() || staged_metadata.is_file() {
                            fs::remove_file(staged.path())?;
                        }
                    }
                }
            }
        }
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".current-"))
        {
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || metadata.is_file() {
                fs::remove_file(entry.path())?;
            }
        }
    }
    if root == managed_root()
        && let Some(parent) = command_link().parent()
        && parent.is_dir()
    {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".ochcli-"))
            {
                let metadata = fs::symlink_metadata(entry.path())?;
                if metadata.file_type().is_symlink() || metadata.is_file() {
                    fs::remove_file(entry.path())?;
                }
            }
        }
    }
    Ok(())
}

fn acquire_update_lock(root: &Path) -> Result<File, CliError> {
    fs::create_dir_all(root)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".update.lock"))?;
    lock.lock_exclusive()?;
    Ok(lock)
}

fn verify_staged_executable(path: &Path, version: &str) -> Result<(), CliError> {
    let output = ProcessCommand::new(path)
        .args(["--json", "version"])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(ApplicationError::UpstreamRejected(format!(
            "staged ochcli failed its version smoke test with {}",
            output.status
        ))
        .into());
    }
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    let actual = envelope
        .pointer("/data/version")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CliError::InvalidInput("staged ochcli did not report a structured version".to_string())
        })?;
    if actual != version {
        return Err(CliError::InvalidInput(format!(
            "staged ochcli reports version {actual}, expected {version}"
        )));
    }
    Ok(())
}

fn activate_version(root: &Path, version: &str) -> Result<(), CliError> {
    validate_token(version, "version")?;
    let executable = root.join("versions").join(version).join(executable_name());
    if !executable.is_file() {
        return Err(CliError::InvalidInput(format!(
            "managed node version {version} is not installed"
        )));
    }
    point_current_at(root, version)?;
    let previous = read_state(root)?
        .map(|state| state.active_version)
        .filter(|active| active != version);
    write_state(
        root,
        &ManagedState {
            schema_version: STATE_SCHEMA_VERSION,
            active_version: version.to_string(),
            previous_version: previous,
            updated_at: chrono::Utc::now().to_rfc3339(),
        },
    )
}

fn restore_managed_state(root: &Path, state: &ManagedState) -> Result<(), CliError> {
    validate_token(&state.active_version, "version")?;
    let executable = root
        .join("versions")
        .join(&state.active_version)
        .join(executable_name());
    if !executable.is_file() {
        return Err(CliError::InvalidInput(format!(
            "managed node version {} is not installed",
            state.active_version
        )));
    }
    point_current_at(root, &state.active_version)?;
    let mut restored = state.clone();
    restored.updated_at = chrono::Utc::now().to_rfc3339();
    write_state(root, &restored)
}

#[cfg(unix)]
fn point_current_at(root: &Path, version: &str) -> Result<(), CliError> {
    use std::os::unix::fs::symlink;
    let current = root.join("current");
    let temporary = root.join(format!(".current-{}", uuid::Uuid::new_v4()));
    symlink(Path::new("versions").join(version), &temporary)?;
    fs::rename(&temporary, &current)?;
    Ok(())
}

#[cfg(not(unix))]
fn point_current_at(_root: &Path, _version: &str) -> Result<(), CliError> {
    Err(ApplicationError::PlatformUnsupported(
        "atomic managed node activation is not available on this platform".to_string(),
    )
    .into())
}

fn install_command_link(root: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link = command_link();
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = link.with_file_name(format!(".ochcli-{}", uuid::Uuid::new_v4()));
        symlink(root.join("current").join(executable_name()), &temporary)?;
        fs::rename(temporary, link)?;
    }
    Ok(())
}

async fn verify_runtime_version(expected: &str) -> Result<(), CliError> {
    let response = crate::runtime_client::ping(None, 3).await?;
    let actual = response
        .data
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if actual != expected {
        return Err(ApplicationError::ProtocolIncompatible(format!(
            "updated daemon reports version {actual}, expected {expected}"
        ))
        .into());
    }
    Ok(())
}

fn ensure_supported_platform() -> Result<(), CliError> {
    if managed_updates_supported() {
        Ok(())
    } else {
        Err(ApplicationError::PlatformUnsupported(
            "managed node updates currently require Linux, WSL, or macOS".to_string(),
        )
        .into())
    }
}

pub(crate) fn managed_updates_supported() -> bool {
    cfg!(unix) && headless::current_target_key().is_some()
}

fn validate_token(value: &str, field: &str) -> Result<(), CliError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(CliError::InvalidInput(format!(
            "{field} contains unsupported characters"
        )));
    }
    Ok(())
}

fn require_yes(cli: &Cli, action: &str) -> Result<(), CliError> {
    if cli.yes {
        Ok(())
    } else {
        Err(CliError::InvalidInput(format!(
            "{action} requires --yes after reviewing --dry-run"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_layout_is_user_scoped() {
        let root = managed_root_for(Path::new("/home/alice"));
        assert!(root.starts_with("/home/alice"));
        assert_eq!(root.file_name().and_then(|name| name.to_str()), Some("cli"));
        assert!(
            root.to_string_lossy()
                .to_ascii_lowercase()
                .contains("ochub")
        );
    }

    #[test]
    fn activation_is_atomic_and_retains_one_previous_version() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for version in ["1.0.0", "1.1.0"] {
            let version_dir = root.join("versions").join(version);
            fs::create_dir_all(&version_dir).unwrap();
            fs::write(version_dir.join(executable_name()), b"binary").unwrap();
        }
        activate_version(root, "1.0.0").unwrap();
        activate_version(root, "1.1.0").unwrap();
        garbage_collect_managed_root(root).unwrap();
        let state = read_state(root).unwrap().unwrap();
        assert_eq!(state.active_version, "1.1.0");
        assert_eq!(state.previous_version.as_deref(), Some("1.0.0"));
        assert_eq!(
            fs::read_link(root.join("current")).unwrap(),
            Path::new("versions").join("1.1.0")
        );
        assert!(root.join("versions/1.0.0").is_dir());
    }

    #[test]
    fn failed_update_restores_state_without_retaining_the_bad_version() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for version in ["0.9.0", "1.0.0", "1.1.0"] {
            let version_dir = root.join("versions").join(version);
            fs::create_dir_all(&version_dir).unwrap();
            fs::write(version_dir.join(executable_name()), b"binary").unwrap();
        }
        activate_version(root, "0.9.0").unwrap();
        activate_version(root, "1.0.0").unwrap();
        let prior = read_state(root).unwrap().unwrap();
        activate_version(root, "1.1.0").unwrap();

        restore_managed_state(root, &prior).unwrap();
        garbage_collect_managed_root(root).unwrap();

        let restored = read_state(root).unwrap().unwrap();
        assert_eq!(restored.active_version, "1.0.0");
        assert_eq!(restored.previous_version.as_deref(), Some("0.9.0"));
        assert_eq!(
            fs::read_link(root.join("current")).unwrap(),
            Path::new("versions").join("1.0.0")
        );
        assert!(!root.join("versions/1.1.0").exists());
    }

    #[test]
    fn garbage_collection_removes_old_versions_and_atomic_write_debris() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for version in ["0.8.0", "0.9.0", "1.0.0"] {
            let version_dir = root.join("versions").join(version);
            fs::create_dir_all(&version_dir).unwrap();
            fs::write(version_dir.join(executable_name()), b"binary").unwrap();
        }
        activate_version(root, "0.9.0").unwrap();
        activate_version(root, "1.0.0").unwrap();
        fs::write(root.join("versions/1.0.0/.interrupted.tmp"), b"partial").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("versions/1.0.0", root.join(".current-stale")).unwrap();

        garbage_collect_managed_root(root).unwrap();

        assert!(!root.join("versions/0.8.0").exists());
        assert!(root.join("versions/0.9.0").is_dir());
        assert!(root.join("versions/1.0.0").is_dir());
        assert!(!root.join("versions/1.0.0/.interrupted.tmp").exists());
        #[cfg(unix)]
        assert!(!root.join(".current-stale").exists());
    }

    #[test]
    fn unsafe_versions_never_become_paths() {
        assert!(validate_token("1.2.3", "version").is_ok());
        assert!(validate_token("../../bin", "version").is_err());
        assert!(validate_token("1.0;touch", "version").is_err());
    }
}
