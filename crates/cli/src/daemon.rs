use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use ochub_core::application::{
    Application, ApplicationError, OpenOptions as ApplicationOpenOptions,
};
use ochub_core::runtime::{
    self, IpcError, IpcRequest, IpcResponse, OwnerGuard, OwnerKind, PROTOCOL_VERSION,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, watch};

use crate::command::{Cli, Command, DaemonCommand, GatewayCommand};
use crate::error::CliError;
use crate::output::Output;

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

pub async fn execute(cli: &Cli, command: &DaemonCommand, output: &Output) -> Result<(), CliError> {
    match command {
        DaemonCommand::Run => {
            if cli.dry_run {
                return output.success(
                    &json!({
                        "action": "run-daemon",
                        "socket": cli.socket.clone().unwrap_or_else(runtime::socket_path),
                        "dryRun": true
                    }),
                    &[],
                );
            }
            run_foreground(cli.socket.clone(), Some(output), false).await
        }
        DaemonCommand::Status => match crate::runtime_client::owner_status()? {
            None => output.success(&json!({ "running": false }), &[]),
            Some(owner) => {
                let response =
                    crate::runtime_client::ping(cli.socket.as_deref(), cli.timeout).await?;
                output.mark_owner();
                output.success(
                    &json!({
                        "running": true,
                        "owner": owner,
                        "runtime": response.data
                    }),
                    &response.warnings,
                )
            }
        },
        DaemonCommand::Start => {
            if cli.dry_run {
                return output.success(&json!({ "action": "start-daemon", "dryRun": true }), &[]);
            }
            let owner = start_background(cli).await?;
            output.success(&json!({ "started": true, "owner": owner }), &[])
        }
        DaemonCommand::Stop => {
            if cli.dry_run {
                return output.success(&json!({ "action": "stop-daemon", "dryRun": true }), &[]);
            }
            stop_running(cli).await?;
            output.mark_owner();
            output.success(&json!({ "stopped": true }), &[])
        }
        DaemonCommand::Restart => {
            if cli.dry_run {
                return output.success(&json!({ "action": "restart-daemon", "dryRun": true }), &[]);
            }
            stop_running(cli).await?;
            let owner = start_background(cli).await?;
            output.mark_owner();
            output.success(&json!({ "restarted": true, "owner": owner }), &[])
        }
        DaemonCommand::Install => {
            let plan = service_plan()?;
            if cli.dry_run {
                return output.success(
                    &json!({ "action": "install-daemon-service", "plan": plan, "dryRun": true }),
                    &[],
                );
            }
            install_service()?;
            output.success(&json!({ "installed": true, "service": plan }), &[])
        }
        DaemonCommand::Uninstall => {
            let plan = service_plan()?;
            if cli.dry_run {
                return output.success(
                    &json!({ "action": "uninstall-daemon-service", "plan": plan, "dryRun": true }),
                    &[],
                );
            }
            if !cli.yes {
                return Err(CliError::InvalidInput(
                    "daemon uninstall requires --yes after reviewing --dry-run".to_string(),
                ));
            }
            let _ = stop_running(cli).await;
            uninstall_service()?;
            output.success(
                &json!({
                    "uninstalled": true,
                    "service": plan,
                    "dataPreserved": true
                }),
                &[],
            )
        }
        DaemonCommand::Logs { lines, follow } => {
            let path = daemon_log_path();
            let content = read_log_tail(&path, *lines)?;
            output.success(
                &json!({
                    "path": path,
                    "lines": content,
                    "following": follow
                }),
                &[],
            )?;
            if *follow {
                follow_log(&path).await?;
            }
            Ok(())
        }
    }
}

pub async fn run_foreground(
    socket_override: Option<PathBuf>,
    output: Option<&Output>,
    force_gateway: bool,
) -> Result<(), CliError> {
    ochub_core::app_store::refresh_app_config_dir_override();
    let data_dir = ochub_core::paths::get_app_config_dir();
    let socket = socket_override.unwrap_or_else(runtime::socket_path);
    let endpoint = format!("unix:{}", socket.display());
    let _owner = OwnerGuard::acquire(
        if force_gateway {
            OwnerKind::Foreground
        } else {
            OwnerKind::Daemon
        },
        &data_dir,
        endpoint,
    )?;

    #[cfg(not(unix))]
    {
        let _ = (socket, output, force_gateway);
        return Err(ApplicationError::PlatformUnsupported(
            "Windows named-pipe daemon transport is not available in this build".to_string(),
        )
        .into());
    }

    #[cfg(unix)]
    {
        if let Some(parent) = socket.parent() {
            fs::create_dir_all(parent)?;
        }
        if socket.exists() {
            fs::remove_file(&socket)?;
        }
        let listener = tokio::net::UnixListener::bind(&socket)?;
        set_socket_permissions(&socket)?;
        use std::os::unix::fs::MetadataExt as _;
        let expected_uid = fs::metadata(&socket)?.uid();

        let application = Application::open(ApplicationOpenOptions::default())?;
        ochub_core::services::pricing_catalog::start_background_pricing_sync(
            application.state().db.clone(),
        );
        if force_gateway {
            application.start_gateway().await?;
        } else {
            application.state().gateway.maybe_autostart().await;
        }
        if let Some(output) = output {
            output.success(
                &json!({
                    "running": true,
                    "pid": std::process::id(),
                    "kind": if force_gateway { "foreground" } else { "daemon" },
                    "socket": socket,
                    "dataDir": data_dir
                }),
                &[],
            )?;
        }

        let application = Arc::new(application);
        let serial = Arc::new(Mutex::new(()));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (stream, _) = result?;
                    let peer_uid = stream.peer_cred()?.uid();
                    if peer_uid != expected_uid {
                        tracing::warn!(
                            peer_uid,
                            expected_uid,
                            "rejected runtime connection from another user"
                        );
                        continue;
                    }
                    let application = application.clone();
                    let serial = serial.clone();
                    let shutdown = shutdown_tx.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle_connection(stream, application, serial, shutdown).await {
                            tracing::warn!("runtime connection failed: {error}");
                        }
                    });
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                signal = tokio::signal::ctrl_c() => {
                    signal?;
                    break;
                }
            }
        }
        let _ = application.stop_gateway().await;
        drop(listener);
        let _ = fs::remove_file(&socket);
        Ok(())
    }
}

#[cfg(unix)]
async fn handle_connection(
    stream: tokio::net::UnixStream,
    application: Arc<Application>,
    serial: Arc<Mutex<()>>,
    shutdown: watch::Sender<bool>,
) -> Result<(), CliError> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let response = if line.len() > MAX_FRAME_SIZE {
        error_response(
            String::new(),
            CliError::InvalidInput("runtime request exceeds maximum frame size".to_string()),
        )
    } else {
        match serde_json::from_str::<IpcRequest>(&line) {
            Ok(request) => {
                let response =
                    handle_request(&request, application.as_ref(), serial.as_ref()).await;
                if request.operation == "shutdown" && response.ok {
                    let _ = shutdown.send(true);
                }
                response
            }
            Err(error) => error_response(String::new(), error.into()),
        }
    };
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    write.shutdown().await?;
    Ok(())
}

async fn handle_request(
    request: &IpcRequest,
    application: &Application,
    serial: &Mutex<()>,
) -> IpcResponse {
    if request.protocol_version != PROTOCOL_VERSION {
        return error_response(
            request.request_id.clone(),
            ApplicationError::ProtocolIncompatible(format!(
                "client protocol {}, owner protocol {}",
                request.protocol_version, PROTOCOL_VERSION
            ))
            .into(),
        );
    }
    match request.operation.as_str() {
        "ping" => IpcResponse {
            frame_type: "response".to_string(),
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            ok: true,
            data: json!({
                "pid": std::process::id(),
                "version": env!("CARGO_PKG_VERSION"),
                "dataDir": ochub_core::paths::get_app_config_dir(),
                "gateway": application.state().gateway.status().await
            }),
            warnings: Vec::new(),
            error: None,
        },
        "shutdown" => IpcResponse {
            frame_type: "response".to_string(),
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id.clone(),
            ok: true,
            data: json!({ "accepted": true }),
            warnings: Vec::new(),
            error: None,
        },
        "execute" => {
            let argv = request
                .params
                .get("argv")
                .and_then(Value::as_array)
                .and_then(|values| {
                    values
                        .iter()
                        .map(|value| value.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()
                });
            let Some(argv) = argv else {
                return error_response(
                    request.request_id.clone(),
                    CliError::InvalidInput("runtime execute requires string argv".to_string()),
                );
            };
            let mut parse_argv = vec!["ochcli".to_string()];
            parse_argv.extend(argv);
            let cli = match Cli::try_parse_from(parse_argv) {
                Ok(cli) => cli,
                Err(error) => {
                    return error_response(
                        request.request_id.clone(),
                        CliError::InvalidInput(error.to_string()),
                    );
                }
            };
            if !remote_command_allowed(&cli.command) {
                return error_response(
                    request.request_id.clone(),
                    CliError::InvalidInput(
                        "this command must execute in the requesting process".to_string(),
                    ),
                );
            }
            let _serial = serial.lock().await;
            let _mutation = match ochub_core::runtime::MutationGuard::acquire() {
                Ok(guard) => guard,
                Err(error) => return error_response(request.request_id.clone(), error.into()),
            };
            let (capture, handle) = Output::capture();
            match crate::run::execute_with_application(application, &cli, &capture).await {
                Ok(()) => {
                    let captured = handle.take().unwrap_or(crate::output::CapturedOutput {
                        data: Value::Null,
                        warnings: Vec::new(),
                    });
                    IpcResponse {
                        frame_type: "response".to_string(),
                        protocol_version: PROTOCOL_VERSION,
                        request_id: request.request_id.clone(),
                        ok: true,
                        data: captured.data,
                        warnings: captured.warnings,
                        error: None,
                    }
                }
                Err(error) => error_response(request.request_id.clone(), error),
            }
        }
        operation => error_response(
            request.request_id.clone(),
            CliError::InvalidInput(format!("unknown runtime operation: {operation}")),
        ),
    }
}

fn remote_command_allowed(command: &Command) -> bool {
    !matches!(
        command,
        Command::Version
            | Command::Paths
            | Command::Completion(_)
            | Command::Man(_)
            | Command::Remote(_)
            | Command::Node(_)
            | Command::Daemon(_)
            | Command::Gateway(crate::command::GatewayArgs {
                command: GatewayCommand::Serve
            })
    )
}

fn error_response(request_id: String, error: CliError) -> IpcResponse {
    IpcResponse {
        frame_type: "response".to_string(),
        protocol_version: PROTOCOL_VERSION,
        request_id,
        ok: false,
        data: Value::Null,
        warnings: Vec::new(),
        error: Some(IpcError {
            code: error.code().to_string(),
            message: error.to_string(),
            retryable: error.retryable(),
            details: error.details(),
            exit_code: error.exit_code_u8(),
        }),
    }
}

pub async fn start_background(cli: &Cli) -> Result<ochub_core::runtime::OwnerRecord, CliError> {
    if let Some(owner) = runtime::active_owner()? {
        return Ok(owner);
    }
    let invocation = daemon_invocation()?;
    let log_path = daemon_log_path();
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let stderr = stdout.try_clone()?;
    let mut process = ProcessCommand::new(&invocation.program);
    process
        .args(&invocation.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(data_dir) = &cli.data_dir {
        process.env("OCHUB_DATA_DIR", data_dir);
    }
    if let Some(socket) = &cli.socket {
        process.env("OCHUB_SOCKET", socket);
    }
    process.spawn()?;

    for _ in 0..50 {
        if let Some(owner) = runtime::active_owner()?
            && crate::runtime_client::ping(cli.socket.as_deref(), 1)
                .await
                .is_ok()
        {
            return Ok(owner);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(ApplicationError::RuntimeUnavailable(format!(
        "daemon did not become ready; inspect {}",
        log_path.display()
    ))
    .into())
}

pub(crate) async fn stop_running(cli: &Cli) -> Result<(), CliError> {
    if runtime::active_owner()?.is_none() {
        return Ok(());
    }
    crate::runtime_client::shutdown(cli.socket.as_deref(), cli.timeout).await?;
    for _ in 0..50 {
        if runtime::active_owner()?.is_none() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(ApplicationError::RuntimeUnavailable(
        "runtime accepted shutdown but did not release the owner lock".to_string(),
    )
    .into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceResumeMode {
    InstalledService,
    Background,
}

pub(crate) fn user_service_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        ProcessCommand::new("systemctl")
            .args(["--user", "show-environment"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

pub(crate) async fn suspend_for_update(cli: &Cli) -> Result<ServiceResumeMode, CliError> {
    if let Some(owner) = runtime::active_owner()?
        && owner.kind == OwnerKind::Gui
    {
        return Err(ApplicationError::OwnerConflict(format!(
            "the desktop GUI owns this data directory (pid {}); quit it before updating the headless node",
            owner.pid
        ))
        .into());
    }

    let installed = service_definition_path().is_ok_and(|path| path.exists());
    if installed && user_service_available() {
        stop_installed_service()?;
        wait_for_owner_exit().await?;
        return Ok(ServiceResumeMode::InstalledService);
    }
    stop_running(cli).await?;
    Ok(ServiceResumeMode::Background)
}

pub(crate) async fn resume_after_update(
    cli: &Cli,
    mode: ServiceResumeMode,
) -> Result<ochub_core::runtime::OwnerRecord, CliError> {
    match mode {
        ServiceResumeMode::InstalledService => {
            // Rewrite the definition as well as starting it: an installation
            // migrated from the legacy two-binary layout may still point at
            // the old, version-specific ochubd path.
            install_service()?;
            wait_for_owner_ready(cli).await
        }
        ServiceResumeMode::Background => start_background(cli).await,
    }
}

async fn wait_for_owner_exit() -> Result<(), CliError> {
    for _ in 0..100 {
        if runtime::active_owner()?.is_none() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(ApplicationError::RuntimeUnavailable(
        "runtime did not stop before the node update".to_string(),
    )
    .into())
}

async fn wait_for_owner_ready(cli: &Cli) -> Result<ochub_core::runtime::OwnerRecord, CliError> {
    for _ in 0..100 {
        if let Some(owner) = runtime::active_owner()?
            && crate::runtime_client::ping(cli.socket.as_deref(), 1)
                .await
                .is_ok()
        {
            return Ok(owner);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(
        ApplicationError::RuntimeUnavailable("updated daemon did not become ready".to_string())
            .into(),
    )
}

#[cfg(target_os = "macos")]
fn stop_installed_service() -> Result<(), CliError> {
    let path = service_definition_path()?;
    let uid = command_stdout("id", &["-u"])?;
    let target = format!("gui/{}", uid.trim());
    let status = ProcessCommand::new("launchctl")
        .args(["bootout", &target])
        .arg(&path)
        .status()?;
    if status.success() || runtime::active_owner()?.is_none() {
        Ok(())
    } else {
        Err(ApplicationError::UpstreamRejected(
            "launchctl could not suspend the OcHub daemon".to_string(),
        )
        .into())
    }
}

#[cfg(target_os = "linux")]
fn stop_installed_service() -> Result<(), CliError> {
    checked_command("systemctl", &["--user", "stop", "ochubd.service"])
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn stop_installed_service() -> Result<(), CliError> {
    Err(ApplicationError::PlatformUnsupported(
        "daemon service suspension is unsupported on this platform".to_string(),
    )
    .into())
}

#[derive(Debug, Clone)]
struct DaemonInvocation {
    program: PathBuf,
    args: Vec<String>,
}

fn daemon_invocation() -> Result<DaemonInvocation, CliError> {
    if let Some(managed) = crate::node::managed_entrypoint() {
        return Ok(DaemonInvocation {
            program: managed,
            args: vec!["daemon".to_string(), "run".to_string()],
        });
    }
    Ok(DaemonInvocation {
        program: std::env::current_exe()?,
        args: vec!["daemon".to_string(), "run".to_string()],
    })
}

fn daemon_log_path() -> PathBuf {
    runtime::runtime_dir().join("daemon.log")
}

fn read_log_tail(path: &Path, lines: usize) -> Result<Vec<String>, CliError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)?;
    let mut lines = content
        .lines()
        .rev()
        .take(lines.min(100_000))
        .map(str::to_string)
        .collect::<Vec<_>>();
    lines.reverse();
    Ok(lines)
}

async fn follow_log(path: &Path) -> Result<(), CliError> {
    let mut offset = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                let Ok(mut file) = OpenOptions::new().read(true).open(path) else {
                    continue;
                };
                let len = file.metadata()?.len();
                if len < offset {
                    offset = 0;
                }
                file.seek(SeekFrom::Start(offset))?;
                let mut content = String::new();
                file.read_to_string(&mut content)?;
                offset = len;
                if !content.is_empty() {
                    print!("{content}");
                }
            }
        }
    }
}

#[cfg(unix)]
fn set_socket_permissions(path: &Path) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub(crate) fn service_plan() -> Result<Value, CliError> {
    let invocation = daemon_invocation()?;
    Ok(json!({
        "platform": std::env::consts::OS,
        "definition": service_definition_path()?,
        "program": invocation.program,
        "arguments": invocation.args,
        "log": daemon_log_path(),
        "scope": "current-user"
    }))
}

#[cfg(target_os = "macos")]
pub(crate) fn service_definition_path() -> Result<PathBuf, CliError> {
    Ok(ochub_core::paths::get_home_dir().join("Library/LaunchAgents/io.ochub.daemon.plist"))
}

#[cfg(target_os = "linux")]
pub(crate) fn service_definition_path() -> Result<PathBuf, CliError> {
    Ok(ochub_core::paths::get_home_dir().join(".config/systemd/user/ochubd.service"))
}

#[cfg(windows)]
pub(crate) fn service_definition_path() -> Result<PathBuf, CliError> {
    Ok(runtime::runtime_dir().join("ochubd-task.json"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn service_definition_path() -> Result<PathBuf, CliError> {
    Err(ApplicationError::PlatformUnsupported(
        "daemon service installation is unsupported on this platform".to_string(),
    )
    .into())
}

#[cfg(target_os = "macos")]
pub(crate) fn install_service() -> Result<(), CliError> {
    let path = service_definition_path()?;
    let invocation = daemon_invocation()?;
    let log = daemon_log_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut arguments = vec![invocation.program.to_string_lossy().into_owned()];
    arguments.extend(invocation.args);
    let arguments = arguments
        .iter()
        .map(|arg| format!("<string>{}</string>", xml_escape(arg)))
        .collect::<Vec<_>>()
        .join("");
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\
         <key>Label</key><string>io.ochub.daemon</string>\
         <key>ProgramArguments</key><array>{arguments}</array>\
         <key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\
         <key>StandardOutPath</key><string>{log}</string>\
         <key>StandardErrorPath</key><string>{log}</string>\
         </dict></plist>\n",
        log = xml_escape(&log.to_string_lossy())
    );
    fs::write(&path, plist)?;
    let uid = command_stdout("id", &["-u"])?;
    let target = format!("gui/{}", uid.trim());
    let result = ProcessCommand::new("launchctl")
        .args(["bootstrap", &target])
        .arg(&path)
        .status()?;
    if !result.success() {
        return Err(
            ApplicationError::UpstreamRejected("launchctl bootstrap failed".to_string()).into(),
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn install_service() -> Result<(), CliError> {
    let path = service_definition_path()?;
    let invocation = daemon_invocation()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut command = shell_escape(&invocation.program.to_string_lossy());
    for arg in invocation.args {
        command.push(' ');
        command.push_str(&shell_escape(&arg));
    }
    let unit = format!(
        "[Unit]\nDescription=OcHub daemon\nAfter=network-online.target\n\n\
         [Service]\nExecStart={command}\nRestart=on-failure\nRestartSec=2\n\n\
         [Install]\nWantedBy=default.target\n"
    );
    fs::write(&path, unit)?;
    checked_command("systemctl", &["--user", "daemon-reload"])?;
    checked_command(
        "systemctl",
        &["--user", "enable", "--now", "ochubd.service"],
    )
}

#[cfg(windows)]
pub(crate) fn install_service() -> Result<(), CliError> {
    Err(ApplicationError::PlatformUnsupported(
        "Windows user-level daemon installation is not available in this build".to_string(),
    )
    .into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn install_service() -> Result<(), CliError> {
    Err(ApplicationError::PlatformUnsupported(
        "daemon service installation is unsupported on this platform".to_string(),
    )
    .into())
}

#[cfg(target_os = "macos")]
fn uninstall_service() -> Result<(), CliError> {
    let path = service_definition_path()?;
    let uid = command_stdout("id", &["-u"])?;
    let target = format!("gui/{}", uid.trim());
    let _ = ProcessCommand::new("launchctl")
        .args(["bootout", &target])
        .arg(&path)
        .status();
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_service() -> Result<(), CliError> {
    let path = service_definition_path()?;
    let _ = ProcessCommand::new("systemctl")
        .args(["--user", "disable", "--now", "ochubd.service"])
        .status();
    if path.exists() {
        fs::remove_file(path)?;
    }
    checked_command("systemctl", &["--user", "daemon-reload"])
}

#[cfg(windows)]
fn uninstall_service() -> Result<(), CliError> {
    Err(ApplicationError::PlatformUnsupported(
        "Windows user-level daemon uninstall is not available in this build".to_string(),
    )
    .into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn uninstall_service() -> Result<(), CliError> {
    Err(ApplicationError::PlatformUnsupported(
        "daemon service uninstall is unsupported on this platform".to_string(),
    )
    .into())
}

#[cfg(target_os = "linux")]
fn checked_command(program: &str, args: &[&str]) -> Result<(), CliError> {
    let status = ProcessCommand::new(program).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(ApplicationError::UpstreamRejected(format!("{program} exited with {status}")).into())
    }
}

#[cfg(target_os = "macos")]
fn command_stdout(program: &str, args: &[&str]) -> Result<String, CliError> {
    let output = ProcessCommand::new(program).args(args).output()?;
    if !output.status.success() {
        return Err(ApplicationError::UpstreamRejected(format!(
            "{program} exited with {}",
            output.status
        ))
        .into());
    }
    String::from_utf8(output.stdout).map_err(|error| {
        CliError::InvalidInput(format!("{program} returned invalid UTF-8: {error}"))
    })
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "linux")]
fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
