//! Operator-facing control plane for headless OcHub nodes.
//!
//! The transport stays deliberately separate from GPUI: this view owns only
//! connection records, explicit host-key confirmation and presentation state.
//! Every remote operation goes through [`WorkspaceBackend`], so local and
//! remote behavior share the same typed application boundary.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt as _;
use gpui::{
    Context, Entity, FontWeight, MouseButton, ScrollHandle, SharedString, Window, div, prelude::*,
    px,
};
use ochub_core::AppId;
use ochub_core::application::{
    AppSummary, DoctorReport, ProviderListItem, ProviderSwitchPolicy, StatusSummary,
};
use ochub_core::gateway::GatewayStatus;
use ochub_core::runtime::journal::{OperationRecord, OperationState};

use crate::components::{self, BadgeTone, ButtonSize, ButtonTone};
use crate::i18n::{k, t};
use crate::icons::{IconName, icon};
use crate::layout;
use crate::notifications::NotificationLevel;
use crate::remote::{
    ProviderSwitchHandle, RemoteClient, RemoteHost, RemoteHostStore, ScannedHostKey,
    SshConfigEntry, WorkspaceBackend, discover_ssh_connections, scan_host_keys, trust_host_key,
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
    status: StatusSummary,
    apps: Vec<AppSummary>,
    selected_app: Option<AppId>,
    providers: Vec<ProviderListItem>,
    gateway: GatewayStatus,
    operations: Vec<OperationRecord>,
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
    version: Option<String>,
    platform: Option<String>,
    error: Option<String>,
}

impl NodeProbe {
    fn checking() -> Self {
        Self {
            state: ProbeState::Checking,
            version: None,
            platform: None,
            error: None,
        }
    }

    fn online(client: &RemoteClient) -> Self {
        let handshake = client.handshake();
        Self {
            state: ProbeState::Online,
            version: Some(handshake.server_version.clone()),
            platform: Some(format!("{} · {}", handshake.node.os, handshake.node.arch)),
            error: None,
        }
    }

    fn offline(error: String) -> Self {
        Self {
            state: ProbeState::Offline,
            version: None,
            platform: None,
            error: Some(error),
        }
    }
}

#[derive(Clone)]
pub(crate) struct RemoteScopeItem {
    pub id: String,
    pub label: String,
    pub target: String,
}

#[derive(Clone)]
pub(crate) enum RemoteEvent {
    ConnectionChanged { id: String, connected: bool },
}

pub struct RemoteView {
    store: RemoteHostStore,
    selected_id: Option<String>,
    connection_state: ConnectionState,
    connection_generation: u64,
    client: Option<Arc<RemoteClient>>,
    backend: Option<WorkspaceBackend>,
    remote_status: Option<StatusSummary>,
    apps: Vec<AppSummary>,
    selected_app: Option<AppId>,
    providers: Vec<ProviderListItem>,
    gateway: Option<GatewayStatus>,
    operations: Vec<OperationRecord>,
    doctor_report: Option<DoctorReport>,
    ssh_diagnostics: Vec<String>,
    pending_plan: Option<ProviderSwitchHandle>,
    pending_provider_name: Option<String>,
    add_open: bool,
    add_mode: AddMode,
    ssh_config_entries: Vec<SshConfigEntry>,
    selected_ssh_config: Option<usize>,
    ssh_config_error: Option<String>,
    label_input: Entity<TextInput>,
    target_input: Entity<TextInput>,
    hostname_input: Entity<TextInput>,
    port_input: Entity<TextInput>,
    cli_input: Entity<TextInput>,
    scanned_keys: Vec<ScannedHostKey>,
    node_probes: HashMap<String, NodeProbe>,
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
        let label_input = cx.new(|cx| TextInput::new(cx, t(k::REMOTE_FIELD_LABEL)));
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
        Self {
            store,
            selected_id,
            connection_state: ConnectionState::Disconnected,
            connection_generation: 0,
            client: None,
            backend: None,
            remote_status: None,
            apps: Vec::new(),
            selected_app: None,
            providers: Vec::new(),
            gateway: None,
            operations: Vec::new(),
            doctor_report: None,
            ssh_diagnostics: Vec::new(),
            pending_plan: None,
            pending_provider_name: None,
            add_open: false,
            add_mode: AddMode::SshConfig,
            ssh_config_entries: Vec::new(),
            selected_ssh_config: None,
            ssh_config_error: None,
            label_input,
            target_input,
            hostname_input,
            port_input,
            cli_input,
            scanned_keys: Vec::new(),
            node_probes: HashMap::new(),
            probe_generation: 0,
            probing: false,
            busy: false,
            scroll: ScrollHandle::new(),
            status,
            status_level,
        }
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
                label: host.label.clone(),
                target: host.ssh_alias.clone(),
            })
            .collect()
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
        self.label_input
            .update(cx, |input, cx| input.set_content("", cx));
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
        self.label_input
            .update(cx, |input, cx| input.set_content(&entry.alias, cx));
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
        let label = self.label_input.read(cx).content().trim().to_string();
        let target = self.target_input.read(cx).content().trim().to_string();
        let hostname = self.hostname_input.read(cx).content().trim().to_string();
        if label.is_empty() || target.is_empty() || hostname.is_empty() {
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
            label: self.label_input.read(cx).content().trim().to_string(),
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
                        Err(error) => NodeProbe::offline(error.to_string()),
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

    fn connect_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected_id.clone() {
            self.connect_host(id, cx);
        }
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
        self.pending_plan = None;
        self.pending_provider_name = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                let client = RemoteClient::connect(host)
                    .await
                    .map_err(|error| error.to_string())?;
                let backend = WorkspaceBackend::remote(client.clone());
                let snapshot = load_snapshot(backend.clone(), None).await?;
                Ok::<_, String>((client, backend, snapshot))
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
                        let node_label = client.host().label.clone();
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
                                    tf!(k::REMOTE_SUCCESS_CONNECTED, node = node_label),
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
                        this.node_probes
                            .insert(id.clone(), NodeProbe::offline(error.clone()));
                        this.set_status(
                            tf!(k::REMOTE_ERROR_CONNECT, error = error),
                            NotificationLevel::Error,
                        );
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
        self.remote_status = None;
        self.apps.clear();
        self.selected_app = None;
        self.providers.clear();
        self.gateway = None;
        self.operations.clear();
        self.doctor_report = None;
        self.ssh_diagnostics.clear();
        self.pending_plan = None;
        self.pending_provider_name = None;
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

    fn toggle_host(&mut self, id: String, cx: &mut Context<Self>) {
        if self.selected_id.as_deref() == Some(id.as_str())
            && self.connection_state == ConnectionState::Connected
        {
            self.disconnect(cx);
        } else {
            self.activate_scope(id, cx);
        }
    }

    fn back_from_fingerprint(&mut self, cx: &mut Context<Self>) {
        self.scanned_keys.clear();
        cx.notify();
    }

    fn install_snapshot(&mut self, snapshot: RemoteSnapshot) {
        self.remote_status = Some(snapshot.status);
        self.apps = snapshot.apps;
        self.selected_app = snapshot.selected_app;
        self.providers = snapshot.providers;
        self.gateway = Some(snapshot.gateway);
        self.operations = snapshot.operations;
    }

    fn refresh_remote(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let preferred = self.selected_app.clone();
        self.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result =
                crate::core_async::run(async move { load_snapshot(backend, preferred).await })
                    .await;
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

    fn run_doctor(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let client = self.client.clone();
        self.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                let report = backend.doctor().await.map_err(|error| error.to_string())?;
                let diagnostics = match client {
                    Some(client) => client.diagnostics().await,
                    None => Vec::new(),
                };
                Ok::<_, String>((report, diagnostics))
            })
            .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((report, diagnostics)) => {
                        this.doctor_report = Some(report);
                        this.ssh_diagnostics = diagnostics;
                    }
                    Err(error) => this.set_status(
                        tf!(k::REMOTE_ERROR_DOCTOR, error = error),
                        NotificationLevel::Error,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_app(&mut self, app_id: String, cx: &mut Context<Self>) {
        if self.busy
            || self
                .selected_app
                .as_ref()
                .is_some_and(|app| app.as_str() == app_id)
        {
            return;
        }
        let Ok(app) = AppId::parse(&app_id) else {
            return;
        };
        let Some(backend) = self.backend.clone() else {
            return;
        };
        self.busy = true;
        self.pending_plan = None;
        self.pending_provider_name = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let app_for_request = app.clone();
            let result = crate::core_async::run(async move {
                backend
                    .list_providers(&app_for_request)
                    .await
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(providers) => {
                        this.selected_app = Some(app);
                        this.providers = providers;
                    }
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

    fn plan_switch(&mut self, provider_id: String, provider_name: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (Some(backend), Some(app)) = (self.backend.clone(), self.selected_app.clone()) else {
            return;
        };
        self.busy = true;
        self.pending_plan = None;
        self.pending_provider_name = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend
                    .plan_provider_switch(&app, &provider_id, ProviderSwitchPolicy::Abort)
                    .await
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(plan) => {
                        this.pending_plan = Some(plan);
                        this.pending_provider_name = Some(provider_name);
                    }
                    Err(error) => this.set_status(
                        tf!(k::REMOTE_ERROR_PLAN, error = error),
                        NotificationLevel::Error,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn cancel_plan(&mut self, cx: &mut Context<Self>) {
        self.pending_plan = None;
        self.pending_provider_name = None;
        cx.notify();
    }

    fn apply_plan(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (Some(backend), Some(handle)) = (self.backend.clone(), self.pending_plan.clone())
        else {
            return;
        };
        let provider_name = self
            .pending_provider_name
            .clone()
            .unwrap_or_else(|| handle.plan().provider_id.clone());
        let app = self.selected_app.clone();
        self.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = crate::core_async::run(async move {
                backend
                    .apply_provider_switch(handle)
                    .await
                    .map_err(|error| error.to_string())?;
                let providers = if let Some(app) = app {
                    backend
                        .list_providers(&app)
                        .await
                        .map_err(|error| error.to_string())?
                } else {
                    Vec::new()
                };
                Ok::<_, String>(providers)
            })
            .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(providers) => {
                        this.providers = providers;
                        this.pending_plan = None;
                        this.pending_provider_name = None;
                        this.set_status(
                            tf!(k::REMOTE_SUCCESS_APPLIED, provider = provider_name),
                            NotificationLevel::Success,
                        );
                    }
                    Err(error) => this.set_status(
                        tf!(k::REMOTE_ERROR_APPLY, error = error),
                        NotificationLevel::Error,
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_gateway(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (Some(backend), Some(gateway)) = (self.backend.clone(), self.gateway.as_ref()) else {
            return;
        };
        let running = !gateway.running;
        self.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let backend_for_request = backend.clone();
            let result = crate::core_async::run(async move {
                backend_for_request
                    .set_gateway_running(running)
                    .await
                    .map_err(|error| error.to_string())?;
                backend_for_request
                    .gateway_status()
                    .await
                    .map_err(|error| error.to_string())
            })
            .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(gateway) => {
                        this.gateway = Some(gateway);
                        this.set_status(
                            if running {
                                t(k::REMOTE_SUCCESS_GATEWAY_STARTED)
                            } else {
                                t(k::REMOTE_SUCCESS_GATEWAY_STOPPED)
                            },
                            NotificationLevel::Success,
                        );
                    }
                    Err(error) => this.set_status(
                        tf!(k::REMOTE_ERROR_GATEWAY, error = error),
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
                t(k::REMOTE_FIELD_LABEL),
                true,
                None,
                self.label_input.clone(),
            ))
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
                    self.remote_status
                        .as_ref()
                        .map(|status| status.version.clone())
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
                let error = (state == ProbeState::Offline)
                    .then(|| probe.and_then(|probe| probe.error.clone()))
                    .flatten();
                let toggle_id = id.clone();
                let manage_id = id.clone();
                let remove_id = id.clone();
                let toggle = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
                    this.toggle_host(toggle_id.clone(), cx)
                });
                let manage = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
                    this.activate_scope(manage_id.clone(), cx)
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
                    .child(
                        div()
                            .id(SharedString::from(format!("remote-toggle-{}", host.id)))
                            .role(gpui::Role::Switch)
                            .aria_label(SharedString::from(format!(
                                "{} · {}",
                                host.label, state_label
                            )))
                            .aria_toggled(if connected {
                                gpui::Toggled::True
                            } else {
                                gpui::Toggled::False
                            })
                            .cursor_pointer()
                            .child(layout::toggle(connected))
                            .on_click(move |_event, window, cx| toggle(&(), window, cx)),
                    )
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
                                            .child(SharedString::from(host.label.clone())),
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
                                    })
                                    .when_some(error, |row, error| {
                                        row.child(
                                            div()
                                                .truncate()
                                                .text_color(theme::red())
                                                .child(SharedString::from(format!("· {error}"))),
                                        )
                                    }),
                            ),
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

    fn render_scope(&self, host: &RemoteHost) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .w_full()
            .px_4()
            .py_3()
            .rounded_lg()
            .border_1()
            .border_color(theme::accent())
            .bg(theme::accent_soft())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::accent())
                    .child(icon(IconName::Terminal, theme::accent(), 15.))
                    .child(tf!(
                        k::REMOTE_SCOPE,
                        node = host.label,
                        target = host.ssh_alias
                    )),
            )
            .child(components::badge(
                if self.busy {
                    BadgeTone::Warning
                } else {
                    BadgeTone::Success
                },
                if self.busy {
                    t(k::REMOTE_BUSY_LOADING)
                } else {
                    t(k::REMOTE_NODE_CONNECTED)
                },
            ))
    }

    fn render_node_status(&self, host: &RemoteHost) -> gpui::Div {
        let Some(status) = &self.remote_status else {
            return div();
        };
        let node_id = self
            .client
            .as_ref()
            .map(|client| client.handshake().node.id.as_str())
            .unwrap_or("-");
        let handshake = self.client.as_ref().map(|client| client.handshake());
        let platform = handshake
            .map(|ack| {
                format!(
                    "{} {} · {} · {}",
                    ack.node.os, ack.node.arch, ack.node.hostname, ack.node.user
                )
            })
            .unwrap_or_else(|| "-".to_string());
        let runtime = handshake
            .and_then(|ack| {
                Some(tf!(
                    k::REMOTE_STATUS_RUNTIME,
                    kind = ack.runtime.owner_kind.as_deref()?,
                    pid = ack.runtime.owner_pid?
                ))
            })
            .unwrap_or_else(|| "-".to_string());
        let rows = vec![
            layout::row()
                .child(layout::row_label(
                    t(k::REMOTE_STATUS_NODE_ID),
                    SharedString::from(node_id.to_string()),
                ))
                .child(components::badge(
                    BadgeTone::Accent,
                    tf!(k::REMOTE_NODE_VERSION, version = status.version),
                ))
                .into_any_element(),
            layout::row()
                .child(layout::row_label(
                    t(k::REMOTE_STATUS_DATA_DIR),
                    SharedString::from(status.data_dir.clone()),
                ))
                .child(components::badge(
                    BadgeTone::Neutral,
                    tf!(
                        k::REMOTE_STATUS_APPS,
                        enabled = status.enabled_apps,
                        total = status.registered_apps
                    ),
                ))
                .into_any_element(),
            layout::row()
                .child(layout::row_label(
                    t(k::REMOTE_STATUS_PLATFORM),
                    SharedString::from(platform),
                ))
                .child(components::badge(BadgeTone::Neutral, runtime))
                .into_any_element(),
        ];
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(layout::section_header(
                t(k::REMOTE_STATUS_TITLE),
                host.remote_node_id
                    .clone()
                    .map(SharedString::from)
                    .unwrap_or_else(|| SharedString::from(node_id.to_string())),
            ))
            .child(layout::group(rows))
    }

    fn render_doctor(&self, cx: &mut Context<Self>) -> gpui::Div {
        let run = cx.listener(|this: &mut Self, _: &(), _window, cx| this.run_doctor(cx));
        let action = if self.busy {
            components::disabled_button(
                "remote-doctor-disabled",
                t(k::REMOTE_ACTION_DOCTOR),
                ButtonTone::Neutral,
                ButtonSize::Sm,
                true,
            )
            .into_any_element()
        } else {
            components::button(
                "remote-doctor",
                t(k::REMOTE_ACTION_DOCTOR),
                ButtonTone::Neutral,
                ButtonSize::Sm,
            )
            .on_click(move |_event, window, cx| run(&(), window, cx))
            .into_any_element()
        };
        let mut block = div().flex().flex_col().gap_2().child(
            layout::section_header(
                t(k::REMOTE_DOCTOR_TITLE),
                self.doctor_report
                    .as_ref()
                    .map(|report| {
                        if report.healthy {
                            t(k::REMOTE_DOCTOR_HEALTHY)
                        } else {
                            t(k::REMOTE_DOCTOR_FAILED)
                        }
                    })
                    .unwrap_or_else(|| t(k::REMOTE_SECURITY_NOTE)),
            )
            .child(action),
        );
        if let Some(report) = &self.doctor_report {
            let mut rows = report
                .checks
                .iter()
                .map(|check| {
                    let healthy = matches!(check.status.as_str(), "ok" | "healthy" | "pass");
                    layout::row()
                        .child(layout::row_label(
                            SharedString::from(check.id.clone()),
                            SharedString::from(check.message.clone()),
                        ))
                        .child(components::badge(
                            if healthy {
                                BadgeTone::Success
                            } else {
                                BadgeTone::Warning
                            },
                            SharedString::from(check.status.clone()),
                        ))
                        .into_any_element()
                })
                .collect::<Vec<_>>();
            rows.extend(
                self.ssh_diagnostics
                    .iter()
                    .take(5)
                    .enumerate()
                    .map(|(index, line)| {
                        layout::row()
                            .child(layout::row_label(
                                SharedString::from(format!("ssh.stderr.{}", index + 1)),
                                SharedString::from(line.clone()),
                            ))
                            .child(components::badge(BadgeTone::Neutral, "SSH"))
                            .into_any_element()
                    }),
            );
            block = block.child(layout::group(rows));
        }
        block
    }

    fn render_operations(&self) -> gpui::Div {
        let rows = self
            .operations
            .iter()
            .take(5)
            .map(|operation| {
                let tone = match operation.state {
                    OperationState::Completed => BadgeTone::Success,
                    OperationState::Failed | OperationState::RecoveryRequired => BadgeTone::Danger,
                    OperationState::Planned | OperationState::Prepared => BadgeTone::Warning,
                    OperationState::RolledBack => BadgeTone::Neutral,
                };
                let state = serde_json::to_value(operation.state)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".to_string());
                layout::row()
                    .child(layout::row_label(
                        SharedString::from(operation.operation.clone()),
                        SharedString::from(format!(
                            "{} · {} · {}",
                            operation.actor, operation.started_at, operation.id
                        )),
                    ))
                    .child(components::badge(tone, state))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(layout::section_header(
                t(k::REMOTE_OPERATION_TITLE),
                if rows.is_empty() {
                    t(k::REMOTE_OPERATION_EMPTY)
                } else {
                    t(k::REMOTE_PLAN_DESC)
                },
            ))
            .when(!rows.is_empty(), |block| block.child(layout::group(rows)))
    }

    fn render_apps_and_providers(&self, cx: &mut Context<Self>) -> gpui::Div {
        let manageable = self
            .apps
            .iter()
            .filter(|app| app.enabled && app.supports_provider)
            .collect::<Vec<_>>();
        let mut block = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(layout::section_header(
                t(k::REMOTE_APP_TITLE),
                if manageable.is_empty() {
                    t(k::REMOTE_APP_EMPTY)
                } else {
                    tf!(
                        k::REMOTE_STATUS_APPS,
                        enabled = self.apps.iter().filter(|app| app.enabled).count(),
                        total = self.apps.len()
                    )
                    .into()
                },
            ));
        if !manageable.is_empty() {
            let mut tabs = div().flex().flex_row().flex_wrap().gap_2();
            for app in manageable {
                let id = app.id.clone();
                let selected = self
                    .selected_app
                    .as_ref()
                    .is_some_and(|selected| selected.as_str() == app.id);
                let select = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
                    this.select_app(id.clone(), cx)
                });
                tabs = tabs.child(
                    components::button(
                        SharedString::from(format!("remote-app-{}", app.id)),
                        SharedString::from(app.display_name.clone()),
                        if selected {
                            ButtonTone::Primary
                        } else {
                            ButtonTone::Neutral
                        },
                        ButtonSize::Sm,
                    )
                    .on_click(move |_event, window, cx| select(&(), window, cx)),
                );
            }
            block = block.child(tabs);
        }
        if self.providers.is_empty() {
            return block.child(
                div()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::surface())
                    .child(components::empty_state(
                        IconName::Cloud,
                        t(k::REMOTE_PROVIDER_EMPTY),
                        t(k::REMOTE_SECURITY_NOTE),
                        None,
                    )),
            );
        }
        let rows = self
            .providers
            .iter()
            .map(|provider| {
                let id = provider.id.clone();
                let name = provider.name.clone();
                let plan = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
                    this.plan_switch(id.clone(), name.clone(), cx)
                });
                let action = if provider.current {
                    components::badge(BadgeTone::Success, t(k::REMOTE_PROVIDER_CURRENT))
                        .into_any_element()
                } else if self.busy {
                    components::disabled_button(
                        SharedString::from(format!("remote-plan-disabled-{}", provider.id)),
                        t(k::REMOTE_ACTION_PLAN),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                        true,
                    )
                    .into_any_element()
                } else {
                    components::button(
                        SharedString::from(format!("remote-plan-{}", provider.id)),
                        t(k::REMOTE_ACTION_PLAN),
                        ButtonTone::Neutral,
                        ButtonSize::Sm,
                    )
                    .on_click(move |_event, window, cx| plan(&(), window, cx))
                    .into_any_element()
                };
                layout::row()
                    .child(layout::row_label(
                        SharedString::from(provider.name.clone()),
                        SharedString::from(provider.base_url.clone()),
                    ))
                    .child(action)
                    .into_any_element()
            })
            .collect();
        block.child(layout::group(rows))
    }

    fn render_gateway(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(gateway) = &self.gateway else {
            return div();
        };
        let toggle = cx.listener(|this: &mut Self, _: &(), _window, cx| this.toggle_gateway(cx));
        let (description, action) = if gateway.running {
            (
                tf!(k::REMOTE_GATEWAY_RUNNING, url = gateway.base_url),
                t(k::REMOTE_GATEWAY_STOP),
            )
        } else {
            (
                tf!(k::REMOTE_GATEWAY_STOPPED, port = gateway.port),
                t(k::REMOTE_GATEWAY_START),
            )
        };
        let button = if self.busy {
            components::disabled_button(
                "remote-gateway-toggle-disabled",
                action,
                ButtonTone::Neutral,
                ButtonSize::Sm,
                true,
            )
            .into_any_element()
        } else {
            components::button(
                "remote-gateway-toggle",
                action,
                if gateway.running {
                    ButtonTone::Danger
                } else {
                    ButtonTone::Primary
                },
                ButtonSize::Sm,
            )
            .on_click(move |_event, window, cx| toggle(&(), window, cx))
            .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(layout::section_header(
                t(k::REMOTE_GATEWAY_TITLE),
                description,
            ))
            .child(layout::group(vec![
                layout::row()
                    .child(layout::row_label(
                        t(k::REMOTE_GATEWAY_TITLE),
                        SharedString::from(format!("{}:{}", gateway.base_url, gateway.port)),
                    ))
                    .child(button)
                    .into_any_element(),
            ]))
    }

    fn render_plan(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let handle = self.pending_plan.as_ref()?;
        let plan = handle.plan();
        let provider = self
            .pending_provider_name
            .as_deref()
            .unwrap_or(plan.provider_id.as_str());
        let current = plan
            .current_provider_id
            .as_deref()
            .unwrap_or_else(|| crate::i18n::raw(k::REMOTE_PLAN_NONE));
        let target_node = self
            .selected_id
            .as_deref()
            .and_then(|id| self.store.get(id));
        let cancel = cx.listener(|this: &mut Self, _: &(), _window, cx| this.cancel_plan(cx));
        let apply = cx.listener(|this: &mut Self, _: &(), _window, cx| this.apply_plan(cx));
        Some(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .w_full()
                .rounded_lg()
                .border_1()
                .border_color(theme::yellow())
                .bg(theme::yellow_soft())
                .p_4()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::text())
                        .child(t(k::REMOTE_PLAN_TITLE)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::subtext())
                        .child(t(k::REMOTE_PLAN_DESC)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .text_xs()
                        .text_color(theme::subtext())
                        .when_some(target_node, |lines, host| {
                            lines.child(tf!(
                                k::REMOTE_PLAN_SCOPE,
                                node = host.label,
                                target = host.ssh_alias
                            ))
                        })
                        .child(tf!(k::REMOTE_PLAN_CURRENT, provider = current))
                        .child(tf!(k::REMOTE_PLAN_TARGET, provider = provider))
                        .child(tf!(k::REMOTE_PLAN_PATH, path = plan.config_path))
                        .child(tf!(k::REMOTE_PLAN_REVISION, revision = handle.revision()))
                        .child(if plan.would_change {
                            t(k::REMOTE_PLAN_WILL_CHANGE)
                        } else {
                            t(k::REMOTE_PLAN_WONT_CHANGE)
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(
                            components::button(
                                "remote-plan-cancel",
                                t(k::REMOTE_ACTION_CANCEL),
                                ButtonTone::Neutral,
                                ButtonSize::Sm,
                            )
                            .on_click(move |_event, window, cx| cancel(&(), window, cx)),
                        )
                        .child(if self.busy {
                            components::disabled_button(
                                "remote-plan-apply-disabled",
                                t(k::REMOTE_ACTION_APPLY),
                                ButtonTone::Primary,
                                ButtonSize::Sm,
                                true,
                            )
                            .into_any_element()
                        } else {
                            components::button(
                                "remote-plan-apply",
                                t(k::REMOTE_ACTION_APPLY),
                                ButtonTone::Primary,
                                ButtonSize::Sm,
                            )
                            .on_click(move |_event, window, cx| apply(&(), window, cx))
                            .into_any_element()
                        }),
                ),
        )
    }

    fn render_selected_node(&self, cx: &mut Context<Self>) -> gpui::Div {
        let Some(id) = self.selected_id.as_deref() else {
            return div().flex().flex_1().child(components::empty_state(
                IconName::Desktop,
                t(k::REMOTE_EMPTY_TITLE),
                t(k::REMOTE_EMPTY_DESC),
                None,
            ));
        };
        let Some(host) = self.store.get(id) else {
            return div();
        };
        let remove_id = host.id.clone();
        let connect = cx.listener(|this: &mut Self, _: &(), _window, cx| this.connect_selected(cx));
        let disconnect = cx.listener(|this: &mut Self, _: &(), _window, cx| this.disconnect(cx));
        let refresh = cx.listener(|this: &mut Self, _: &(), _window, cx| this.refresh_remote(cx));
        let remove = cx.listener(move |this: &mut Self, _: &(), _window, cx| {
            this.remove_host(remove_id.clone(), cx)
        });
        let status_label = match self.connection_state {
            ConnectionState::Disconnected => t(k::REMOTE_NODE_DISCONNECTED),
            ConnectionState::Connecting => t(k::REMOTE_NODE_CONNECTING),
            ConnectionState::Connected => t(k::REMOTE_NODE_CONNECTED),
        };
        let primary = match self.connection_state {
            ConnectionState::Disconnected => {
                if self.busy {
                    components::disabled_button(
                        "remote-connect-disabled",
                        status_label.clone(),
                        ButtonTone::Primary,
                        ButtonSize::Md,
                        true,
                    )
                    .into_any_element()
                } else {
                    components::button(
                        "remote-connect",
                        t(k::REMOTE_ACTION_CONNECT),
                        ButtonTone::Primary,
                        ButtonSize::Md,
                    )
                    .on_click(move |_event, window, cx| connect(&(), window, cx))
                    .into_any_element()
                }
            }
            ConnectionState::Connecting => components::disabled_button(
                "remote-connecting",
                status_label.clone(),
                ButtonTone::Primary,
                ButtonSize::Md,
                true,
            )
            .into_any_element(),
            ConnectionState::Connected => components::button(
                "remote-disconnect",
                t(k::REMOTE_ACTION_DISCONNECT),
                ButtonTone::Neutral,
                ButtonSize::Md,
            )
            .on_click(move |_event, window, cx| disconnect(&(), window, cx))
            .into_any_element(),
        };
        let last_seen = host
            .last_seen_at
            .as_deref()
            .map(|time| tf!(k::REMOTE_HOST_LAST_SEEN, time = time))
            .unwrap_or_else(|| t(k::REMOTE_HOST_NEVER_SEEN).to_string());
        let mut detail = div().flex().flex_col().flex_1().min_w_0().gap_3().child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::text())
                                .child(SharedString::from(host.label.clone())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(format!("{} · {}", host.ssh_alias, last_seen)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .when(self.connection_state == ConnectionState::Connected, |row| {
                            row.child(
                                components::button(
                                    "remote-refresh",
                                    t(k::REMOTE_ACTION_REFRESH),
                                    ButtonTone::Ghost,
                                    ButtonSize::Md,
                                )
                                .on_click(move |_event, window, cx| refresh(&(), window, cx)),
                            )
                        })
                        .child(primary),
                ),
        );
        if self.connection_state == ConnectionState::Connected {
            detail = detail
                .child(self.render_scope(host))
                .child(self.render_node_status(host))
                .child(self.render_apps_and_providers(cx))
                .child(self.render_gateway(cx))
                .child(self.render_doctor(cx))
                .child(self.render_operations());
            if let Some(plan) = self.render_plan(cx) {
                detail = detail.child(plan);
            }
        } else {
            detail = detail
                .child(
                    div()
                        .w_full()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme::border())
                        .bg(theme::surface())
                        .child(components::empty_state(
                            IconName::Terminal,
                            status_label,
                            if self.busy {
                                t(k::REMOTE_BUSY_CONNECTING)
                            } else {
                                t(k::REMOTE_SECURITY_NOTE)
                            },
                            None,
                        )),
                )
                .child(
                    div().flex().flex_row().justify_end().child(
                        components::button(
                            "remote-remove",
                            t(k::REMOTE_ACTION_REMOVE),
                            ButtonTone::Danger,
                            ButtonSize::Sm,
                        )
                        .on_click(move |_event, window, cx| remove(&(), window, cx)),
                    ),
                );
        }
        detail
    }
}

async fn load_snapshot(
    backend: WorkspaceBackend,
    preferred: Option<AppId>,
) -> Result<RemoteSnapshot, String> {
    let (status, apps, gateway, operations) = tokio::join!(
        backend.status(),
        backend.list_apps(),
        backend.gateway_status(),
        backend.list_operations()
    );
    let status = status.map_err(|error| error.to_string())?;
    let apps = apps.map_err(|error| error.to_string())?;
    let gateway = gateway.map_err(|error| error.to_string())?;
    let operations = operations.map_err(|error| error.to_string())?;
    let selected_app = preferred
        .filter(|preferred| {
            apps.iter()
                .any(|app| app.id == preferred.as_str() && app.enabled && app.supports_provider)
        })
        .or_else(|| {
            apps.iter()
                .find(|app| app.enabled && app.supports_provider)
                .and_then(|app| AppId::parse(&app.id).ok())
        });
    let providers = match &selected_app {
        Some(app) => backend
            .list_providers(app)
            .await
            .map_err(|error| error.to_string())?,
        None => Vec::new(),
    };
    Ok(RemoteSnapshot {
        status,
        apps,
        selected_app,
        providers,
        gateway,
        operations,
    })
}

impl gpui::Render for RemoteView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            let mut column = layout::wide_column().child(self.render_connection_list(cx));
            if self.connection_state == ConnectionState::Connected {
                column = column.child(self.render_selected_node(cx));
            }
            column
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
        page
    }
}

crate::notifications::impl_status_toasts_leveled!(RemoteView);
