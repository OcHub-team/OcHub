//! Controlled Codex desktop launcher and native picker unlock.
//!
//! The Codex renderer applies account gates after it reads the model catalog:
//! Fast requires ChatGPT auth, while Ultra requires an account setting. OcHub
//! launches an isolated debugging instance and uses CDP Fetch interception to
//! remove only those two renderer gates as `app-initial-*.js` loads. The model
//! catalog and Codex's own React components, setters, and request path remain
//! responsible for the actual options and selected values. Nothing is changed
//! in the signed application bundle on disk.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use base64::Engine;
use futures::{SinkExt, StreamExt};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::error::AppError;
use crate::paths::get_home_dir;

const CODEX_CDP_READY_ATTEMPTS: usize = 30;
const CODEX_CDP_READY_INTERVAL: Duration = Duration::from_millis(400);
const CODEX_PICKER_INJECTION_TIMEOUT: Duration = Duration::from_secs(30);
static ACTIVE_CODEX_DEBUG_PORT: AtomicU16 = AtomicU16::new(0);
static CODEX_PICKER_MONITOR: Lazy<Mutex<Option<JoinHandle<()>>>> = Lazy::new(|| Mutex::new(None));

static FAST_UI_GATE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?P<lhs>[A-Za-z_$][A-Za-z0-9_$]*)=[A-Za-z_$][A-Za-z0-9_$]*&&![A-Za-z_$][A-Za-z0-9_$]*&&[A-Za-z_$][A-Za-z0-9_$]*!=null&&[A-Za-z_$][A-Za-z0-9_$]*\?\.requirements\?\.featureRequirements\?\.fast_mode!==!1",
    )
    .expect("Fast UI gate regex must compile")
});

static FAST_AUTH_GATE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"if\((?P<auth>[A-Za-z_$][A-Za-z0-9_$]*)!==`chatgpt`\)return!1;")
        .expect("Fast auth gate regex must compile")
});

static FAST_REQUEST_GATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"fast_mode===!1").expect("Fast request gate regex must compile"));

static ULTRA_ACCOUNT_GATE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?P<lhs>[A-Za-z_$][A-Za-z0-9_$]*)=[A-Za-z_$][A-Za-z0-9_$]*\?\.ultraEffortEnabled===!0",
    )
    .expect("Ultra account gate regex must compile")
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAppLaunch {
    pub app_path: PathBuf,
    pub debug_port: u16,
    pub reused: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct CdpTarget {
    #[serde(rename = "type")]
    target_type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default, rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct CdpVersion {
    #[serde(default, rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PickerPatchStats {
    fast_ui_gates: usize,
    fast_auth_gates: usize,
    fast_request_gates: usize,
    ultra_gates: usize,
}

type CdpSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct CdpConnection {
    socket: CdpSocket,
    next_id: u64,
    queued_messages: VecDeque<Value>,
}

fn codex_app_candidates(home: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("OCHUB_CODEX_APP_PATH") {
        candidates.push(PathBuf::from(path));
    }

    for root in [PathBuf::from("/Applications"), home.join("Applications")] {
        for name in [
            "Codex.app",
            "OpenAI Codex.app",
            "OpenAI.Codex.app",
            // Current macOS releases use ChatGPT.app while retaining Codex as
            // the signing base name and renderer product name.
            "ChatGPT.app",
        ] {
            candidates.push(root.join(name));
        }
    }
    candidates
}

fn is_supported_codex_app(path: &Path) -> bool {
    if !path.is_dir() || path.extension().and_then(|value| value.to_str()) != Some("app") {
        return false;
    }
    let macos = path.join("Contents/MacOS");
    macos.join("Codex").is_file() || macos.join("ChatGPT").is_file()
}

pub fn find_codex_app() -> Result<PathBuf, AppError> {
    codex_app_candidates(&get_home_dir())
        .into_iter()
        .find(|candidate| is_supported_codex_app(candidate))
        .ok_or_else(|| {
            AppError::localized_ja(
                "codex.app.not_found",
                "未找到 Codex App；请将 Codex.app 或 ChatGPT.app 安装到 Applications，或设置 OCHUB_CODEX_APP_PATH。",
                "Codex App was not found. Install Codex.app or ChatGPT.app in Applications, or set OCHUB_CODEX_APP_PATH.",
                "Codex App が見つかりません。Codex.app または ChatGPT.app を Applications にインストールするか、OCHUB_CODEX_APP_PATH を設定してください。",
            )
        })
}

fn reserve_loopback_port() -> Result<u16, AppError> {
    let listener =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(|error| {
            AppError::IoContext {
                context: "failed to reserve a Codex debugging port".to_string(),
                source: error,
            }
        })?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| AppError::IoContext {
            context: "failed to inspect the Codex debugging port".to_string(),
            source: error,
        })
}

#[cfg(target_os = "macos")]
fn macos_open_command(app_path: &Path, debug_port: u16, new_instance: bool) -> Vec<String> {
    let mut command = vec!["open".to_string()];
    if new_instance {
        command.push("-n".to_string());
    }
    command.extend(["-a".to_string(), app_path.to_string_lossy().into_owned()]);
    if new_instance {
        command.extend([
            "--args".to_string(),
            "--remote-debugging-address=127.0.0.1".to_string(),
            format!("--remote-debugging-port={debug_port}"),
            format!("--remote-allow-origins=http://127.0.0.1:{debug_port}"),
        ]);
    }
    command
}

fn is_main_codex_target(target: &CdpTarget, debug_port: u16) -> bool {
    if target.target_type != "page"
        || !target.url.starts_with("app://-/")
        || target.url.contains("avatar-overlay")
    {
        return false;
    }
    let Some(websocket_url) = target.web_socket_debugger_url.as_deref() else {
        return false;
    };
    is_loopback_debugger_websocket(websocket_url, debug_port)
}

fn is_loopback_debugger_websocket(websocket_url: &str, debug_port: u16) -> bool {
    let Ok(url) = url::Url::parse(websocket_url) else {
        return false;
    };
    matches!(url.scheme(), "ws" | "wss")
        && url.port_or_known_default() == Some(debug_port)
        && url
            .host_str()
            .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]"))
}

async fn browser_debugger_websocket(client: &reqwest::Client, debug_port: u16) -> Option<String> {
    let url = format!("http://127.0.0.1:{debug_port}/json/version");
    let response = client.get(url).send().await.ok()?.error_for_status().ok()?;
    let version = response.json::<CdpVersion>().await.ok()?;
    version
        .web_socket_debugger_url
        .filter(|url| is_loopback_debugger_websocket(url, debug_port))
}

async fn wait_for_browser_debugger_websocket(
    client: &reqwest::Client,
    debug_port: u16,
) -> Result<String, AppError> {
    for _ in 0..CODEX_CDP_READY_ATTEMPTS * 4 {
        if let Some(websocket_url) = browser_debugger_websocket(client, debug_port).await {
            return Ok(websocket_url);
        }
        tokio::time::sleep(CODEX_CDP_READY_INTERVAL / 4).await;
    }
    Err(AppError::localized_ja(
        "codex.app.launch_timeout",
        "Codex App 已启动，但没有开放调试连接。请退出所有 Codex/ChatGPT App 窗口后重试。",
        "Codex App launched, but it did not expose the debugging connection. Quit all Codex/ChatGPT App windows and try again.",
        "Codex App は起動しましたが、デバッグ接続を確認できませんでした。Codex/ChatGPT App をすべて終了してから再試行してください。",
    ))
}

async fn main_codex_target(client: &reqwest::Client, debug_port: u16) -> Option<CdpTarget> {
    let url = format!("http://127.0.0.1:{debug_port}/json/list");
    let Ok(response) = client.get(url).send().await else {
        return None;
    };
    let Ok(response) = response.error_for_status() else {
        return None;
    };
    let Ok(targets) = response.json::<Vec<CdpTarget>>().await else {
        return None;
    };
    targets
        .into_iter()
        .find(|target| is_main_codex_target(target, debug_port))
}

async fn main_codex_target_ready(client: &reqwest::Client, debug_port: u16) -> bool {
    main_codex_target(client, debug_port).await.is_some()
}

async fn wait_for_main_codex_target(
    client: &reqwest::Client,
    debug_port: u16,
) -> Result<(), AppError> {
    for _ in 0..CODEX_CDP_READY_ATTEMPTS {
        if main_codex_target_ready(client, debug_port).await {
            return Ok(());
        }
        tokio::time::sleep(CODEX_CDP_READY_INTERVAL).await;
    }
    Err(AppError::localized_ja(
        "codex.app.launch_timeout",
        "Codex App 已启动，但主窗口没有开放调试连接。请退出所有 Codex/ChatGPT App 窗口后重试。",
        "Codex App launched, but its main window did not expose the debugging connection. Quit all Codex/ChatGPT App windows and try again.",
        "Codex App は起動しましたが、メインウィンドウのデバッグ接続を確認できませんでした。Codex/ChatGPT App をすべて終了してから再試行してください。",
    ))
}

fn patch_codex_picker_source(source: &str) -> Result<(String, PickerPatchStats), String> {
    let fast_ui_gates = FAST_UI_GATE.find_iter(source).count();
    let fast_auth_gates = FAST_AUTH_GATE.find_iter(source).count();
    let fast_request_gates = FAST_REQUEST_GATE.find_iter(source).count();
    let ultra_gates = ULTRA_ACCOUNT_GATE.find_iter(source).count();
    if fast_ui_gates != 1 || fast_auth_gates != 1 || fast_request_gates != 1 || ultra_gates == 0 {
        return Err(format!(
            "unsupported Codex renderer gates (Fast UI: {fast_ui_gates}, Fast auth: {fast_auth_gates}, Fast request: {fast_request_gates}, Ultra: {ultra_gates})"
        ));
    }

    let source = FAST_UI_GATE.replace_all(source, "${lhs}=!0");
    // API-key/custom-provider hosts should use their catalog capability rather
    // than an unrelated ChatGPT account type to decide whether Fast exists.
    let source = FAST_AUTH_GATE.replace_all(&source, "if(${auth}!==`chatgpt`)return!0;");
    // Preserve the official service-tier setter and request shape, but do not
    // let ChatGPT account requirements erase that value for a custom provider.
    let source = FAST_REQUEST_GATE.replace_all(&source, "fast_mode===!2");
    let source = ULTRA_ACCOUNT_GATE.replace_all(&source, "${lhs}=!0");
    Ok((
        source.into_owned(),
        PickerPatchStats {
            fast_ui_gates,
            fast_auth_gates,
            fast_request_gates,
            ultra_gates,
        },
    ))
}

impl CdpConnection {
    fn new(socket: CdpSocket) -> Self {
        Self {
            socket,
            next_id: 1,
            queued_messages: VecDeque::new(),
        }
    }

    async fn read_message(&mut self, context: &str) -> Result<Option<Value>, String> {
        while let Some(message) = self.socket.next().await {
            let message = message.map_err(|error| format!("CDP {context} failed: {error}"))?;
            let Message::Text(text) = message else {
                continue;
            };
            let payload: Value = serde_json::from_str(&text)
                .map_err(|error| format!("invalid CDP message during {context}: {error}"))?;
            return Ok(Some(payload));
        }
        Ok(None)
    }

    async fn send(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut command = json!({ "id": id, "method": method, "params": params });
        if let Some(session_id) = session_id {
            command["sessionId"] = json!(session_id);
        }
        self.socket
            .send(Message::Text(command.to_string().into()))
            .await
            .map_err(|error| format!("failed to send CDP {method}: {error}"))?;
        Ok(id)
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        let id = self.send(method, params, session_id).await?;
        while let Some(payload) = self.read_message(method).await? {
            if payload.get("id").and_then(Value::as_u64) != Some(id) {
                self.queued_messages.push_back(payload);
                continue;
            }
            if let Some(error) = payload.get("error") {
                return Err(format!("CDP {method} returned {error}"));
            }
            return Ok(payload.get("result").cloned().unwrap_or(Value::Null));
        }
        Err(format!("CDP connection closed while waiting for {method}"))
    }

    async fn next_message(&mut self) -> Result<Option<Value>, String> {
        if let Some(payload) = self.queued_messages.pop_front() {
            return Ok(Some(payload));
        }
        self.read_message("monitor").await
    }
}

async fn patch_paused_picker_script(
    connection: &mut CdpConnection,
    params: &Value,
    session_id: &str,
) -> Result<PickerPatchStats, String> {
    let request_id = params
        .get("requestId")
        .and_then(Value::as_str)
        .ok_or_else(|| "CDP Fetch.requestPaused is missing requestId".to_string())?;
    let response_code = params
        .get("responseStatusCode")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            "Codex renderer interception paused before the response stage".to_string()
        })?;
    let body = connection
        .request(
            "Fetch.getResponseBody",
            json!({ "requestId": request_id }),
            Some(session_id),
        )
        .await?;
    let body_text = body
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| "CDP Fetch.getResponseBody returned no body".to_string())?;
    let source_bytes = if body
        .get("base64Encoded")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        base64::engine::general_purpose::STANDARD
            .decode(body_text)
            .map_err(|error| format!("invalid base64 Codex renderer body: {error}"))?
    } else {
        body_text.as_bytes().to_vec()
    };
    let source = String::from_utf8(source_bytes)
        .map_err(|error| format!("Codex renderer source is not UTF-8: {error}"))?;
    let (patched, stats) = patch_codex_picker_source(&source)?;

    let response_headers = params
        .get("responseHeaders")
        .and_then(Value::as_array)
        .map(|headers| {
            headers
                .iter()
                .filter(|header| {
                    !header
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| {
                            name.eq_ignore_ascii_case("content-length")
                                || name.eq_ignore_ascii_case("content-encoding")
                        })
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let encoded = base64::engine::general_purpose::STANDARD.encode(patched.as_bytes());
    let mut fulfill_params = json!({
        "requestId": request_id,
        "responseCode": response_code,
        "responseHeaders": response_headers,
        "body": encoded,
    });
    if let Some(response_phrase) = params.get("responseStatusText").and_then(Value::as_str) {
        fulfill_params["responsePhrase"] = json!(response_phrase);
    }
    connection
        .request("Fetch.fulfillRequest", fulfill_params, Some(session_id))
        .await?;
    Ok(stats)
}

async fn continue_paused_request(connection: &mut CdpConnection, params: &Value, session_id: &str) {
    let Some(request_id) = params.get("requestId").and_then(Value::as_str) else {
        return;
    };
    // Fire-and-forget keeps the event loop available for app-initial.js. Its
    // pause can arrive before Chromium acknowledges the preceding CSS resume.
    let _ = connection
        .send(
            "Fetch.continueRequest",
            json!({ "requestId": request_id }),
            Some(session_id),
        )
        .await;
}

async fn run_picker_monitor(
    websocket_url: String,
    initial_result: oneshot::Sender<Result<PickerPatchStats, String>>,
) {
    let mut injection_completed = false;
    let result: Result<(), String> = async {
        // Fetch.getResponseBody returns the minified renderer (currently about
        // 15 MiB) inside a JSON frame, so Chromium can exceed Tungstenite's
        // default 16 MiB single-frame cap.
        let websocket_config = WebSocketConfig::default()
            .max_message_size(Some(64 << 20))
            .max_frame_size(Some(64 << 20));
        let (socket, _) = tokio_tungstenite::connect_async_with_config(
            &websocket_url,
            Some(websocket_config),
            false,
        )
        .await
        .map_err(|error| format!("failed to connect to the Codex browser debugger: {error}"))?;
        let mut connection = CdpConnection::new(socket);
        // The browser endpoint appears before the first renderer. Auto-attach
        // pauses each renderer before any application JavaScript runs, which
        // lets us intercept app-initial.js without reloading app://-/index.html.
        // Codex treats an external reload of that custom-scheme page as a
        // navigation failure, so first-document interception is required.
        connection
            .request(
                "Target.setAutoAttach",
                json!({
                    "autoAttach": true,
                    "waitForDebuggerOnStart": true,
                    "flatten": true
                }),
                None,
            )
            .await?;

        let mut initial_result = Some(initial_result);
        while let Some(payload) = connection.next_message().await? {
            match payload.get("method").and_then(Value::as_str) {
                Some("Target.attachedToTarget") => {
                    let params = payload.get("params").cloned().unwrap_or(Value::Null);
                    let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
                        continue;
                    };
                    let target_type = params
                        .get("targetInfo")
                        .and_then(|target| target.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if target_type == "page" {
                        connection
                            .request(
                                "Fetch.enable",
                                json!({
                                    "patterns": [{
                                        // The custom-scheme glob does not
                                        // reliably match a suffix after `*`.
                                        // Pass CSS through and patch only JS.
                                        "urlPattern": "app://-/assets/app-initial-*",
                                        "requestStage": "Response"
                                    }]
                                }),
                                Some(session_id),
                            )
                            .await?;
                    }
                    connection
                        .request(
                            "Runtime.runIfWaitingForDebugger",
                            json!({}),
                            Some(session_id),
                        )
                        .await?;
                }
                Some("Fetch.requestPaused") => {
                    let params = payload.get("params").cloned().unwrap_or(Value::Null);
                    let Some(session_id) = payload.get("sessionId").and_then(Value::as_str) else {
                        continue;
                    };
                    let is_javascript = params
                        .get("request")
                        .and_then(|request| request.get("url"))
                        .and_then(Value::as_str)
                        .is_some_and(|url| {
                            url.split('?')
                                .next()
                                .is_some_and(|url| url.ends_with(".js"))
                        });
                    if !is_javascript {
                        continue_paused_request(&mut connection, &params, session_id).await;
                        continue;
                    }
                    match patch_paused_picker_script(&mut connection, &params, session_id).await {
                        Ok(stats) => {
                            injection_completed = true;
                            if let Some(sender) = initial_result.take() {
                                let _ = sender.send(Ok(stats));
                            }
                        }
                        Err(error) => {
                            continue_paused_request(&mut connection, &params, session_id).await;
                            if let Some(sender) = initial_result.take() {
                                let _ = sender.send(Err(error.clone()));
                            }
                            return Err(error);
                        }
                    }
                }
                _ => {}
            }
        }
        Err("Codex CDP monitor disconnected".to_string())
    }
    .await;

    if let Err(error) = result {
        if injection_completed {
            log::debug!("Codex native picker monitor stopped after injection: {error}");
        } else {
            log::warn!("Codex native picker monitor stopped before injection: {error}");
        }
    }
}

async fn install_picker_unlock(
    client: &reqwest::Client,
    debug_port: u16,
) -> Result<PickerPatchStats, AppError> {
    let websocket_url = wait_for_browser_debugger_websocket(client, debug_port).await?;

    let (sender, receiver) = oneshot::channel();
    let handle = tokio::spawn(run_picker_monitor(websocket_url, sender));
    {
        let mut monitor = CODEX_PICKER_MONITOR
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = monitor.replace(handle) {
            previous.abort();
        }
    }

    match tokio::time::timeout(CODEX_PICKER_INJECTION_TIMEOUT, receiver).await {
        Ok(Ok(Ok(stats))) => Ok(stats),
        Ok(Ok(Err(error))) => Err(picker_injection_error(error)),
        Ok(Err(_)) => Err(picker_injection_error(
            "Codex CDP monitor stopped before injection completed".to_string(),
        )),
        Err(_) => Err(picker_injection_error(
            "timed out waiting for the Codex renderer script".to_string(),
        )),
    }
}

fn picker_monitor_running() -> bool {
    CODEX_PICKER_MONITOR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .is_some_and(|handle| !handle.is_finished())
}

fn picker_injection_error(detail: String) -> AppError {
    AppError::localized_ja(
        "codex.app.picker_injection_failed",
        format!("Codex App 已启动，但原生 Fast/Ultra 选择器注入失败：{detail}"),
        format!(
            "Codex App launched, but its native Fast/Ultra picker could not be unlocked: {detail}"
        ),
        format!(
            "Codex App は起動しましたが、ネイティブの Fast/Ultra ピッカーを解除できませんでした: {detail}"
        ),
    )
}

/// Launch (or reactivate) a Codex instance whose native model picker consumes
/// the OcHub-generated model catalog with the account-only UI gates removed.
pub async fn launch_codex_app() -> Result<CodexAppLaunch, AppError> {
    let app_path = find_codex_app()?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|error| AppError::Message(format!("failed to build Codex CDP client: {error}")))?;

    let active_port = ACTIVE_CODEX_DEBUG_PORT.load(Ordering::Acquire);
    if active_port != 0
        && main_codex_target_ready(&client, active_port).await
        && picker_monitor_running()
    {
        launch_platform_command(&app_path, active_port, false)?;
        return Ok(CodexAppLaunch {
            app_path,
            debug_port: active_port,
            reused: true,
        });
    }

    let debug_port = reserve_loopback_port()?;
    launch_platform_command(&app_path, debug_port, true)?;
    install_picker_unlock(&client, debug_port).await?;
    wait_for_main_codex_target(&client, debug_port).await?;
    ACTIVE_CODEX_DEBUG_PORT.store(debug_port, Ordering::Release);

    Ok(CodexAppLaunch {
        app_path,
        debug_port,
        reused: false,
    })
}

#[cfg(target_os = "macos")]
fn launch_platform_command(
    app_path: &Path,
    debug_port: u16,
    new_instance: bool,
) -> Result<(), AppError> {
    let command = macos_open_command(app_path, debug_port, new_instance);
    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .status()
        .map_err(|error| AppError::IoContext {
            context: format!("failed to launch {}", app_path.display()),
            source: error,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Message(format!(
            "failed to launch {}: open exited with {status}",
            app_path.display()
        )))
    }
}

#[cfg(not(target_os = "macos"))]
fn launch_platform_command(
    _app_path: &Path,
    _debug_port: u16,
    _new_instance: bool,
) -> Result<(), AppError> {
    Err(AppError::localized_ja(
        "codex.app.unsupported_platform",
        "当前版本的 OcHub Codex App 启动器仅支持 macOS。",
        "The OcHub Codex App launcher currently supports macOS only.",
        "現在、OcHub Codex App ランチャーは macOS のみをサポートしています。",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_candidates_include_current_and_legacy_bundle_names() {
        let candidates = codex_app_candidates(Path::new("/Users/test"));
        assert!(candidates.contains(&PathBuf::from("/Applications/Codex.app")));
        assert!(candidates.contains(&PathBuf::from("/Applications/ChatGPT.app")));
        assert!(candidates.contains(&PathBuf::from("/Users/test/Applications/Codex.app")));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_debug_launch_uses_a_new_instance_and_loopback_origin() {
        let command = macos_open_command(Path::new("/Applications/ChatGPT.app"), 43123, true);
        assert_eq!(
            command,
            vec![
                "open",
                "-n",
                "-a",
                "/Applications/ChatGPT.app",
                "--args",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=43123",
                "--remote-allow-origins=http://127.0.0.1:43123",
            ]
        );
    }

    #[test]
    fn target_picker_rejects_overlay_and_non_loopback_targets() {
        let target = |url: &str, websocket: &str| CdpTarget {
            target_type: "page".to_string(),
            title: "Codex".to_string(),
            url: url.to_string(),
            web_socket_debugger_url: Some(websocket.to_string()),
        };
        assert!(is_main_codex_target(
            &target(
                "app://-/index.html",
                "ws://127.0.0.1:43123/devtools/page/main"
            ),
            43123
        ));
        assert!(!is_main_codex_target(
            &target(
                "app://-/index.html?initialRoute=%2Favatar-overlay",
                "ws://127.0.0.1:43123/devtools/page/overlay"
            ),
            43123
        ));
        assert!(!is_main_codex_target(
            &target(
                "app://-/index.html",
                "ws://example.com:43123/devtools/page/main"
            ),
            43123
        ));
    }

    #[test]
    fn renderer_patch_removes_fast_and_ultra_account_gates() {
        let source = r#"async function uvn(e,t){if(e==null)return null;try{if((await t()).requirements?.featureRequirements?.fast_mode===!1)return null}catch(e){return null}return e}function X1r(e){let i=Ij(e),a=i?.authMethod===`chatgpt`,c={},u=!1,d=a&&!u&&c!=null&&c?.requirements?.featureRequirements?.fast_mode!==!1;return{isServiceTierAllowed:d}}async function Pna(e,t){let n=await jna(e,t);if(n!==`chatgpt`)return!1;return!0}function OYs(){let o={},s=o?.ultraEffortEnabled===!0;return s}function HTs(){let A={},j=A?.ultraEffortEnabled===!0;return j}"#;
        let (patched, stats) = patch_codex_picker_source(source).expect("supported renderer");
        assert_eq!(
            stats,
            PickerPatchStats {
                fast_ui_gates: 1,
                fast_auth_gates: 1,
                fast_request_gates: 1,
                ultra_gates: 2,
            }
        );
        assert!(patched.contains("d=!0"));
        assert!(patched.contains("if(n!==`chatgpt`)return!0"));
        assert!(patched.contains("fast_mode===!2"));
        assert!(patched.contains("s=!0"));
        assert!(patched.contains("j=!0"));
        assert!(!patched.contains("fast_mode===!1"));
        assert!(!patched.contains("fast_mode!==!1"));
        assert!(!patched.contains("ultraEffortEnabled"));
    }

    #[test]
    fn renderer_patch_fails_closed_when_codex_changes_its_gate_shape() {
        let error = patch_codex_picker_source("const app = 'Codex';").unwrap_err();
        assert!(error.contains("Fast UI: 0"));
        assert!(error.contains("Fast auth: 0"));
        assert!(error.contains("Fast request: 0"));
        assert!(error.contains("Ultra: 0"));
    }
}
