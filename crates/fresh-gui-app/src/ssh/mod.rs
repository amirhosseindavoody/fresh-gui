//! SSH remote bootstrap for the GPUI host.
//!
//! Fresh's Orchestrator can describe SSH workspaces, but that stack is TUI /
//! plugin code and is not linked into the ADE daemon (`fresh-editor` feature
//! `runtime` only). This module shells out to the system OpenSSH client
//! (`ssh` / `scp`) and then speaks ADE over the resulting loopback tunnel.
//! Authentication is whatever OpenSSH already does (keys, agent, `ssh_config`).

mod bootstrap;
mod config;
mod probe;

pub use bootstrap::{DaemonSource, RemoteControlHandle, RemoteEndpoint, RemoteSession, Toolchain, bootstrap};
pub use config::{SshTarget, load_remotes, remotes_config_path, save_remotes};
pub use probe::validate_destination;
