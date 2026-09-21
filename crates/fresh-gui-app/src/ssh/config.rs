//! Saved SSH targets (`user@host` or an OpenSSH Host alias).
//!
//! The file is host-local. It never stores the ADE bearer token.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::probe::{validate_destination, validate_name};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemotesFile {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Release asset URL (`.tar.gz` with `bin/fresh-gui`). Empty → GitHub latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_url: Option<String>,
    /// Local Linux `fresh-gui` binary, or a `.tar.gz` archive of one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_path: Option<String>,
    #[serde(default)]
    pub targets: Vec<SshTarget>,
}

fn default_version() -> u32 {
    1
}

impl Default for RemotesFile {
    fn default() -> Self {
        Self {
            version: 1,
            daemon_url: None,
            daemon_path: None,
            targets: Vec::new(),
        }
    }
}

impl RemotesFile {
    pub fn add(&mut self, target: SshTarget) -> Result<()> {
        target.validate()?;
        if self.targets.iter().any(|t| t.name == target.name) {
            bail!(
                "SSH target '{}' already exists (remove it first)",
                target.name
            );
        }
        self.targets.push(target);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<SshTarget> {
        let Some(ix) = self.targets.iter().position(|t| t.name == name) else {
            bail!("no SSH target named '{name}'");
        };
        Ok(self.targets.remove(ix))
    }

    pub fn get(&self, name: &str) -> Result<&SshTarget> {
        self.targets
            .iter()
            .find(|t| t.name == name)
            .with_context(|| format!("no SSH target named '{name}'"))
    }
}

/// One saved remote. Auth is OpenSSH only (`ssh` on `PATH`, keys / agent / config).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshTarget {
    pub name: String,
    /// `user@host` or an OpenSSH config `Host` alias.
    pub destination: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Optional identity file (`ssh -i`). Must not start with `-`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Passed to `fresh-gui --root` when this host starts the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_root: Option<String>,
    /// Preferred local tunnel port. Omitted → 7420 if free, else ephemeral.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_port: Option<u16>,
}

impl SshTarget {
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        validate_destination(&self.destination)?;
        if self.port == Some(0) {
            bail!("SSH port must be non-zero");
        }
        if self.local_port == Some(0) {
            bail!("local tunnel port must be non-zero (omit it to auto-pick)");
        }
        if self.identity.as_deref().is_some_and(|id| {
            id.is_empty() || id.starts_with('-') || id.contains('\n') || id.contains('\0')
        }) {
            bail!("identity path is invalid");
        }
        if self
            .remote_root
            .as_deref()
            .is_some_and(|root| root.contains('\0') || root.contains('\n'))
        {
            bail!("remote root is invalid");
        }
        Ok(())
    }
}

pub fn remotes_config_path() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA").context("%APPDATA% is unset")?;
        return Ok(PathBuf::from(base).join("fresh-gui").join("remotes.json"));
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return Ok(PathBuf::from(xdg).join("fresh-gui").join("remotes.json"));
        }
        let home = std::env::var_os("HOME").context("HOME is unset")?;
        Ok(PathBuf::from(home)
            .join(".config")
            .join("fresh-gui")
            .join("remotes.json"))
    }
}

pub fn load_remotes(path: &Path) -> Result<RemotesFile> {
    if !path.is_file() {
        return Ok(RemotesFile::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

pub fn save_remotes(path: &Path, file: &RemotesFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let text = serde_json::to_string_pretty(file).context("serialize remotes")?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_duplicate_name() {
        let dir = std::env::temp_dir().join(format!(
            "fresh-gui-remotes-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("remotes.json");
        let mut file = RemotesFile::default();
        file.add(SshTarget {
            name: "lab".into(),
            destination: "ada@lab".into(),
            port: Some(22),
            identity: None,
            remote_root: Some("/work/proj".into()),
            local_port: None,
        })
        .unwrap();
        assert!(
            file.add(SshTarget {
                name: "lab".into(),
                destination: "ada@other".into(),
                port: None,
                identity: None,
                remote_root: None,
                local_port: None,
            })
            .is_err()
        );
        file.daemon_url = Some("https://example.test/daemon.tar.gz".into());
        save_remotes(&path, &file).unwrap();
        let loaded = load_remotes(&path).unwrap();
        assert_eq!(loaded, file);
        assert_eq!(loaded.get("lab").unwrap().destination, "ada@lab");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_empty() {
        let path =
            std::env::temp_dir().join(format!("fresh-gui-remotes-missing-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        let loaded = load_remotes(&path).unwrap();
        assert!(loaded.targets.is_empty());
    }
}
