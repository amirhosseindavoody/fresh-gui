# fresh-gui-protocol

Shared Rust message types for communication between the desktop host and daemon. The protocol covers connection setup, workspaces and sessions, terminals, files, editor operations, and source control. The optional `editor.external-changes` capability reports revisioned disk generations for open editor buffers and requires explicit resolution before an external change can replace a draft or be overwritten by a save.

This crate is an implementation component, not a separate user-facing application. See the [project README](../../README.md) for installation and use.
