//! Desktop-side Remote Nodes infrastructure.
//!
//! GPUI views consume [`WorkspaceBackend`]; SSH process management and the
//! JSONL protocol stay below that boundary.

mod backend;
mod client;
mod ssh;
mod ssh_config;
mod store;

pub(crate) use backend::{
    NodeInstallStatus, NodeUpdateInstallResult, NodeUpdateReport, ProviderSwitchHandle,
    WorkspaceBackend, WorkspaceBackendError,
};
pub(crate) use client::{
    RemoteClient, RemoteClientError, RemoteConnectionIssue, RemoteConnectionIssueKind,
    RemoteRequestOptions,
};
pub(crate) use ssh::{
    BootstrapProbe, ScannedHostKey, install_bootstrap, probe_bootstrap, relay_node_update,
    scan_host_keys, trust_host_key,
};
pub(crate) use ssh_config::{SshConfigEntry, discover_ssh_connections};
pub(crate) use store::{RemoteHost, RemoteHostStore};
