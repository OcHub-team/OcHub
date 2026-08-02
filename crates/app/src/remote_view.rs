//! Operator-facing control plane for headless OcHub nodes.
//!
//! The transport stays deliberately separate from GPUI: this view owns only
//! connection records, explicit host-key confirmation and presentation state.
//! Every remote operation goes through [`WorkspaceBackend`], so local and
//! remote behavior share the same typed application boundary.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt as _;
use gpui::{
    Context, Entity, FontWeight, MouseButton, ScrollHandle, SharedString, Window, div, prelude::*,
    px, relative,
};
use ochub_core::application::AppSummary;
use ochub_protocol::Capability;

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::i18n::{k, t};
use crate::icons::{IconName, icon};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::remote::{
    BootstrapProbe, NodeInstallStatus, NodeUpdateInstallResult, NodeUpdateReport, RemoteClient,
    RemoteConnectionIssue, RemoteConnectionIssueKind, RemoteHost, RemoteHostStore, ScannedHostKey,
    SshConfigEntry, WorkspaceBackend, discover_ssh_connections, install_bootstrap, probe_bootstrap,
    relay_node_update, scan_host_keys, trust_host_key,
};
use crate::text_input::TextInput;
use crate::{tf, theme};

fn icon_only_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    name: IconName,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label.into())
        .w(px(30.))
        .h(px(30.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .text_color(theme::muted())
        .hover(|button| button.bg(theme::surface_hover()).text_color(theme::text()))
        .child(icon(name, theme::muted(), 14.))
}

struct RemoteSnapshot {
    apps: Vec<AppSummary>,
}

struct RemoteConnectFailure {
    issue: Option<RemoteConnectionIssue>,
    message: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddMode {
    SshConfig,
    Manual,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeState {
    Checking,
    Online,
    Offline,
}

#[derive(Clone)]
struct NodeProbe {
    state: ProbeState,
    hostname: Option<String>,
    version: Option<String>,
    platform: Option<String>,
    quick_update_supported: bool,
    issue: Option<RemoteConnectionIssue>,
}

impl NodeProbe {
    fn checking() -> Self {
        Self {
            state: ProbeState::Checking,
            hostname: None,
            version: None,
            platform: None,
            quick_update_supported: false,
            issue: None,
        }
    }

    fn online(client: &RemoteClient) -> Self {
        let handshake = client.handshake();
        Self {
            state: ProbeState::Online,
            hostname: Some(handshake.node.hostname.clone()),
            version: Some(handshake.server_version.clone()),
            platform: Some(format!("{} · {}", handshake.node.os, handshake.node.arch)),
            quick_update_supported: handshake.capabilities.contains(&Capability::NodeUpdateRead),
            issue: None,
        }
    }

    fn offline(error: &crate::remote::RemoteClientError) -> Self {
        Self {
            state: ProbeState::Offline,
            hostname: None,
            version: None,
            platform: None,
            quick_update_supported: false,
            issue: Some(error.connection_issue()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeBootstrapPhase {
    Detecting,
    Ready,
    Downloading,
    Installing,
    Reconnecting,
    Complete,
    Failed,
}

#[derive(Clone)]
struct PreparedBootstrap {
    probe: BootstrapProbe,
    version: String,
    target: String,
    entry: ochub_core::services::update::headless::HeadlessPlatformEntry,
}

struct NodeBootstrapDialog {
    host: RemoteHost,
    issue: RemoteConnectionIssue,
    phase: NodeBootstrapPhase,
    prepared: Option<PreparedBootstrap>,
    installed_version: Option<String>,
    error: Option<String>,
}

impl NodeBootstrapDialog {
    fn detecting(host: RemoteHost, issue: RemoteConnectionIssue) -> Self {
        Self {
            host,
            issue,
            phase: NodeBootstrapPhase::Detecting,
            prepared: None,
            installed_version: None,
            error: None,
        }
    }

    fn working(&self) -> bool {
        matches!(
            self.phase,
            NodeBootstrapPhase::Detecting
                | NodeBootstrapPhase::Downloading
                | NodeBootstrapPhase::Installing
                | NodeBootstrapPhase::Reconnecting
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeUpdateStrategy {
    Automatic,
    Direct,
    Relay,
}

fn resolve_node_update_strategy(
    requested: NodeUpdateStrategy,
    direct_download: bool,
    relay_available: bool,
) -> NodeUpdateStrategy {
    match requested {
        NodeUpdateStrategy::Automatic if relay_available => NodeUpdateStrategy::Relay,
        NodeUpdateStrategy::Automatic if direct_download => NodeUpdateStrategy::Direct,
        NodeUpdateStrategy::Automatic => NodeUpdateStrategy::Relay,
        strategy => strategy,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeUpdatePhase {
    Checking,
    Ready,
    Downloading,
    Uploading,
    Installing,
    Reconnecting,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy)]
struct NodeUpdateProgress {
    phase: NodeUpdatePhase,
    done: u64,
    total: u64,
}

struct NodeUpdateDialog {
    host: RemoteHost,
    client: Option<Arc<RemoteClient>>,
    report: Option<NodeUpdateReport>,
    strategy: NodeUpdateStrategy,
    phase: NodeUpdatePhase,
    result: Option<NodeUpdateInstallResult>,
    error: Option<String>,
    progress: Option<(u64, u64)>,
    cancelled: Arc<AtomicBool>,
    effective_strategy: Option<NodeUpdateStrategy>,
}

impl NodeUpdateDialog {
    fn checking(host: RemoteHost) -> Self {
        Self {
            host,
            client: None,
            report: None,
            strategy: NodeUpdateStrategy::Automatic,
            phase: NodeUpdatePhase::Checking,
            result: None,
            error: None,
            progress: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            effective_strategy: None,
        }
    }

    fn working(&self) -> bool {
        matches!(
            self.phase,
            NodeUpdatePhase::Checking
                | NodeUpdatePhase::Downloading
                | NodeUpdatePhase::Uploading
                | NodeUpdatePhase::Installing
                | NodeUpdatePhase::Reconnecting
        )
    }

    fn cancellable(&self) -> bool {
        matches!(
            self.phase,
            NodeUpdatePhase::Checking | NodeUpdatePhase::Downloading | NodeUpdatePhase::Uploading
        )
    }
}

#[derive(Clone)]
pub(crate) struct RemoteScopeItem {
    pub id: String,
    pub name: String,
}

#[derive(Clone)]
pub(crate) enum RemoteEvent {
    ConnectionChanged { id: String, connected: bool },
    ManageRequested { id: String },
    NodeNamesChanged,
}

pub struct RemoteView {
    store: RemoteHostStore,
    selected_id: Option<String>,
    connection_state: ConnectionState,
    connection_generation: u64,
    client: Option<Arc<RemoteClient>>,
    backend: Option<WorkspaceBackend>,
    apps: Vec<AppSummary>,
    add_open: bool,
    add_mode: AddMode,
    ssh_config_entries: Vec<SshConfigEntry>,
    selected_ssh_config: Option<usize>,
    ssh_config_error: Option<String>,
    target_input: Entity<TextInput>,
    hostname_input: Entity<TextInput>,
    port_input: Entity<TextInput>,
    cli_input: Entity<TextInput>,
    scanned_keys: Vec<ScannedHostKey>,
    node_probes: HashMap<String, NodeProbe>,
    update_dialog: Option<NodeUpdateDialog>,
    bootstrap_dialog: Option<NodeBootstrapDialog>,
    issue_dialog: Option<(RemoteHost, RemoteConnectionIssue)>,
    probe_generation: u64,
    probing: bool,
    busy: bool,
    scroll: ScrollHandle,
    status: Option<SharedString>,
    status_level: Option<NotificationLevel>,
}

impl gpui::EventEmitter<RemoteEvent> for RemoteView {}

impl RemoteView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (store, status, status_level) = match RemoteHostStore::load() {
            Ok(store) => (store, None, None),
            Err(error) => (
                RemoteHostStore::default(),
                Some(SharedString::from(tf!(k::REMOTE_ERROR_LOAD, error = error))),
                Some(NotificationLevel::Error),
            ),
        };
        let selected_id = store.hosts().first().map(|host| host.id.clone());
        let target_input = cx.new(|cx| TextInput::new(cx, SharedString::from("user@host")));
        let hostname_input =
            cx.new(|cx| TextInput::new(cx, SharedString::from("host.example.com")));
        let port_input = cx.new(|cx| {
            let mut input = TextInput::new(cx, SharedString::from("22"));
            input.set_content("22", cx);
            input
        });
        let cli_input = cx.new(|cx| {
            let mut input = TextInput::new(cx, SharedString::from("ochcli"));
            input.set_content("ochcli", cx);
            input
        });
        let mut this = Self {
            store,
            selected_id,
            connection_state: ConnectionState::Disconnected,
            connection_generation: 0,
            client: None,
            backend: None,
            apps: Vec::new(),
            add_open: false,
            add_mode: AddMode::SshConfig,
            ssh_config_entries: Vec::new(),
            selected_ssh_config: None,
            ssh_config_error: None,
            target_input,
            hostname_input,
            port_input,
            cli_input,
            scanned_keys: Vec::new(),
            node_probes: HashMap::new(),
            update_dialog: None,
            bootstrap_dialog: None,
            issue_dialog: None,
            probe_generation: 0,
            probing: false,
            busy: false,
            scroll: ScrollHandle::new(),
            status,
            status_level,
        };
        // Saved connections no longer carry a second, local display name.
        // Probe immediately so every workspace surface can use the hostname
        // reported by OCH, even before the Remote page is opened.
        this.probe_nodes(cx);
        this
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        match RemoteHostStore::load() {
            Ok(store) => {
                self.store = store;
                if self
                    .selected_id
                    .as_deref()
                    .is_none_or(|id| self.store.get(id).is_none())
                {
                    self.selected_id = self.store.hosts().first().map(|host| host.id.clone());
                }
            }
            Err(error) => self.set_status(
                tf!(k::REMOTE_ERROR_LOAD, error = error),
                NotificationLevel::Error,
            ),
        }
        if self.connection_state == ConnectionState::Connected {
            self.refresh_remote(cx);
        }
        self.probe_nodes(cx);
        cx.notify();
    }

    pub(crate) fn scope_items(&self) -> Vec<RemoteScopeItem> {
        self.store
            .hosts()
            .iter()
            .map(|host| RemoteScopeItem {
                id: host.id.clone(),
                name: self.node_name(host),
            })
            .collect()
    }

    fn node_name(&self, host: &RemoteHost) -> String {
        self.client
            .as_ref()
            .filter(|client| client.host().id == host.id)
            .map(|client| client.handshake().node.hostname.clone())
            .or_else(|| {
                self.node_probes
                    .get(&host.id)
                    .and_then(|probe| probe.hostname.clone())
            })
            // A live OCH handshake is not available while a connection is
            // being added or is unreachable. The SSH target is a transient UI
            // fallback, not a separately persisted display name.
            .unwrap_or_else(|| host.ssh_alias.clone())
    }

    pub(crate) fn backend_for_scope(&self, id: &str) -> Option<WorkspaceBackend> {
        (self.selected_id.as_deref() == Some(id)
            && self.connection_state == ConnectionState::Connected)
            .then(|| self.backend.clone())
            .flatten()
    }

    pub(crate) fn enabled_builtin_apps(&self) -> Vec<ochub_core::AppType> {
        self.apps
            .iter()
            .filter(|app| app.enabled && app.supports_provider)
            .filter_map(|app| app.id.parse().ok())
            .collect()
    }

    pub(crate) fn activate_scope(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected_id.as_deref() != Some(id.as_str()) {
            self.select_host(id.clone(), cx);
        }
        if self.connection_state == ConnectionState::Disconnected {
            self.connect_host(id, cx);
        }
    }

    fn set_status(&mut self, message: impl Into<SharedString>, level: NotificationLevel) {
        self.status = Some(message.into());
        self.status_level = Some(level);
    }

    fn open_add(&mut self, cx: &mut Context<Self>) {
        self.add_open = true;
        self.add_mode = AddMode::SshConfig;
        self.scanned_keys.clear();
        self.ssh_config_error = None;
        match discover_ssh_connections() {
            Ok(entries) => {
                self.ssh_config_entries = entries;
                self.selected_ssh_config = self
                    .ssh_config_entries
                    .iter()
                    .position(|entry| !self.has_ssh_alias(&entry.alias));
            }
            Err(error) => {
                self.ssh_config_entries.clear();
                self.selected_ssh_config = None;
                self.ssh_config_error = Some(error.to_string());
            }
        }
        cx.notify();
    }

    fn cancel_add(&mut self, cx: &mut Context<Self>) {
        self.add_open = false;
        self.selected_ssh_config = None;
        self.scanned_keys.clear();
        cx.notify();
    }

    fn has_ssh_alias(&self, alias: &str) -> bool {
        self.store
            .hosts()
            .iter()
            .any(|host| host.ssh_alias == alias)
    }

    fn show_manual_add(&mut self, cx: &mut Context<Self>) {
        self.add_mode = AddMode::Manual;
        self.scanned_keys.clear();
        self.target_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.hostname_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.port_input
            .update(cx, |input, cx| input.set_content("22", cx));
        self.cli_input
            .update(cx, |input, cx| input.set_content("ochcli", cx));
        cx.notify();
    }

    fn show_ssh_config_add(&mut self, cx: &mut Context<Self>) {
        self.add_mode = AddMode::SshConfig;
        self.scanned_keys.clear();
        cx.notify();
    }

    fn select_ssh_config(&mut self, index: usize, cx: &mut Context<Self>) {
        if self
            .ssh_config_entries
            .get(index)
            .is_some_and(|entry| !self.has_ssh_alias(&entry.alias))
        {
            self.selected_ssh_config = Some(index);
            cx.notify();
        }
    }

    fn scan_selected_ssh_config(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self
            .selected_ssh_config
            .and_then(|index| self.ssh_config_entries.get(index))
            .cloned()
        else {
            return;
        };
        if self.has_ssh_alias(&entry.alias) {
            return;
        }
        self.target_input
            .update(cx, |input, cx| input.set_content(&entry.alias, cx));
        self.hostname_input
            .update(cx, |input, cx| input.set_content(&entry.hostname, cx));
        self.port_input.update(cx, |input, cx| {
            input.set_content(entry.port.to_string(), cx)
        });
        self.cli_input
            .update(cx, |input, cx| input.set_content("ochcli", cx));
        self.scan(cx);
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let target = self.target_input.read(cx).content().trim().to_string();
        let hostname = self.hostname_input.read(cx).content().trim().to_string();
        if target.is_empty() || hostname.is_empty() {
            self.set_status(t(k::REMOTE_ERROR_REQUIRED), NotificationLevel::Error);
            cx.notify();
            return;
        }
        let Ok(port) = self.port_input.read(cx).content().trim().parse::<u16>() else {
            self.set_status(t(k::REMOTE_ERROR_PORT), NotificationLevel::Error);
            cx.notify();
            return;
        };
        if port == 0 {
            self.set_status(t(k::REMOTE_ERROR_PORT), NotificationLevel::Error);
            cx.notify();
            return;
        }
        self.busy = true;
        self.scanned_keys.clear();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result =
                crate::core_async::run(async move { scan_host_keys(&hostname, port).await }).await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(keys) => this.scanned_keys = keys,
                    Err(error) => this.set_status(
                        tf!(k::REMOTE_ERROR_SCAN, error = error),
                        NotificationLevel::Error,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn trust_and_connect(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.scanned_keys.is_empty() {
            return;
        }
        let port = match self.port_input.read(cx).content().trim().parse::<u16>() {
            Ok(port) if port > 0 => port,
            _ => {
                self.set_status(t(k::REMOTE_ERROR_PORT), NotificationLevel::Error);
                cx.notify();
                return;
            }
        };
        for key in &self.scanned_keys {
            if let Err(error) = trust_host_key(key) {
                self.set_status(
                    tf!(k::REMOTE_ERROR_STORE, error = error),
                    NotificationLevel::Error,
                );
                cx.notify();
                return;
            }
        }
        let host = RemoteHost {
            id: uuid::Uuid::new_v4().to_string(),
            ssh_alias: self.target_input.read(cx).content().trim().to_string(),
            hostname: Some(self.hostname_input.read(cx).content().trim().to_string()),
            port: Some(port),
            remote_node_id: None,
            host_key_fingerprint: self.scanned_keys.first().map(|key| key.fingerprint.clone()),
            ochcli_path: self.cli_input.read(cx).content().trim().to_string(),
            tags: Vec::new(),
            last_seen_at: None,
        };
        let id = host.id.clone();
        if let Err(error) = self.store.upsert(host) {
            self.set_status(
                tf!(k::REMOTE_ERROR_STORE, error = error),
                NotificationLevel::Error,
            );
            cx.notify();
            return;
        }
        self.selected_id = Some(id.clone());
        self.add_open = false;
        self.selected_ssh_config = None;
        self.scanned_keys.clear();
        self.connect_host(id, cx);
    }

    fn probe_nodes(&mut self, cx: &mut Context<Self>) {
        let connected_id = (self.connection_state == ConnectionState::Connected)
            .then(|| self.selected_id.clone())
            .flatten();
        let hosts = self
            .store
            .hosts()
            .iter()
            .filter(|host| connected_id.as_deref() != Some(host.id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if hosts.is_empty() {
            self.probing = false;
            return;
        }
        self.probe_generation = self.probe_generation.wrapping_add(1);
        let generation = self.probe_generation;
        self.probing = true;
        for host in &hosts {
            self.node_probes
                .insert(host.id.clone(), NodeProbe::checking());
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let results = crate::core_async::run(async move {
                futures::stream::iter(hosts.into_iter().map(|host| async move {
                    let id = host.id.clone();
                    let result = match RemoteClient::connect(host).await {
                        Ok(client) => {
                            let probe = NodeProbe::online(&client);
                            let _ = client.close().await;
                            probe
                        }
                        Err(error) => NodeProbe::offline(&error),
                    };
                    (id, result)
                }))
                .buffer_unordered(4)
                .collect::<Vec<_>>()
                .await
            })
            .await;
            this.update(cx, |this, cx| {
                if this.probe_generation != generation {
                    return;
                }
                this.probing = false;
                for (id, probe) in results {
                    this.node_probes.insert(id, probe);
                }
                cx.emit(RemoteEvent::NodeNamesChanged);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_host(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected_id.as_deref() == Some(id.as_str()) {
            return;
        }
        self.disconnect(cx);
        self.selected_id = Some(id);
        cx.notify();
    }

    fn connect_host(&mut self, id: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(host) = self.store.get(&id).cloned() else {
            return;
        };
        self.connection_generation = self.connection_generation.wrapping_add(1);
        let generation = self.connection_generation;
        self.busy = true;
        self.connection_state = ConnectionState::Connecting;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                let client =
                    RemoteClient::connect(host)
                        .await
                        .map_err(|error| RemoteConnectFailure {
                            issue: Some(error.connection_issue()),
                            message: error.to_string(),
                        })?;
                let backend = WorkspaceBackend::remote(client.clone());
                let snapshot = load_snapshot(backend.clone()).await.map_err(|message| {
                    RemoteConnectFailure {
                        issue: None,
                        message,
                    }
                })?;
                Ok::<_, RemoteConnectFailure>((client, backend, snapshot))
            })
            .await;
            this.update(cx, |this, cx| {
                if this.connection_generation != generation {
                    return;
                }
                this.busy = false;
                match result {
                    Ok((client, backend, snapshot)) => {
                        let node_id = client.handshake().node.id.clone();
                        let node_name = client.handshake().node.hostname.clone();
                        let host_id = client.host().id.clone();
                        let probe = NodeProbe::online(&client);
                        this.client = Some(client);
                        this.backend = Some(backend);
                        this.install_snapshot(snapshot);
                        this.connection_state = ConnectionState::Connected;
                        this.node_probes.insert(host_id.clone(), probe);
                        if let Some(mut host) = this.store.get(&host_id).cloned() {
                            host.remote_node_id = Some(node_id);
                            host.last_seen_at = Some(chrono::Utc::now().to_rfc3339());
                            if let Err(error) = this.store.upsert(host) {
                                this.set_status(
                                    tf!(k::REMOTE_ERROR_STORE, error = error),
                                    NotificationLevel::Warning,
                                );
                            } else {
                                this.set_status(
                                    tf!(k::REMOTE_SUCCESS_CONNECTED, node = node_name),
                                    NotificationLevel::Success,
                                );
                            }
                        }
                        cx.emit(RemoteEvent::ConnectionChanged {
                            id: host_id,
                            connected: true,
                        });
                    }
                    Err(error) => {
                        this.connection_state = ConnectionState::Disconnected;
                        if let Some(issue) = error.issue {
                            this.node_probes.insert(
                                id.clone(),
                                NodeProbe {
                                    state: ProbeState::Offline,
                                    hostname: None,
                                    version: None,
                                    platform: None,
                                    quick_update_supported: false,
                                    issue: Some(issue.clone()),
                                },
                            );
                            this.set_status(
                                tf!(
                                    k::REMOTE_ERROR_CONNECT,
                                    error = remote_issue_title(issue.kind)
                                ),
                                NotificationLevel::Error,
                            );
                        } else {
                            this.set_status(
                                tf!(k::REMOTE_ERROR_CONNECT, error = error.message),
                                NotificationLevel::Error,
                            );
                        }
                        cx.emit(RemoteEvent::ConnectionChanged {
                            id,
                            connected: false,
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        let disconnected_id = self.selected_id.clone();
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.busy = false;
        self.connection_state = ConnectionState::Disconnected;
        self.backend = None;
        self.apps.clear();
        if let Some(client) = self.client.take() {
            cx.spawn(async move |_this, _cx| {
                let _ = crate::core_async::run(async move { client.close().await }).await;
            })
            .detach();
        }
        if let Some(id) = disconnected_id {
            cx.emit(RemoteEvent::ConnectionChanged {
                id,
                connected: false,
            });
        }
        cx.notify();
    }

    fn remove_host(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected_id.as_deref() == Some(id.as_str()) {
            self.disconnect(cx);
            self.selected_id = None;
        }
        match self.store.remove(&id) {
            Ok(_) => {
                self.node_probes.remove(&id);
                if self.selected_id.is_none() {
                    self.selected_id = self.store.hosts().first().map(|host| host.id.clone());
                }
                self.set_status(t(k::REMOTE_SUCCESS_REMOVED), NotificationLevel::Success);
            }
            Err(error) => self.set_status(
                tf!(k::REMOTE_ERROR_STORE, error = error),
                NotificationLevel::Error,
            ),
        }
        cx.notify();
    }

    fn open_issue_details(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(host) = self.store.get(&id).cloned() else {
            return;
        };
        let Some(issue) = self
            .node_probes
            .get(&id)
            .and_then(|probe| probe.issue.clone())
        else {
            return;
        };
        self.issue_dialog = Some((host, issue));
        cx.notify();
    }

    fn close_issue_details(&mut self, cx: &mut Context<Self>) {
        self.issue_dialog = None;
        cx.notify();
    }

    fn open_node_bootstrap(&mut self, id: String, cx: &mut Context<Self>) {
        if self
            .bootstrap_dialog
            .as_ref()
            .is_some_and(NodeBootstrapDialog::working)
        {
            return;
        }
        let Some(host) = self.store.get(&id).cloned() else {
            return;
        };
        let Some(issue) = self
            .node_probes
            .get(&id)
            .and_then(|probe| probe.issue.clone())
            .filter(|issue| issue.kind.can_bootstrap())
        else {
            return;
        };
        self.issue_dialog = None;
        self.bootstrap_dialog = Some(NodeBootstrapDialog::detecting(host.clone(), issue));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                let probe = probe_bootstrap(&host)
                    .await
                    .map_err(|error| localized_remote_error(&error.connection_issue()))?;
                let platform = format!("{}-{}", probe.os, probe.arch);
                let manifest = ochub_core::services::update::headless::fetch_manifest(None)
                    .await
                    .map_err(|error| error.to_string())?;
                let (target, entry) = manifest
                    .entry_for(&probe.os, &probe.arch)
                    .ok_or_else(|| tf!(k::REMOTE_BOOTSTRAP_UNSUPPORTED, platform = platform))?;
                if entry.signature.trim().is_empty()
                    || !ochub_core::services::update::manifest::signing_configured()
                {
                    return Err(t(k::REMOTE_UPDATE_UNSIGNED).to_string());
                }
                Ok::<_, String>(PreparedBootstrap {
                    probe,
                    version: manifest.version.clone(),
                    target: target.to_string(),
                    entry: entry.clone(),
                })
            })
            .await;
            this.update(cx, |this, cx| {
                let Some(dialog) = this
                    .bootstrap_dialog
                    .as_mut()
                    .filter(|dialog| dialog.host.id == id)
                else {
                    return;
                };
                match result {
                    Ok(prepared) => {
                        dialog.prepared = Some(prepared);
                        dialog.phase = NodeBootstrapPhase::Ready;
                        dialog.error = None;
                    }
                    Err(error) => {
                        dialog.phase = NodeBootstrapPhase::Failed;
                        dialog.error = Some(error);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_node_bootstrap(&mut self, cx: &mut Context<Self>) {
        if self
            .bootstrap_dialog
            .as_ref()
            .is_some_and(NodeBootstrapDialog::working)
        {
            return;
        }
        self.bootstrap_dialog = None;
        cx.notify();
    }

    fn install_node_bootstrap(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.bootstrap_dialog.as_mut() else {
            return;
        };
        if dialog.phase != NodeBootstrapPhase::Ready {
            return;
        }
        let Some(prepared) = dialog.prepared.clone() else {
            return;
        };
        dialog.phase = NodeBootstrapPhase::Downloading;
        dialog.error = None;
        let host = dialog.host.clone();
        let host_id = host.id.clone();
        let target_version = prepared.version.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let download = crate::core_async::run({
                let entry = prepared.entry.clone();
                async move {
                    ochub_core::services::update::headless::download(&entry)
                        .await
                        .map_err(|error| error.to_string())
                }
            })
            .await;
            let Ok(payload) = download else {
                this.update(cx, |this, cx| {
                    if let Some(dialog) = this
                        .bootstrap_dialog
                        .as_mut()
                        .filter(|dialog| dialog.host.id == host_id)
                    {
                        dialog.phase = NodeBootstrapPhase::Failed;
                        dialog.error = Some(download.unwrap_err());
                    }
                    cx.notify();
                })
                .ok();
                return;
            };
            if this
                .update(cx, |this, cx| {
                    if let Some(dialog) = this
                        .bootstrap_dialog
                        .as_mut()
                        .filter(|dialog| dialog.host.id == host_id)
                    {
                        dialog.phase = NodeBootstrapPhase::Installing;
                    }
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            let installation = crate::core_async::run({
                let host = host.clone();
                let entry = prepared.entry.clone();
                async move {
                    install_bootstrap(&host, &entry, &payload)
                        .await
                        .map_err(|error| localized_remote_error(&error.connection_issue()))
                }
            })
            .await;
            let Ok(installation) = installation else {
                this.update(cx, |this, cx| {
                    if let Some(dialog) = this
                        .bootstrap_dialog
                        .as_mut()
                        .filter(|dialog| dialog.host.id == host_id)
                    {
                        dialog.phase = NodeBootstrapPhase::Failed;
                        dialog.error = Some(installation.unwrap_err());
                    }
                    cx.notify();
                })
                .ok();
                return;
            };
            let mut installed_host = host.clone();
            installed_host.ochcli_path = installation.executable.display().to_string();
            let persisted = this.update(cx, |this, cx| {
                let result = this
                    .store
                    .upsert(installed_host.clone())
                    .map_err(|error| error.to_string());
                if let Some(dialog) = this
                    .bootstrap_dialog
                    .as_mut()
                    .filter(|dialog| dialog.host.id == host_id)
                {
                    dialog.host = installed_host.clone();
                    dialog.phase = if result.is_ok() {
                        NodeBootstrapPhase::Reconnecting
                    } else {
                        NodeBootstrapPhase::Failed
                    };
                    dialog.error = result.as_ref().err().cloned();
                }
                cx.notify();
                result
            });
            if !matches!(persisted, Ok(Ok(()))) {
                return;
            }
            let reconnect = crate::core_async::run({
                let installed_host = installed_host.clone();
                let target_version = target_version.clone();
                async move {
                    let mut last_error = None;
                    for _ in 0..30 {
                        match RemoteClient::connect(installed_host.clone()).await {
                            Ok(client) if client.handshake().server_version == target_version => {
                                let probe = NodeProbe::online(&client);
                                let node_id = client.handshake().node.id.clone();
                                let _ = client.close().await;
                                return Ok::<_, String>((probe, node_id));
                            }
                            Ok(client) => {
                                last_error = Some(format!(
                                    "node reconnected with version {}, expected {}",
                                    client.handshake().server_version,
                                    target_version
                                ));
                                let _ = client.close().await;
                            }
                            Err(error) => {
                                last_error =
                                    Some(localized_remote_error(&error.connection_issue()));
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                    Err(last_error.unwrap_or_else(|| {
                        t(k::REMOTE_ISSUE_CONNECTION_TIMED_OUT_HELP).to_string()
                    }))
                }
            })
            .await;
            this.update(cx, |this, cx| {
                let Some(dialog) = this
                    .bootstrap_dialog
                    .as_mut()
                    .filter(|dialog| dialog.host.id == host_id)
                else {
                    return;
                };
                match reconnect {
                    Ok((probe, node_id)) => {
                        let mut saved_host = installed_host;
                        saved_host.remote_node_id = Some(node_id);
                        saved_host.last_seen_at = Some(chrono::Utc::now().to_rfc3339());
                        if let Err(error) = this.store.upsert(saved_host) {
                            dialog.phase = NodeBootstrapPhase::Failed;
                            dialog.error = Some(error.to_string());
                            cx.notify();
                            return;
                        }
                        dialog.phase = NodeBootstrapPhase::Complete;
                        dialog.installed_version = Some(target_version.clone());
                        dialog.error = None;
                        this.node_probes.insert(host_id.clone(), probe);
                        this.set_status(
                            tf!(k::REMOTE_BOOTSTRAP_SUCCESS, version = target_version),
                            NotificationLevel::Success,
                        );
                        if this.selected_id.as_deref() == Some(host_id.as_str())
                            && this.connection_state != ConnectionState::Connected
                        {
                            this.connect_host(host_id.clone(), cx);
                        }
                    }
                    Err(error) => {
                        dialog.phase = NodeBootstrapPhase::Failed;
                        dialog.error = Some(error);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_node_update(&mut self, id: String, cx: &mut Context<Self>) {
        if self
            .update_dialog
            .as_ref()
            .is_some_and(NodeUpdateDialog::working)
        {
            return;
        }
        let Some(host) = self.store.get(&id).cloned() else {
            return;
        };
        let existing = (self.selected_id.as_deref() == Some(id.as_str())
            && self.connection_state == ConnectionState::Connected)
            .then(|| self.client.clone())
            .flatten();
        self.update_dialog = Some(NodeUpdateDialog::checking(host.clone()));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                let client = match existing {
                    Some(client) => client,
                    None => RemoteClient::connect(host)
                        .await
                        .map_err(|error| error.to_string())?,
                };
                if !client
                    .handshake()
                    .capabilities
                    .contains(&Capability::NodeUpdateRead)
                {
                    return Err(t(k::REMOTE_UPDATE_LEGACY).to_string());
                }
                let backend = WorkspaceBackend::remote(client.clone());
                let report = match backend.node_update_check().await {
                    Ok(report) => report,
                    Err(remote_error) => {
                        // A node that cannot reach GitHub must still be
                        // updatable. Read its local installation state, fetch
                        // and verify the release metadata on this Mac, and
                        // force the automatic strategy onto the relay path.
                        let installation = backend
                            .node_update_status()
                            .await
                            .map_err(|error| error.to_string())?;
                        let manifest = ochub_core::services::update::headless::fetch_manifest(None)
                            .await
                            .map_err(|error| error.to_string())?;
                        fallback_node_update_report(
                            installation,
                            manifest,
                            &client.handshake().node.os,
                            &client.handshake().node.arch,
                            remote_error.to_string(),
                        )?
                    }
                };
                Ok::<_, String>((client, report))
            })
            .await;
            this.update(cx, |this, cx| {
                let Some(dialog) = this.update_dialog.as_mut() else {
                    return;
                };
                if dialog.host.id != id {
                    return;
                }
                if dialog.cancelled.load(Ordering::Relaxed) {
                    if let Ok((client, _)) = result {
                        cx.spawn(async move |_this, _cx| {
                            let _ =
                                crate::core_async::run(async move { client.close().await }).await;
                        })
                        .detach();
                    }
                    return;
                }
                match result {
                    Ok((client, report)) => {
                        dialog.client = Some(client);
                        dialog.report = Some(report);
                        dialog.phase = NodeUpdatePhase::Ready;
                        dialog.error = None;
                    }
                    Err(error) => {
                        dialog.phase = NodeUpdatePhase::Failed;
                        dialog.error = Some(error);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_node_update_strategy(
        &mut self,
        strategy: NodeUpdateStrategy,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.update_dialog.as_mut() else {
            return;
        };
        if dialog.phase == NodeUpdatePhase::Ready {
            dialog.strategy = strategy;
            cx.notify();
        }
    }

    fn close_node_update(&mut self, cx: &mut Context<Self>) {
        if self
            .update_dialog
            .as_ref()
            .is_some_and(NodeUpdateDialog::working)
        {
            return;
        }
        let client = self
            .update_dialog
            .take()
            .and_then(|dialog| dialog.client)
            .filter(|client| {
                self.client
                    .as_ref()
                    .is_none_or(|current| !Arc::ptr_eq(current, client))
            });
        if let Some(client) = client {
            cx.spawn(async move |_this, _cx| {
                let _ = crate::core_async::run(async move { client.close().await }).await;
            })
            .detach();
        }
        cx.notify();
    }

    fn cancel_node_update(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.update_dialog.as_mut() else {
            return;
        };
        if !dialog.cancellable() {
            return;
        }
        dialog.cancelled.store(true, Ordering::Relaxed);
        dialog.phase = NodeUpdatePhase::Cancelled;
        dialog.progress = None;
        dialog.error = None;

        let direct = dialog.effective_strategy == Some(NodeUpdateStrategy::Direct);
        let client = direct.then(|| dialog.client.clone()).flatten();
        let host_id = dialog.host.id.clone();
        if self.selected_id.as_deref() == Some(host_id.as_str()) {
            self.busy = false;
            if direct {
                self.client = None;
                self.backend = None;
                self.connection_state = ConnectionState::Disconnected;
                cx.emit(RemoteEvent::ConnectionChanged {
                    id: host_id,
                    connected: false,
                });
            } else {
                self.connection_state = ConnectionState::Connected;
            }
        }
        if let Some(client) = client {
            cx.spawn(async move |_this, _cx| {
                let _ = crate::core_async::run(async move { client.close().await }).await;
            })
            .detach();
        }
        self.probe_nodes(cx);
        cx.notify();
    }

    fn install_node_update(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.update_dialog.as_mut() else {
            return;
        };
        if dialog.phase != NodeUpdatePhase::Ready {
            return;
        }
        let (Some(client), Some(report)) = (dialog.client.clone(), dialog.report.clone()) else {
            return;
        };
        if !report.update.has_update {
            dialog.phase = NodeUpdatePhase::Complete;
            cx.notify();
            return;
        }
        if !report.installation.can_self_update || !report.update.signed {
            dialog.phase = NodeUpdatePhase::Failed;
            dialog.error = Some(t(k::REMOTE_UPDATE_UNSIGNED).to_string());
            cx.notify();
            return;
        }
        let relay_available = client
            .handshake()
            .capabilities
            .contains(&Capability::NodeUpdateRelay);
        let strategy = resolve_node_update_strategy(
            dialog.strategy,
            report.update.direct_download,
            relay_available,
        );
        let required = match strategy {
            NodeUpdateStrategy::Direct => Capability::NodeUpdateInstall,
            NodeUpdateStrategy::Relay => Capability::NodeUpdateRelay,
            NodeUpdateStrategy::Automatic => unreachable!(),
        };
        if let Err(error) = client.require_capability(required) {
            dialog.phase = NodeUpdatePhase::Failed;
            dialog.error = Some(error.to_string());
            cx.notify();
            return;
        }
        dialog.phase = NodeUpdatePhase::Downloading;
        dialog.progress = None;
        dialog.cancelled.store(false, Ordering::Relaxed);
        dialog.effective_strategy = Some(strategy);
        dialog.error = None;
        let cancelled = dialog.cancelled.clone();
        let host = dialog.host.clone();
        let host_id = host.id.clone();
        let active = self.selected_id.as_deref() == Some(host_id.as_str())
            && self.connection_state == ConnectionState::Connected;
        if active {
            self.connection_state = ConnectionState::Connecting;
            self.busy = true;
        }
        cx.notify();
        let (progress_tx, mut progress_rx) =
            futures::channel::mpsc::unbounded::<NodeUpdateProgress>();
        let progress_host_id = host_id.clone();
        cx.spawn(async move |this, cx| {
            while let Some(progress) = progress_rx.next().await {
                this.update(cx, |this, cx| {
                    if let Some(dialog) = this
                        .update_dialog
                        .as_mut()
                        .filter(|dialog| dialog.host.id == progress_host_id)
                        .filter(|dialog| !dialog.cancelled.load(Ordering::Relaxed))
                    {
                        dialog.phase = progress.phase;
                        dialog.progress = Some((progress.done, progress.total));
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let install_client = client.clone();
            let install_cancelled = cancelled.clone();
            let install = crate::core_async::run(async move {
                match strategy {
                    NodeUpdateStrategy::Direct => {
                        WorkspaceBackend::remote(install_client.clone())
                            .node_update_install_direct()
                            .await
                            .map_err(|error| error.to_string())
                    }
                    NodeUpdateStrategy::Relay => {
                        let manifest = ochub_core::services::update::headless::fetch_manifest(None)
                            .await
                            .map_err(|error| error.to_string())?;
                        if manifest.version != report.update.latest_version {
                            return Err(format!(
                                "latest node release changed from {} to {}; check again before installing",
                                report.update.latest_version, manifest.version
                            ));
                        }
                        let node = &install_client.handshake().node;
                        let (target, entry) =
                            manifest.entry_for(&node.os, &node.arch).ok_or_else(|| {
                                format!(
                                    "release {} has no node executable for {}-{}",
                                    manifest.version, node.os, node.arch
                                )
                            })?;
                        let entry_size = entry.size;
                        let download_progress = progress_tx.clone();
                        let download_cancelled = install_cancelled.clone();
                        let payload = ochub_core::services::update::headless::download_with_progress(
                            entry,
                            Some(Box::new(move |done, total| {
                                let _ = download_progress.unbounded_send(NodeUpdateProgress {
                                    phase: NodeUpdatePhase::Downloading,
                                    done,
                                    total: total.unwrap_or(entry_size),
                                });
                            })),
                            Some(Box::new(move || {
                                download_cancelled.load(Ordering::Relaxed)
                            })),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        if install_cancelled.load(Ordering::Relaxed) {
                            return Err("node update was cancelled".to_string());
                        }
                        let upload_progress = progress_tx.clone();
                        let value = relay_node_update(
                            install_client.host(),
                            &node.id,
                            &manifest.version,
                            target,
                            entry,
                            &payload,
                            install_cancelled.clone(),
                            move |done, total| {
                                let _ = upload_progress.unbounded_send(NodeUpdateProgress {
                                    phase: if done == total {
                                        NodeUpdatePhase::Installing
                                    } else {
                                        NodeUpdatePhase::Uploading
                                    },
                                    done,
                                    total,
                                });
                            },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        serde_json::from_value(value).map_err(|error| error.to_string())
                    }
                    NodeUpdateStrategy::Automatic => unreachable!(),
                }
            })
            .await;
            let Ok(result) = install else {
                let install_error = install.unwrap_err();
                this.update(cx, |this, cx| {
                    if let Some(dialog) = this
                        .update_dialog
                        .as_mut()
                        .filter(|dialog| dialog.host.id == host_id)
                    {
                        if dialog.cancelled.load(Ordering::Relaxed) {
                            dialog.phase = NodeUpdatePhase::Cancelled;
                            dialog.error = None;
                        } else {
                            dialog.phase = NodeUpdatePhase::Failed;
                            dialog.error = Some(install_error);
                        }
                    }
                    if active {
                        if strategy == NodeUpdateStrategy::Relay {
                            this.connection_state = ConnectionState::Connected;
                        }
                        this.busy = false;
                    }
                    this.probe_nodes(cx);
                    cx.notify();
                })
                .ok();
                return;
            };
            this.update(cx, |this, cx| {
                if let Some(dialog) = this
                    .update_dialog
                    .as_mut()
                    .filter(|dialog| dialog.host.id == host_id)
                {
                    dialog.phase = NodeUpdatePhase::Reconnecting;
                }
                cx.notify();
            })
            .ok();
            let target_version = result.version.clone();
            let _ = crate::core_async::run({
                let client = client.clone();
                async move { client.close().await }
            })
            .await;
            let reconnect = crate::core_async::run(async move {
                let mut last_error = None;
                for _ in 0..30 {
                    match RemoteClient::connect(host.clone()).await {
                        Ok(new_client)
                            if new_client.handshake().server_version == target_version =>
                        {
                            let probe = NodeProbe::online(&new_client);
                            if active {
                                let backend = WorkspaceBackend::remote(new_client.clone());
                                let snapshot = load_snapshot(backend.clone()).await?;
                                return Ok::<_, String>((
                                    new_client,
                                    Some(backend),
                                    Some(snapshot),
                                    probe,
                                ));
                            }
                            return Ok((new_client, None, None, probe));
                        }
                        Ok(new_client) => {
                            last_error = Some(format!(
                                "node reconnected with version {}, expected {}",
                                new_client.handshake().server_version,
                                target_version
                            ));
                            let _ = new_client.close().await;
                        }
                        Err(error) => last_error = Some(error.to_string()),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(last_error.unwrap_or_else(|| "remote node did not reconnect".to_string()))
            })
            .await;
            this.update(cx, |this, cx| {
                if !this
                    .update_dialog
                    .as_ref()
                    .filter(|dialog| dialog.host.id == host_id)
                    .is_some()
                {
                    return;
                }
                match reconnect {
                    Ok((new_client, backend, snapshot, probe)) => {
                        let version = result.version.clone();
                        if let Some(dialog) = this.update_dialog.as_mut() {
                            dialog.client = Some(new_client.clone());
                            dialog.phase = NodeUpdatePhase::Complete;
                            dialog.result = Some(result);
                            dialog.error = None;
                        }
                        this.node_probes.insert(host_id.clone(), probe);
                        if active {
                            this.client = Some(new_client);
                            this.backend = backend;
                            if let Some(snapshot) = snapshot {
                                this.install_snapshot(snapshot);
                            }
                            this.busy = false;
                            this.connection_state = ConnectionState::Connected;
                            cx.emit(RemoteEvent::ConnectionChanged {
                                id: host_id.clone(),
                                connected: true,
                            });
                        } else {
                            let client = new_client;
                            cx.spawn(async move |_this, _cx| {
                                let _ =
                                    crate::core_async::run(async move { client.close().await }).await;
                            })
                            .detach();
                            if let Some(dialog) = this.update_dialog.as_mut() {
                                dialog.client = None;
                            }
                        }
                        this.set_status(
                            tf!(k::REMOTE_UPDATE_SUCCESS, version = version),
                            NotificationLevel::Success,
                        );
                    }
                    Err(error) => {
                        if let Some(dialog) = this.update_dialog.as_mut() {
                            dialog.phase = NodeUpdatePhase::Failed;
                            dialog.error = Some(error);
                        }
                        if active {
                            this.busy = false;
                            this.connection_state = ConnectionState::Disconnected;
                            this.client = None;
                            this.backend = None;
                            cx.emit(RemoteEvent::ConnectionChanged {
                                id: host_id.clone(),
                                connected: false,
                            });
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn back_from_fingerprint(&mut self, cx: &mut Context<Self>) {
        self.scanned_keys.clear();
        cx.notify();
    }

    fn install_snapshot(&mut self, snapshot: RemoteSnapshot) {
        self.apps = snapshot.apps;
    }

    fn refresh_remote(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(backend) = self.backend.clone() else {
            return;
        };
        self.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move { load_snapshot(backend).await }).await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(snapshot) => this.install_snapshot(snapshot),
                    Err(error) => this.set_status(
                        tf!(k::REMOTE_ERROR_LOAD, error = error),
                        NotificationLevel::Error,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_add_modal(&self, cx: &mut Context<Self>) -> gpui::Div {
        let close = cx.listener(|this: &mut Self, _: &(), _window, cx| this.cancel_add(cx));
        let card = if !self.scanned_keys.is_empty() {
            self.render_fingerprint_card(cx)
        } else {
            match self.add_mode {
                AddMode::SshConfig => self.render_ssh_config_card(cx),
                AddMode::Manual => self.render_manual_add_card(cx),
            }
        };
        components::modal_overlay(card)
            .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                close(&(), window, cx)
            })
    }

    fn render_issue_details_modal(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some((host, issue)) = self.issue_dialog.as_ref() else {
            return div();
        };
        let node_name = self.node_name(host);
        let close_footer =
            cx.listener(|this: &mut Self, _: &(), _window, cx| this.close_issue_details(cx));
        let close_icon =
            cx.listener(|this: &mut Self, _: &(), _window, cx| this.close_issue_details(cx));
        let host_id = host.id.clone();
        let bootstrap = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
            this.open_node_bootstrap(host_id.clone(), cx)
        });
        let diagnostics = if issue.diagnostics.is_empty() {
            issue.detail.clone()
        } else {
            issue.diagnostics.join("\n")
        };
        let mut summary = div()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(theme::red())
            .bg(theme::red_soft())
            .px_4()
            .py_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::red())
                    .child(remote_issue_title(issue.kind)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme::subtext())
                    .child(remote_issue_help(issue.kind)),
            );
        if let Some(exit_code) = issue.exit_code {
            summary = summary.child(div().flex().flex_row().items_center().gap_2().child(
                components::badge(
                    BadgeTone::Danger,
                    tf!(k::REMOTE_ISSUE_EXIT_CODE, code = exit_code),
                ),
            ));
        }
        let body = components::modal_body()
            .id("remote-issue-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(SharedString::from(node_name)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(SharedString::from(host.ssh_alias.clone())),
                    ),
            )
            .child(summary)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::subtext())
                            .child(t(k::REMOTE_ISSUE_RAW_DETAILS)),
                    )
                    .child(
                        div()
                            .id("remote-issue-diagnostics")
                            .max_h(px(220.))
                            .overflow_y_scroll()
                            .rounded_lg()
                            .border_1()
                            .border_color(theme::border())
                            .bg(theme::surface())
                            .px_3()
                            .py_3()
                            .font_family("Menlo")
                            .text_xs()
                            .text_color(theme::subtext())
                            .child(SharedString::from(diagnostics)),
                    ),
            );
        let mut actions = Vec::new();
        if issue.kind.can_bootstrap() {
            actions.push(
                components::button(
                    "remote-issue-bootstrap",
                    if issue.kind == RemoteConnectionIssueKind::CliNotInstalled {
                        t(k::REMOTE_ACTION_INSTALL_NODE)
                    } else {
                        t(k::REMOTE_ACTION_UPGRADE_NODE)
                    },
                    ButtonTone::Primary,
                    ButtonSize::Md,
                )
                .on_click(move |_event, window, cx| bootstrap(&(), window, cx))
                .into_any_element(),
            );
        }
        actions.push(
            components::button(
                "remote-issue-close",
                t(k::REMOTE_ACTION_CANCEL),
                ButtonTone::Ghost,
                ButtonSize::Md,
            )
            .on_click(move |_event, window, cx| close_footer(&(), window, cx))
            .into_any_element(),
        );
        let card = components::modal_card()
            .w(px(680.))
            .max_h(px(720.))
            .child(
                components::modal_header(t(k::REMOTE_ISSUE_DETAILS_TITLE)).child(
                    icon_only_button(
                        "remote-issue-close-icon",
                        t(k::REMOTE_ACTION_CANCEL),
                        IconName::Close,
                    )
                    .on_click(move |_event, window, cx| close_icon(&(), window, cx)),
                ),
            )
            .child(body)
            .child(components::modal_footer(actions));
        components::modal_overlay(card)
    }

    fn render_node_bootstrap_modal(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(dialog) = self.bootstrap_dialog.as_ref() else {
            return div();
        };
        let node_name = self.node_name(&dialog.host);
        let close_header =
            cx.listener(|this: &mut Self, _: &(), _window, cx| this.close_node_bootstrap(cx));
        let close_done =
            cx.listener(|this: &mut Self, _: &(), _window, cx| this.close_node_bootstrap(cx));
        let install =
            cx.listener(|this: &mut Self, _: &(), _window, cx| this.install_node_bootstrap(cx));
        let retry_id = dialog.host.id.clone();
        let retry = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
            this.open_node_bootstrap(retry_id.clone(), cx)
        });
        let (phase_label, phase_tone) = match dialog.phase {
            NodeBootstrapPhase::Detecting => {
                (t(k::REMOTE_BOOTSTRAP_PHASE_DETECTING), BadgeTone::Neutral)
            }
            NodeBootstrapPhase::Ready => (t(k::REMOTE_BOOTSTRAP_PHASE_READY), BadgeTone::Accent),
            NodeBootstrapPhase::Downloading => {
                (t(k::REMOTE_BOOTSTRAP_PHASE_DOWNLOADING), BadgeTone::Accent)
            }
            NodeBootstrapPhase::Installing => {
                (t(k::REMOTE_BOOTSTRAP_PHASE_INSTALLING), BadgeTone::Accent)
            }
            NodeBootstrapPhase::Reconnecting => {
                (t(k::REMOTE_BOOTSTRAP_PHASE_RECONNECTING), BadgeTone::Accent)
            }
            NodeBootstrapPhase::Complete => {
                (t(k::REMOTE_BOOTSTRAP_PHASE_COMPLETE), BadgeTone::Success)
            }
            NodeBootstrapPhase::Failed => (t(k::REMOTE_BOOTSTRAP_PHASE_FAILED), BadgeTone::Danger),
        };
        let title = if dialog.issue.kind == RemoteConnectionIssueKind::CliNotInstalled {
            t(k::REMOTE_BOOTSTRAP_TITLE_INSTALL)
        } else {
            t(k::REMOTE_BOOTSTRAP_TITLE_UPGRADE)
        };
        let mut body = components::modal_body()
            .id("remote-bootstrap-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child(SharedString::from(node_name)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(SharedString::from(dialog.host.ssh_alias.clone())),
                            ),
                    )
                    .child(components::badge(phase_tone, phase_label.clone())),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme::subtext())
                    .child(t(k::REMOTE_BOOTSTRAP_DESC)),
            );
        if let Some(prepared) = &dialog.prepared {
            let destination = prepared.probe.home.join(".local/bin/ochcli");
            let rows = vec![
                layout::row()
                    .child(layout::row_label(
                        t(k::REMOTE_BOOTSTRAP_PLATFORM),
                        SharedString::from(format!(
                            "{} · {}",
                            prepared.probe.os, prepared.probe.arch
                        )),
                    ))
                    .child(components::badge(
                        BadgeTone::Neutral,
                        SharedString::from(prepared.target.clone()),
                    ))
                    .into_any_element(),
                layout::row()
                    .child(layout::row_label(
                        t(k::REMOTE_BOOTSTRAP_CURRENT),
                        prepared
                            .probe
                            .existing_cli
                            .as_ref()
                            .map(|path| SharedString::from(path.display().to_string()))
                            .unwrap_or_else(|| t(k::REMOTE_BOOTSTRAP_NOT_INSTALLED)),
                    ))
                    .child(components::badge(
                        BadgeTone::Neutral,
                        prepared
                            .probe
                            .existing_version
                            .clone()
                            .map(SharedString::from)
                            .unwrap_or_else(|| t(k::REMOTE_BOOTSTRAP_NOT_INSTALLED)),
                    ))
                    .into_any_element(),
                layout::row()
                    .child(layout::row_label(
                        t(k::REMOTE_BOOTSTRAP_LATEST),
                        SharedString::from(format_update_size(prepared.entry.size)),
                    ))
                    .child(components::badge(
                        BadgeTone::Accent,
                        SharedString::from(format!("v{}", prepared.version)),
                    ))
                    .into_any_element(),
                layout::row()
                    .child(layout::row_label(
                        t(k::REMOTE_BOOTSTRAP_DESTINATION),
                        SharedString::from(destination.display().to_string()),
                    ))
                    .child(components::badge(BadgeTone::Success, "SSH"))
                    .into_any_element(),
            ];
            body = body.child(layout::group(rows));
        } else if dialog.phase == NodeBootstrapPhase::Detecting {
            body = body.child(
                div()
                    .min_h(px(150.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(theme::muted())
                    .child(t(k::REMOTE_BOOTSTRAP_PHASE_DETECTING)),
            );
        }
        if let Some(error) = &dialog.error {
            body = body.child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::red())
                    .bg(theme::red_soft())
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(theme::red())
                    .child(SharedString::from(error.clone())),
            );
        }
        if let Some(version) = &dialog.installed_version {
            body = body.child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::green())
                    .bg(theme::green_soft())
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(theme::green())
                    .child(tf!(k::REMOTE_BOOTSTRAP_SUCCESS, version = version)),
            );
        }
        let primary = match dialog.phase {
            NodeBootstrapPhase::Ready => components::button(
                "remote-bootstrap-install",
                if dialog.issue.kind == RemoteConnectionIssueKind::CliNotInstalled {
                    t(k::REMOTE_BOOTSTRAP_CONFIRM_INSTALL)
                } else {
                    t(k::REMOTE_BOOTSTRAP_CONFIRM_UPGRADE)
                },
                ButtonTone::Primary,
                ButtonSize::Md,
            )
            .on_click(move |_event, window, cx| install(&(), window, cx))
            .into_any_element(),
            NodeBootstrapPhase::Failed => components::button(
                "remote-bootstrap-retry",
                t(k::REMOTE_ACTION_RETRY_PROBE),
                ButtonTone::Primary,
                ButtonSize::Md,
            )
            .on_click(move |_event, window, cx| retry(&(), window, cx))
            .into_any_element(),
            NodeBootstrapPhase::Complete => components::button(
                "remote-bootstrap-done",
                t(k::REMOTE_UPDATE_DONE),
                ButtonTone::Primary,
                ButtonSize::Md,
            )
            .on_click(move |_event, window, cx| close_done(&(), window, cx))
            .into_any_element(),
            _ => components::disabled_button(
                "remote-bootstrap-working",
                phase_label,
                ButtonTone::Primary,
                ButtonSize::Md,
                true,
            )
            .into_any_element(),
        };
        let card = components::modal_card()
            .w(px(700.))
            .max_h(px(760.))
            .child(components::modal_header(title).child(if dialog.working() {
                icon_only_button(
                    "remote-bootstrap-close-disabled",
                    t(k::REMOTE_ACTION_CANCEL),
                    IconName::Close,
                )
                .opacity(0.35)
                .into_any_element()
            } else {
                icon_only_button(
                    "remote-bootstrap-close",
                    t(k::REMOTE_ACTION_CANCEL),
                    IconName::Close,
                )
                .on_click(move |_event, window, cx| close_header(&(), window, cx))
                .into_any_element()
            }))
            .child(body)
            .child(components::modal_footer(vec![primary]));
        components::modal_overlay(card)
    }

    fn render_node_update_modal(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let Some(dialog) = self.update_dialog.as_ref() else {
            return div();
        };
        let node_name = self.node_name(&dialog.host);
        let close = cx.listener(|this: &mut Self, _: &(), _window, cx| this.close_node_update(cx));
        let install =
            cx.listener(|this: &mut Self, _: &(), _window, cx| this.install_node_update(cx));
        let cancel =
            cx.listener(|this: &mut Self, _: &(), _window, cx| this.cancel_node_update(cx));
        let automatic = cx.listener(|this: &mut Self, _: &(), _window, cx| {
            this.select_node_update_strategy(NodeUpdateStrategy::Automatic, cx)
        });
        let direct = cx.listener(|this: &mut Self, _: &(), _window, cx| {
            this.select_node_update_strategy(NodeUpdateStrategy::Direct, cx)
        });
        let relay = cx.listener(|this: &mut Self, _: &(), _window, cx| {
            this.select_node_update_strategy(NodeUpdateStrategy::Relay, cx)
        });
        let direct_allowed = dialog.client.as_ref().is_some_and(|client| {
            client
                .handshake()
                .capabilities
                .contains(&Capability::NodeUpdateInstall)
        });
        let relay_allowed = dialog.client.as_ref().is_some_and(|client| {
            client
                .handshake()
                .capabilities
                .contains(&Capability::NodeUpdateRelay)
        });

        let (phase_label, phase_tone) = match dialog.phase {
            NodeUpdatePhase::Checking => (t(k::REMOTE_UPDATE_PHASE_CHECKING), BadgeTone::Neutral),
            NodeUpdatePhase::Ready => (t(k::REMOTE_UPDATE_PHASE_READY), BadgeTone::Accent),
            NodeUpdatePhase::Downloading => {
                (t(k::REMOTE_UPDATE_PHASE_DOWNLOADING), BadgeTone::Accent)
            }
            NodeUpdatePhase::Uploading => (t(k::REMOTE_UPDATE_PHASE_UPLOADING), BadgeTone::Accent),
            NodeUpdatePhase::Installing => {
                (t(k::REMOTE_UPDATE_PHASE_INSTALLING), BadgeTone::Accent)
            }
            NodeUpdatePhase::Reconnecting => {
                (t(k::REMOTE_UPDATE_PHASE_RECONNECTING), BadgeTone::Accent)
            }
            NodeUpdatePhase::Complete => (t(k::REMOTE_UPDATE_PHASE_COMPLETE), BadgeTone::Success),
            NodeUpdatePhase::Failed => (t(k::REMOTE_UPDATE_PHASE_FAILED), BadgeTone::Danger),
            NodeUpdatePhase::Cancelled => (t(k::REMOTE_UPDATE_PHASE_CANCELLED), BadgeTone::Neutral),
        };
        let mut body = components::modal_body()
            .id("remote-node-update-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child(SharedString::from(node_name)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(SharedString::from(dialog.host.ssh_alias.clone())),
                            ),
                    )
                    .child(components::badge(phase_tone, phase_label)),
            );
        if let Some(report) = &dialog.report {
            let relay_selected = dialog.strategy == NodeUpdateStrategy::Relay
                || (dialog.strategy == NodeUpdateStrategy::Automatic && relay_allowed);
            let route = if relay_selected {
                t(k::REMOTE_UPDATE_ROUTE_RELAY)
            } else {
                t(k::REMOTE_UPDATE_ROUTE_DIRECT)
            };
            let rows = vec![
                layout::row()
                    .child(layout::row_label(
                        t(k::REMOTE_UPDATE_CURRENT),
                        SharedString::from(report.update.current_version.clone()),
                    ))
                    .child(components::badge(
                        BadgeTone::Neutral,
                        SharedString::from(format!("v{}", report.update.current_version)),
                    ))
                    .into_any_element(),
                layout::row()
                    .child(layout::row_label(
                        t(k::REMOTE_UPDATE_LATEST),
                        SharedString::from(report.update.latest_version.clone()),
                    ))
                    .child(components::badge(
                        if report.update.has_update {
                            BadgeTone::Accent
                        } else {
                            BadgeTone::Success
                        },
                        if report.update.has_update {
                            t(k::REMOTE_UPDATE_AVAILABLE)
                        } else {
                            t(k::REMOTE_UPDATE_CURRENT_BADGE)
                        },
                    ))
                    .into_any_element(),
                layout::row()
                    .child(layout::row_label(
                        t(k::REMOTE_UPDATE_TARGET),
                        SharedString::from(report.update.target.clone()),
                    ))
                    .child(components::badge(
                        BadgeTone::Neutral,
                        report
                            .update
                            .payload_size
                            .map(format_update_size)
                            .unwrap_or_else(|| "—".to_string()),
                    ))
                    .into_any_element(),
                layout::row()
                    .child(layout::row_label(
                        t(k::REMOTE_UPDATE_INSTALLATION),
                        SharedString::from(report.installation.managed_root.display().to_string()),
                    ))
                    .child(components::badge(
                        if report.installation.managed {
                            BadgeTone::Success
                        } else {
                            BadgeTone::Warning
                        },
                        SharedString::from(report.installation.service_mode.clone()),
                    ))
                    .into_any_element(),
                layout::row()
                    .child(layout::row_label(
                        t(k::REMOTE_UPDATE_NETWORK),
                        if relay_selected {
                            route.clone()
                        } else {
                            report
                                .update
                                .direct_error
                                .clone()
                                .map(SharedString::from)
                                .unwrap_or_else(|| route.clone())
                        },
                    ))
                    .child(components::badge(
                        if !relay_selected && report.update.direct_download {
                            BadgeTone::Success
                        } else {
                            BadgeTone::Neutral
                        },
                        route,
                    ))
                    .into_any_element(),
            ];
            body = body.child(layout::group(rows)).child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::subtext())
                            .child(t(k::REMOTE_UPDATE_STRATEGY)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                components::button(
                                    "remote-update-strategy-auto",
                                    t(k::REMOTE_UPDATE_STRATEGY_AUTO),
                                    if dialog.strategy == NodeUpdateStrategy::Automatic {
                                        ButtonTone::Primary
                                    } else {
                                        ButtonTone::Neutral
                                    },
                                    ButtonSize::Sm,
                                )
                                .on_click(move |_event, window, cx| automatic(&(), window, cx)),
                            )
                            .child(if report.update.direct_download && direct_allowed {
                                components::button(
                                    "remote-update-strategy-direct",
                                    t(k::REMOTE_UPDATE_STRATEGY_DIRECT),
                                    if dialog.strategy == NodeUpdateStrategy::Direct {
                                        ButtonTone::Primary
                                    } else {
                                        ButtonTone::Neutral
                                    },
                                    ButtonSize::Sm,
                                )
                                .on_click(move |_event, window, cx| direct(&(), window, cx))
                                .into_any_element()
                            } else {
                                components::disabled_button(
                                    "remote-update-strategy-direct-disabled",
                                    t(k::REMOTE_UPDATE_STRATEGY_DIRECT),
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                    true,
                                )
                                .into_any_element()
                            })
                            .child(if relay_allowed {
                                components::button(
                                    "remote-update-strategy-relay",
                                    t(k::REMOTE_UPDATE_STRATEGY_RELAY),
                                    if dialog.strategy == NodeUpdateStrategy::Relay {
                                        ButtonTone::Primary
                                    } else {
                                        ButtonTone::Neutral
                                    },
                                    ButtonSize::Sm,
                                )
                                .on_click(move |_event, window, cx| relay(&(), window, cx))
                                .into_any_element()
                            } else {
                                components::disabled_button(
                                    "remote-update-strategy-relay-disabled",
                                    t(k::REMOTE_UPDATE_STRATEGY_RELAY),
                                    ButtonTone::Neutral,
                                    ButtonSize::Sm,
                                    true,
                                )
                                .into_any_element()
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(t(k::REMOTE_UPDATE_STRATEGY_DESC)),
                    ),
            );
            if report.update.has_update
                && (!report.installation.can_self_update || !report.update.signed)
            {
                body = body.child(
                    div()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme::yellow())
                        .bg(theme::yellow_soft())
                        .px_4()
                        .py_3()
                        .text_sm()
                        .text_color(theme::yellow())
                        .child(t(k::REMOTE_UPDATE_UNSIGNED)),
                );
            } else if report.update.has_update && !direct_allowed && !relay_allowed {
                body = body.child(
                    div()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme::yellow())
                        .bg(theme::yellow_soft())
                        .px_4()
                        .py_3()
                        .text_sm()
                        .text_color(theme::yellow())
                        .child(t(k::REMOTE_UPDATE_POLICY_REQUIRED)),
                );
            }
        } else if dialog.phase == NodeUpdatePhase::Checking {
            body = body.child(
                div()
                    .min_h(px(180.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(theme::muted())
                    .child(t(k::REMOTE_UPDATE_CHECKING_DESC)),
            );
        }
        if let Some(error) = &dialog.error {
            body = body.child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::red())
                    .bg(theme::red_soft())
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(theme::red())
                    .child(SharedString::from(error.clone())),
            );
        }
        if matches!(
            dialog.phase,
            NodeUpdatePhase::Downloading | NodeUpdatePhase::Uploading | NodeUpdatePhase::Installing
        ) {
            let progress = dialog.progress.map(|(done, total)| {
                let fraction = if total == 0 {
                    0.0
                } else {
                    (done as f32 / total as f32).clamp(0.0, 1.0)
                };
                (done, total, fraction)
            });
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_lg()
                    .bg(theme::surface_hover())
                    .px_4()
                    .py_3()
                    .child(div().text_xs().text_color(theme::subtext()).child(
                        if let Some((done, total, fraction)) = progress {
                            SharedString::from(tf!(
                                k::REMOTE_UPDATE_PROGRESS,
                                done = format_update_size(done),
                                total = format_update_size(total),
                                percent = format!("{:.0}", fraction * 100.0)
                            ))
                        } else {
                            t(k::REMOTE_UPDATE_PROGRESS_WAITING)
                        },
                    ))
                    .child(
                        div()
                            .w_full()
                            .h(px(6.))
                            .rounded_full()
                            .bg(theme::inset())
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(progress.map_or(0.08, |(_, _, fraction)| fraction)))
                                    .rounded_full()
                                    .bg(theme::accent()),
                            ),
                    ),
            );
        }
        if let Some(result) = &dialog.result {
            body = body.child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::green())
                    .bg(theme::green_soft())
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(theme::green())
                    .child(tf!(
                        k::REMOTE_UPDATE_RESULT,
                        from = result.from_version,
                        version = result.version,
                        strategy = result.strategy
                    )),
            );
        }

        let selected_strategy_allowed =
            dialog
                .report
                .as_ref()
                .is_some_and(|report| match dialog.strategy {
                    NodeUpdateStrategy::Automatic if relay_allowed => true,
                    NodeUpdateStrategy::Automatic => {
                        direct_allowed && report.update.direct_download
                    }
                    NodeUpdateStrategy::Direct => direct_allowed && report.update.direct_download,
                    NodeUpdateStrategy::Relay => relay_allowed,
                });
        let can_install = dialog.phase == NodeUpdatePhase::Ready
            && selected_strategy_allowed
            && dialog.report.as_ref().is_some_and(|report| {
                report.update.has_update
                    && report.update.signed
                    && report.installation.can_self_update
            });
        let primary = if dialog.cancellable() {
            components::button(
                "remote-node-update-cancel",
                t(k::REMOTE_UPDATE_CANCEL),
                ButtonTone::Neutral,
                ButtonSize::Md,
            )
            .on_click(move |_event, window, cx| cancel(&(), window, cx))
            .into_any_element()
        } else if can_install {
            components::button(
                "remote-node-update-install",
                t(k::REMOTE_UPDATE_INSTALL),
                ButtonTone::Primary,
                ButtonSize::Md,
            )
            .on_click(move |_event, window, cx| install(&(), window, cx))
            .into_any_element()
        } else {
            components::disabled_button(
                "remote-node-update-install-disabled",
                if matches!(
                    dialog.phase,
                    NodeUpdatePhase::Complete | NodeUpdatePhase::Cancelled
                ) {
                    t(k::REMOTE_UPDATE_DONE)
                } else if dialog
                    .report
                    .as_ref()
                    .is_some_and(|report| !report.update.has_update)
                {
                    t(k::REMOTE_UPDATE_CURRENT_BADGE)
                } else {
                    t(k::REMOTE_UPDATE_INSTALL)
                },
                ButtonTone::Primary,
                ButtonSize::Md,
                true,
            )
            .into_any_element()
        };
        let max_height = (window.viewport_size().height - px(32.)).max(px(320.));
        let card = components::modal_card()
            .w(px(720.))
            .max_w_full()
            .max_h(max_height)
            .child(
                components::modal_header(t(k::REMOTE_UPDATE_TITLE)).child(if dialog.working() {
                    icon_only_button(
                        "remote-node-update-close-disabled",
                        t(k::REMOTE_ACTION_CANCEL),
                        IconName::Close,
                    )
                    .opacity(0.35)
                    .into_any_element()
                } else {
                    icon_only_button(
                        "remote-node-update-close",
                        t(k::REMOTE_ACTION_CANCEL),
                        IconName::Close,
                    )
                    .on_click(move |_event, window, cx| close(&(), window, cx))
                    .into_any_element()
                }),
            )
            .child(body)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(primary),
            );
        components::modal_overlay(card)
    }

    fn modal_header(&self, title: SharedString, cx: &mut Context<Self>) -> gpui::Div {
        components::modal_header(title).child(
            icon_only_button(
                "remote-add-close",
                t(k::REMOTE_ACTION_CANCEL),
                IconName::Close,
            )
            .on_click(cx.listener(|this, _event, _window, cx| this.cancel_add(cx))),
        )
    }

    fn render_ssh_config_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let manual = cx.listener(|this: &mut Self, _: &(), _window, cx| this.show_manual_add(cx));
        let cancel = cx.listener(|this: &mut Self, _: &(), _window, cx| this.cancel_add(cx));
        let add =
            cx.listener(|this: &mut Self, _: &(), _window, cx| this.scan_selected_ssh_config(cx));
        let mut list = div()
            .id("remote-ssh-config-list")
            .role(gpui::Role::List)
            .flex()
            .flex_col()
            .w_full()
            .max_h(px(380.))
            .overflow_y_scroll()
            .rounded_lg()
            .border_1()
            .border_color(theme::border())
            .bg(theme::surface())
            .occlude();
        for (index, entry) in self.ssh_config_entries.iter().enumerate() {
            let selected = self.selected_ssh_config == Some(index);
            let already_added = self.has_ssh_alias(&entry.alias);
            let select = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
                this.select_ssh_config(index, cx)
            });
            let target = match entry.user.as_deref() {
                Some(user) => format!("{user}@{}:{}", entry.hostname, entry.port),
                None => format!("{}:{}", entry.hostname, entry.port),
            };
            let identity = entry
                .identity_file
                .as_ref()
                .and_then(|path| {
                    std::path::Path::new(path)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .or_else(|| {
                    entry
                        .source
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                });
            let marker = if already_added {
                components::badge(BadgeTone::Neutral, t(k::REMOTE_ADD_CONFIG_ALREADY))
                    .into_any_element()
            } else {
                div()
                    .w(px(22.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .border_1()
                    .border_color(if selected {
                        theme::accent()
                    } else {
                        theme::border_strong()
                    })
                    .bg(if selected {
                        theme::accent()
                    } else {
                        theme::surface()
                    })
                    .when(selected, |box_| {
                        box_.child(icon(IconName::Check, theme::surface(), 13.))
                    })
                    .into_any_element()
            };
            list = list.child(
                div()
                    .id(SharedString::from(format!("ssh-config-entry-{index}")))
                    .role(gpui::Role::ListBoxOption)
                    .aria_label(SharedString::from(format!("{} · {target}", entry.alias)))
                    .aria_selected(selected)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .min_h(px(72.))
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(theme::border())
                    .when(!already_added, |row| {
                        row.cursor_pointer()
                            .hover(|hover| hover.bg(theme::surface_hover()))
                            .on_click(move |_event, window, cx| select(&(), window, cx))
                    })
                    .when(selected, |row| row.bg(theme::accent_soft()))
                    .when(already_added, |row| row.opacity(0.62))
                    .child(icon(IconName::Desktop, theme::subtext(), 19.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child(SharedString::from(entry.alias.clone())),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(div().truncate().child(SharedString::from(target)))
                                    .when_some(identity, |row, identity| {
                                        row.child(SharedString::from(format!("· {identity}")))
                                    }),
                            ),
                    )
                    .child(marker),
            );
        }
        if self.ssh_config_entries.is_empty() {
            list = list.child(
                div()
                    .min_h(px(180.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .px_8()
                    .text_center()
                    .text_sm()
                    .text_color(theme::muted())
                    .child(t(k::REMOTE_ADD_CONFIG_EMPTY)),
            );
        }
        let selected_available = self
            .selected_ssh_config
            .and_then(|index| self.ssh_config_entries.get(index))
            .is_some_and(|entry| !self.has_ssh_alias(&entry.alias));
        components::modal_card()
            .w(px(680.))
            .max_h(px(680.))
            .child(self.modal_header(t(k::REMOTE_ADD_CONFIG_TITLE), cx))
            .child(
                components::modal_body()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::subtext())
                            .child(t(k::REMOTE_ADD_CONFIG_DESC)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::muted())
                            .child(tf!(
                                k::REMOTE_ADD_CONFIG_FOUND,
                                count = self.ssh_config_entries.len()
                            )),
                    )
                    .when_some(self.ssh_config_error.clone(), |body, error| {
                        body.child(div().text_xs().text_color(theme::red()).child(error))
                    })
                    .child(list),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(
                        components::button(
                            "remote-add-manual",
                            t(k::REMOTE_ACTION_MANUAL),
                            ButtonTone::Ghost,
                            ButtonSize::Md,
                        )
                        .on_click(move |_event, window, cx| manual(&(), window, cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                components::button(
                                    "remote-add-cancel",
                                    t(k::REMOTE_ACTION_CANCEL),
                                    ButtonTone::Neutral,
                                    ButtonSize::Md,
                                )
                                .on_click(move |_event, window, cx| cancel(&(), window, cx)),
                            )
                            .child(if self.busy || !selected_available {
                                components::disabled_button(
                                    "remote-config-add-disabled",
                                    t(k::REMOTE_ACTION_ADD),
                                    ButtonTone::Primary,
                                    ButtonSize::Md,
                                    self.busy,
                                )
                                .into_any_element()
                            } else {
                                components::button(
                                    "remote-config-add",
                                    t(k::REMOTE_ACTION_ADD),
                                    ButtonTone::Primary,
                                    ButtonSize::Md,
                                )
                                .on_click(move |_event, window, cx| add(&(), window, cx))
                                .into_any_element()
                            }),
                    ),
            )
    }

    fn render_manual_add_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let scan = cx.listener(|this: &mut Self, _: &(), _window, cx| this.scan(cx));
        let back = cx.listener(|this: &mut Self, _: &(), _window, cx| this.show_ssh_config_add(cx));
        let fields = div()
            .flex()
            .flex_col()
            .gap_4()
            .child(components::field(
                t(k::REMOTE_FIELD_TARGET),
                true,
                Some(t(k::REMOTE_FIELD_TARGET_HELP)),
                self.target_input.clone(),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(
                        components::field(
                            t(k::REMOTE_FIELD_HOSTNAME),
                            true,
                            Some(t(k::REMOTE_FIELD_HOSTNAME_HELP)),
                            self.hostname_input.clone(),
                        )
                        .flex_1(),
                    )
                    .child(
                        components::field(
                            t(k::REMOTE_FIELD_PORT),
                            true,
                            None,
                            self.port_input.clone(),
                        )
                        .w(px(130.)),
                    ),
            )
            .child(components::field(
                t(k::REMOTE_FIELD_CLI),
                true,
                Some(t(k::REMOTE_FIELD_CLI_HELP)),
                self.cli_input.clone(),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(icon(IconName::Key, theme::muted(), 14.))
                    .child(t(k::REMOTE_SECURITY_NOTE)),
            );
        components::modal_card()
            .w(px(680.))
            .max_h(px(720.))
            .child(self.modal_header(t(k::REMOTE_ADD_TITLE), cx))
            .child(
                components::modal_body()
                    .id("remote-manual-add-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::subtext())
                            .child(t(k::REMOTE_ADD_DESC)),
                    )
                    .child(fields),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(
                        components::button(
                            "remote-manual-back",
                            t(k::REMOTE_ACTION_BACK),
                            ButtonTone::Ghost,
                            ButtonSize::Md,
                        )
                        .on_click(move |_event, window, cx| back(&(), window, cx)),
                    )
                    .child(if self.busy {
                        components::disabled_button(
                            "remote-scan-disabled",
                            t(k::REMOTE_ACTION_SCAN),
                            ButtonTone::Primary,
                            ButtonSize::Md,
                            true,
                        )
                        .into_any_element()
                    } else {
                        components::button(
                            "remote-scan",
                            t(k::REMOTE_ACTION_SCAN),
                            ButtonTone::Primary,
                            ButtonSize::Md,
                        )
                        .on_click(move |_event, window, cx| scan(&(), window, cx))
                        .into_any_element()
                    }),
            )
    }

    fn render_fingerprint_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let back =
            cx.listener(|this: &mut Self, _: &(), _window, cx| this.back_from_fingerprint(cx));
        let trust = cx.listener(|this: &mut Self, _: &(), _window, cx| this.trust_and_connect(cx));
        let rows = self
            .scanned_keys
            .iter()
            .map(|key| {
                layout::row()
                    .child(layout::row_label(
                        SharedString::from(key.key_type.clone()),
                        SharedString::from(key.fingerprint.clone()),
                    ))
                    .child(components::badge(BadgeTone::Warning, "SSH"))
                    .into_any_element()
            })
            .collect();
        components::modal_card()
            .w(px(680.))
            .child(self.modal_header(t(k::REMOTE_FINGERPRINT_TITLE), cx))
            .child(
                components::modal_body()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::subtext())
                            .child(t(k::REMOTE_FINGERPRINT_DESC)),
                    )
                    .child(layout::group(rows)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(
                        components::button(
                            "remote-fingerprint-back",
                            t(k::REMOTE_ACTION_BACK),
                            ButtonTone::Ghost,
                            ButtonSize::Md,
                        )
                        .on_click(move |_event, window, cx| back(&(), window, cx)),
                    )
                    .child(
                        components::button(
                            "remote-trust-connect",
                            t(k::REMOTE_ACTION_TRUST_CONNECT),
                            ButtonTone::Primary,
                            ButtonSize::Md,
                        )
                        .on_click(move |_event, window, cx| trust(&(), window, cx)),
                    ),
            )
    }

    fn render_connection_list(&self, cx: &mut Context<Self>) -> gpui::Div {
        let retry = cx.listener(|this: &mut Self, _: &(), _window, cx| this.probe_nodes(cx));
        let rows = self
            .store
            .hosts()
            .iter()
            .map(|host| {
                let id = host.id.clone();
                let node_name = self.node_name(host);
                let connected = self.selected_id.as_deref() == Some(host.id.as_str())
                    && self.connection_state == ConnectionState::Connected;
                let probe = self.node_probes.get(&host.id);
                let (state, state_label, state_color) =
                    if connected || probe.is_some_and(|probe| probe.state == ProbeState::Online) {
                        (
                            ProbeState::Online,
                            t(k::REMOTE_CONNECTION_ONLINE),
                            theme::green(),
                        )
                    } else if probe.is_some_and(|probe| probe.state == ProbeState::Checking) {
                        (
                            ProbeState::Checking,
                            t(k::REMOTE_CONNECTION_CHECKING),
                            theme::yellow(),
                        )
                    } else {
                        (
                            ProbeState::Offline,
                            t(k::REMOTE_CONNECTION_OFFLINE),
                            theme::muted(),
                        )
                    };
                let version = if connected {
                    self.client
                        .as_ref()
                        .map(|client| client.handshake().server_version.clone())
                } else {
                    probe.and_then(|probe| probe.version.clone())
                };
                let platform = probe
                    .and_then(|probe| probe.platform.clone())
                    .or_else(|| {
                        connected.then(|| {
                            self.client
                                .as_ref()
                                .map(|client| {
                                    let node = &client.handshake().node;
                                    format!("{} · {}", node.os, node.arch)
                                })
                                .unwrap_or_default()
                        })
                    })
                    .filter(|value| !value.is_empty());
                let issue = (state == ProbeState::Offline)
                    .then(|| probe.and_then(|probe| probe.issue.clone()))
                    .flatten();
                let issue_label = issue.as_ref().map(|issue| remote_issue_title(issue.kind));
                let can_bootstrap = issue
                    .as_ref()
                    .is_some_and(|issue| issue.kind.can_bootstrap());
                let quick_update_supported = if connected {
                    self.client.as_ref().is_some_and(|client| {
                        client
                            .handshake()
                            .capabilities
                            .contains(&Capability::NodeUpdateRead)
                    })
                } else {
                    probe.is_some_and(|probe| probe.quick_update_supported)
                };
                let update_id = id.clone();
                let bootstrap_id = id.clone();
                let details_id = id.clone();
                let manage_id = id.clone();
                let remove_id = id.clone();
                let manage = cx.listener(move |_this: &mut Self, _: &(), _window, cx| {
                    cx.emit(RemoteEvent::ManageRequested {
                        id: manage_id.clone(),
                    })
                });
                let update = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
                    this.open_node_update(update_id.clone(), cx)
                });
                let bootstrap = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
                    this.open_node_bootstrap(bootstrap_id.clone(), cx)
                });
                let details = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
                    this.open_issue_details(details_id.clone(), cx)
                });
                let remove = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
                    this.remove_host(remove_id.clone(), cx)
                });
                div()
                    .id(SharedString::from(format!("remote-connection-{}", host.id)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .min_h(px(82.))
                    .px_4()
                    .py_3()
                    .child(icon(IconName::Globe, theme::accent(), 18.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .truncate()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme::text())
                                            .child(SharedString::from(node_name)),
                                    )
                                    .when(connected, |row| {
                                        row.child(components::badge(
                                            BadgeTone::Accent,
                                            t(k::REMOTE_CONNECTION_CURRENT),
                                        ))
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(
                                        div()
                                            .truncate()
                                            .child(SharedString::from(host.ssh_alias.clone())),
                                    )
                                    .when_some(platform, |row, platform| {
                                        row.child(SharedString::from(format!("· {platform}")))
                                    }),
                            )
                            .when_some(issue_label, |column, issue_label| {
                                column.child(
                                    div()
                                        .line_clamp(1)
                                        .text_xs()
                                        .text_color(theme::red())
                                        .child(issue_label),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(components::status_dot_sized(state_color, 7.))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::subtext())
                                    .child(state_label),
                            ),
                    )
                    .child(components::badge(
                        if version.is_some() {
                            BadgeTone::Neutral
                        } else {
                            BadgeTone::Warning
                        },
                        version
                            .map(|version| tf!(k::REMOTE_NODE_VERSION, version = version))
                            .unwrap_or_else(|| t(k::REMOTE_CONNECTION_UNKNOWN_VERSION).to_string()),
                    ))
                    .child(if state == ProbeState::Online && quick_update_supported {
                        components::button(
                            SharedString::from(format!("remote-update-{}", host.id)),
                            t(k::REMOTE_UPDATE_ACTION),
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                        )
                        .on_click(move |_event, window, cx| update(&(), window, cx))
                        .into_any_element()
                    } else if can_bootstrap {
                        components::button(
                            SharedString::from(format!("remote-bootstrap-{}", host.id)),
                            if issue.as_ref().is_some_and(|issue| {
                                issue.kind == RemoteConnectionIssueKind::CliNotInstalled
                            }) {
                                t(k::REMOTE_ACTION_INSTALL_NODE)
                            } else {
                                t(k::REMOTE_ACTION_UPGRADE_NODE)
                            },
                            ButtonTone::Primary,
                            ButtonSize::Sm,
                        )
                        .on_click(move |_event, window, cx| bootstrap(&(), window, cx))
                        .into_any_element()
                    } else {
                        components::disabled_button(
                            SharedString::from(format!("remote-update-disabled-{}", host.id)),
                            if state == ProbeState::Online {
                                t(k::REMOTE_UPDATE_LEGACY_SHORT)
                            } else {
                                t(k::REMOTE_UPDATE_ACTION)
                            },
                            ButtonTone::Neutral,
                            ButtonSize::Sm,
                            true,
                        )
                        .into_any_element()
                    })
                    .when(issue.is_some(), |row| {
                        row.child(
                            components::button(
                                SharedString::from(format!("remote-details-{}", host.id)),
                                t(k::REMOTE_ACTION_DETAILS),
                                ButtonTone::Ghost,
                                ButtonSize::Sm,
                            )
                            .on_click(move |_event, window, cx| details(&(), window, cx)),
                        )
                    })
                    .child(
                        components::button(
                            SharedString::from(format!("remote-manage-{}", host.id)),
                            t(k::REMOTE_ACTION_MANAGE),
                            ButtonTone::Ghost,
                            ButtonSize::Sm,
                        )
                        .on_click(move |_event, window, cx| manage(&(), window, cx)),
                    )
                    .child(
                        icon_only_button(
                            SharedString::from(format!("remote-remove-{}", host.id)),
                            t(k::REMOTE_ACTION_REMOVE),
                            IconName::Trash,
                        )
                        .on_click(move |_event, window, cx| remove(&(), window, cx)),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .child(
                layout::section_header(
                    t(k::REMOTE_CONNECTION_TITLE),
                    tf!(k::REMOTE_CONNECTION_COUNT, count = self.store.hosts().len()),
                )
                .child(if self.probing {
                    components::disabled_button(
                        "remote-probe-disabled",
                        t(k::REMOTE_CONNECTION_CHECKING),
                        ButtonTone::Ghost,
                        ButtonSize::Sm,
                        true,
                    )
                    .into_any_element()
                } else {
                    components::button(
                        "remote-probe",
                        t(k::REMOTE_ACTION_RETRY_PROBE),
                        ButtonTone::Ghost,
                        ButtonSize::Sm,
                    )
                    .on_click(move |_event, window, cx| retry(&(), window, cx))
                    .into_any_element()
                }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(t(k::REMOTE_CONNECTION_DESC)),
            )
            .child(layout::group(rows))
    }
}

async fn load_snapshot(backend: WorkspaceBackend) -> Result<RemoteSnapshot, String> {
    let apps = backend
        .list_apps()
        .await
        .map_err(|error| error.to_string())?;
    Ok(RemoteSnapshot { apps })
}

impl gpui::Render for RemoteView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let add = cx.listener(|this: &mut Self, _: &(), _window, cx| this.open_add(cx));
        let action = if self.add_open {
            components::disabled_button(
                "remote-add-open",
                t(k::REMOTE_ACTION_ADD),
                ButtonTone::Primary,
                ButtonSize::Md,
                true,
            )
            .into_any_element()
        } else {
            components::button(
                "remote-add",
                t(k::REMOTE_ACTION_ADD),
                ButtonTone::Primary,
                ButtonSize::Md,
            )
            .on_click(move |_event, window, cx| add(&(), window, cx))
            .into_any_element()
        };
        let content = if self.store.hosts().is_empty() {
            layout::content_column().child(components::empty_state(
                IconName::Desktop,
                t(k::REMOTE_EMPTY_TITLE),
                t(k::REMOTE_EMPTY_DESC),
                None,
            ))
        } else {
            layout::wide_column().child(self.render_connection_list(cx))
        };
        let mut page = layout::page()
            .child(
                layout::page_header(t(k::REMOTE_PAGE_TITLE), Some(t(k::REMOTE_PAGE_DESC)))
                    .child(action),
            )
            .child(layout::scroll_body(
                "remote-nodes-body",
                &self.scroll,
                content,
            ));
        if self.add_open {
            page = page.child(self.render_add_modal(cx));
        }
        if self.update_dialog.is_some() {
            page = page.child(self.render_node_update_modal(window, cx));
        }
        if self.issue_dialog.is_some() {
            page = page.child(self.render_issue_details_modal(cx));
        }
        if self.bootstrap_dialog.is_some() {
            page = page.child(self.render_node_bootstrap_modal(cx));
        }
        page
    }
}

fn format_update_size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn remote_issue_title(kind: RemoteConnectionIssueKind) -> SharedString {
    match kind {
        RemoteConnectionIssueKind::CliNotInstalled => t(k::REMOTE_ISSUE_CLI_NOT_INSTALLED),
        RemoteConnectionIssueKind::NodeUpgradeRequired => t(k::REMOTE_ISSUE_NODE_UPGRADE_REQUIRED),
        RemoteConnectionIssueKind::DesktopUpgradeRequired => {
            t(k::REMOTE_ISSUE_DESKTOP_UPGRADE_REQUIRED)
        }
        RemoteConnectionIssueKind::RemoteDisabled => t(k::REMOTE_ISSUE_REMOTE_DISABLED),
        RemoteConnectionIssueKind::AuthenticationFailed => t(k::REMOTE_ISSUE_AUTHENTICATION_FAILED),
        RemoteConnectionIssueKind::HostKeyChanged => t(k::REMOTE_ISSUE_HOST_KEY_CHANGED),
        RemoteConnectionIssueKind::HostKeyUnknown => t(k::REMOTE_ISSUE_HOST_KEY_UNKNOWN),
        RemoteConnectionIssueKind::ConnectionRefused => t(k::REMOTE_ISSUE_CONNECTION_REFUSED),
        RemoteConnectionIssueKind::ConnectionTimedOut => t(k::REMOTE_ISSUE_CONNECTION_TIMED_OUT),
        RemoteConnectionIssueKind::NetworkUnreachable => t(k::REMOTE_ISSUE_NETWORK_UNREACHABLE),
        RemoteConnectionIssueKind::CliNotExecutable => t(k::REMOTE_ISSUE_CLI_NOT_EXECUTABLE),
        RemoteConnectionIssueKind::ArchitectureMismatch => t(k::REMOTE_ISSUE_ARCHITECTURE_MISMATCH),
        RemoteConnectionIssueKind::SystemIncompatible => t(k::REMOTE_ISSUE_SYSTEM_INCOMPATIBLE),
        RemoteConnectionIssueKind::ProtocolCorrupted => t(k::REMOTE_ISSUE_PROTOCOL_CORRUPTED),
        RemoteConnectionIssueKind::Unknown => t(k::REMOTE_ISSUE_UNKNOWN),
    }
}

fn remote_issue_help(kind: RemoteConnectionIssueKind) -> SharedString {
    match kind {
        RemoteConnectionIssueKind::CliNotInstalled => t(k::REMOTE_ISSUE_CLI_NOT_INSTALLED_HELP),
        RemoteConnectionIssueKind::NodeUpgradeRequired => {
            t(k::REMOTE_ISSUE_NODE_UPGRADE_REQUIRED_HELP)
        }
        RemoteConnectionIssueKind::DesktopUpgradeRequired => {
            t(k::REMOTE_ISSUE_DESKTOP_UPGRADE_REQUIRED_HELP)
        }
        RemoteConnectionIssueKind::RemoteDisabled => t(k::REMOTE_ISSUE_REMOTE_DISABLED_HELP),
        RemoteConnectionIssueKind::AuthenticationFailed => {
            t(k::REMOTE_ISSUE_AUTHENTICATION_FAILED_HELP)
        }
        RemoteConnectionIssueKind::HostKeyChanged => t(k::REMOTE_ISSUE_HOST_KEY_CHANGED_HELP),
        RemoteConnectionIssueKind::HostKeyUnknown => t(k::REMOTE_ISSUE_HOST_KEY_UNKNOWN_HELP),
        RemoteConnectionIssueKind::ConnectionRefused => t(k::REMOTE_ISSUE_CONNECTION_REFUSED_HELP),
        RemoteConnectionIssueKind::ConnectionTimedOut => {
            t(k::REMOTE_ISSUE_CONNECTION_TIMED_OUT_HELP)
        }
        RemoteConnectionIssueKind::NetworkUnreachable => {
            t(k::REMOTE_ISSUE_NETWORK_UNREACHABLE_HELP)
        }
        RemoteConnectionIssueKind::CliNotExecutable => t(k::REMOTE_ISSUE_CLI_NOT_EXECUTABLE_HELP),
        RemoteConnectionIssueKind::ArchitectureMismatch => {
            t(k::REMOTE_ISSUE_ARCHITECTURE_MISMATCH_HELP)
        }
        RemoteConnectionIssueKind::SystemIncompatible => {
            t(k::REMOTE_ISSUE_SYSTEM_INCOMPATIBLE_HELP)
        }
        RemoteConnectionIssueKind::ProtocolCorrupted => t(k::REMOTE_ISSUE_PROTOCOL_CORRUPTED_HELP),
        RemoteConnectionIssueKind::Unknown => t(k::REMOTE_ISSUE_UNKNOWN_HELP),
    }
}

fn localized_remote_error(issue: &RemoteConnectionIssue) -> String {
    let mut message = format!(
        "{}\n{}",
        remote_issue_title(issue.kind),
        remote_issue_help(issue.kind)
    );
    if !issue.detail.trim().is_empty() {
        message.push_str("\n\n");
        message.push_str(issue.detail.trim());
    }
    message
}

fn fallback_node_update_report(
    installation: NodeInstallStatus,
    manifest: ochub_core::services::update::headless::HeadlessUpdateManifest,
    os: &str,
    arch: &str,
    remote_error: String,
) -> Result<NodeUpdateReport, String> {
    let (target, entry) = manifest.entry_for(os, arch).ok_or_else(|| {
        format!(
            "release {} has no node executable for {os}-{arch}",
            manifest.version
        )
    })?;
    let target = target.to_string();
    let payload_size = entry.size;
    let signed = !entry.signature.trim().is_empty();
    let current_version = installation.current_version.clone();
    Ok(NodeUpdateReport {
        installation,
        update: ochub_core::services::update::headless::HeadlessUpdateCheck {
            current_version: current_version.clone(),
            latest_version: manifest.version.clone(),
            has_update: ochub_core::services::update::is_newer_version(
                &current_version,
                &manifest.version,
            ),
            target,
            release_url: ochub_core::services::update::headless::release_url(
                None,
                &manifest.version,
            ),
            notes: manifest.notes,
            published_at: manifest.pub_date,
            payload_size: Some(payload_size),
            signed,
            direct_download: false,
            direct_error: Some(remote_error),
        },
    })
}

#[cfg(test)]
mod node_update_tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use ochub_core::services::update::headless::{HeadlessPlatformEntry, HeadlessUpdateManifest};

    use super::*;

    #[test]
    fn automatic_updates_prefer_the_observable_relay_route() {
        assert_eq!(
            resolve_node_update_strategy(NodeUpdateStrategy::Automatic, true, true),
            NodeUpdateStrategy::Relay
        );
        assert_eq!(
            resolve_node_update_strategy(NodeUpdateStrategy::Automatic, true, false),
            NodeUpdateStrategy::Direct
        );
        assert_eq!(
            resolve_node_update_strategy(NodeUpdateStrategy::Direct, true, true),
            NodeUpdateStrategy::Direct
        );
    }

    #[test]
    fn desktop_manifest_fallback_keeps_an_offline_node_relay_updatable() {
        let installation = NodeInstallStatus {
            managed: true,
            current_version: "1.0.0".to_string(),
            active_version: Some("1.0.0".to_string()),
            previous_version: None,
            target: Some("linux-x86_64".to_string()),
            managed_root: PathBuf::from("/home/test/.local/share/ochub/cli"),
            executable: PathBuf::from("/home/test/.local/share/ochub/cli/current/ochcli"),
            command_link: PathBuf::from("/home/test/.local/bin/ochcli"),
            service_mode: "systemd-user".to_string(),
            service_definition: None,
            daemon: serde_json::json!({ "running": true }),
            can_self_update: true,
        };
        let manifest = HeadlessUpdateManifest {
            version: "1.1.0".to_string(),
            notes: Some("release".to_string()),
            pub_date: None,
            protocol_min: 1,
            protocol_max: 2,
            targets: BTreeMap::from([(
                "linux-x86_64".to_string(),
                HeadlessPlatformEntry {
                    url: "https://github.com/OcHub-team/OcHub/releases/download/v1.1.0/ochcli"
                        .to_string(),
                    signature: "signed".to_string(),
                    sha256: "a".repeat(64),
                    size: 42,
                },
            )]),
        };

        let report = fallback_node_update_report(
            installation,
            manifest,
            "linux",
            "x86_64",
            "node cannot reach GitHub".to_string(),
        )
        .unwrap();

        assert!(report.update.has_update);
        assert!(report.update.signed);
        assert!(!report.update.direct_download);
        assert_eq!(
            report.update.direct_error.as_deref(),
            Some("node cannot reach GitHub")
        );
        assert_eq!(report.update.target, "linux-x86_64");
    }
}

crate::notifications::impl_status_toasts_leveled!(RemoteView);
