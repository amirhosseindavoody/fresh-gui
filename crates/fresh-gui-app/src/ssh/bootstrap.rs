//! Connect path: probe → maybe SCP the Linux daemon → start headless → tunnel.

use std::fs::{self, File};
use std::io::{self, IsTerminal, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use super::config::{RemotesFile, SshTarget};
use super::probe::{self, PROBE_SCRIPT, Probe};

/// Where the Linux `fresh-gui` binary comes from when the remote has none.
///
/// Precedence: `FRESH_GUI_DAEMON_PATH`, `FRESH_GUI_DAEMON_URL`, the saved
/// file's path, the saved file's URL, then the latest GitHub linux-gnu asset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonSource {
    pub path: Option<PathBuf>,
    pub url: Option<String>,
}

impl DaemonSource {
    pub fn resolve(file: &RemotesFile, env_path: Option<&str>, env_url: Option<&str>) -> Self {
        let nonempty = |s: &str| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        if let Some(path) = env_path.and_then(nonempty) {
            return Self {
                path: Some(PathBuf::from(path)),
                url: None,
            };
        }
        if let Some(url) = env_url.and_then(nonempty) {
            return Self {
                path: None,
                url: Some(url),
            };
        }
        if let Some(path) = file.daemon_path.as_deref().and_then(nonempty) {
            return Self {
                path: Some(PathBuf::from(path)),
                url: None,
            };
        }
        if let Some(url) = file.daemon_url.as_deref().and_then(nonempty) {
            return Self {
                path: None,
                url: Some(url),
            };
        }
        Self::default()
    }
}

/// Programs used to talk to OpenSSH and to fetch/extract a release archive.
#[derive(Clone, Debug)]
pub struct Toolchain {
    pub ssh: PathBuf,
    pub scp: PathBuf,
    pub curl: PathBuf,
    pub tar: PathBuf,
}

impl Default for Toolchain {
    fn default() -> Self {
        Self {
            ssh: PathBuf::from("ssh"),
            scp: PathBuf::from("scp"),
            curl: PathBuf::from("curl"),
            tar: PathBuf::from("tar"),
        }
    }
}

/// Live local tunnel. Dropping this value kills the `ssh -N` process.
pub struct RemoteSession {
    pub destination: String,
    pub remote_port: u16,
    pub local_port: u16,
    /// ADE bearer token read from the remote session file. Not logged.
    pub token: Option<String>,
    pub ws_url: String,
    tunnel: Child,
}

/// Cloneable control for an SSH remote that stays alive for the GUI lifetime.
/// Restarting drops the old tunnel before binding its local port again.
#[derive(Clone)]
pub struct RemoteControlHandle(Arc<Mutex<RemoteControlState>>);

struct RemoteControlState {
    target: SshTarget,
    daemon: DaemonSource,
    tools: Toolchain,
    session: Option<RemoteSession>,
}

impl std::fmt::Debug for RemoteControlHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteControlHandle").finish_non_exhaustive()
    }
}

impl PartialEq for RemoteControlHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for RemoteControlHandle {}

impl RemoteControlHandle {
    pub fn new(
        target: SshTarget,
        daemon: DaemonSource,
        tools: Toolchain,
        session: RemoteSession,
    ) -> Self {
        Self(Arc::new(Mutex::new(RemoteControlState {
            target,
            daemon,
            tools,
            session: Some(session),
        })))
    }

    /// Stop the running remote daemon, then bootstrap it and return the new endpoint.
    pub fn restart(&self) -> Result<RemoteEndpoint> {
        let mut state = self.0.lock().map_err(|_| anyhow::anyhow!("remote control lock poisoned"))?;
        let previous = state.session.take();
        if let Some(ref session) = previous {
            state.target.local_port = Some(session.local_port);
        }
        // Release the port before bootstrap starts the replacement tunnel.
        drop(previous);

        let probe = ssh_probe(&state.target, &state.tools)?;
        if probe.running {
            let binary = probe.binary.as_deref().context("remote probe has no binary path")?;
            ssh_simple(
                &state.target,
                &state.tools,
                &format!("{} close", probe::shell_single_quote(binary)),
            )?;
            if ssh_probe(&state.target, &state.tools)?.running {
                bail!("remote fresh-gui session is still running after close");
            }
        }

        let session = bootstrap(&state.target, &state.daemon, &state.tools, |_| {})?;
        let endpoint = RemoteEndpoint {
            ws_url: session.ws_url.clone(),
            token: session.token.clone(),
            local_port: session.local_port,
        };
        state.session = Some(session);
        Ok(endpoint)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEndpoint {
    pub ws_url: String,
    pub token: Option<String>,
    pub local_port: u16,
}

impl std::fmt::Debug for RemoteSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSession")
            .field("destination", &self.destination)
            .field("remote_port", &self.remote_port)
            .field("local_port", &self.local_port)
            .field("ws_url", &self.ws_url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        let _ = self.tunnel.kill();
        let _ = self.tunnel.wait();
    }
}

const GITHUB_LATEST: &str =
    "https://api.github.com/repos/amirhosseindavoody/fresh-gui/releases/latest";

const INSTALLED_BIN: &str = "$HOME/.local/bin/fresh-gui";
const FINISH_INSTALL: &str = "chmod 755 \"$HOME/.local/bin/fresh-gui.partial\" && mv -f \"$HOME/.local/bin/fresh-gui.partial\" \"$HOME/.local/bin/fresh-gui\"";

pub fn bootstrap(
    target: &SshTarget,
    daemon: &DaemonSource,
    tools: &Toolchain,
    mut log: impl FnMut(&str),
) -> Result<RemoteSession> {
    target.validate()?;
    if let Some(id) = &target.identity {
        let path = Path::new(id);
        if !path.is_file() {
            bail!("identity file {} does not exist", path.display());
        }
    }

    log(&format!("Checking {}…", target.destination));
    let mut probe = ssh_probe(target, tools)?;
    let mut start_output = None;

    if probe.installed {
        let remote_version = probe.version.as_deref().context(
            "remote fresh-gui did not report a version; run `fresh-gui --version` on the server",
        )?;
        if compare_versions(env!("CARGO_PKG_VERSION"), remote_version)?
            == std::cmp::Ordering::Greater
        {
            consent_to_upgrade(&target.destination, remote_version)?;
            log("Downloading the matching Linux daemon before stopping the remote session…");
            let libc = remote_libc(target, tools)?;
            let url = release_daemon_url(env!("CARGO_PKG_VERSION"), libc);
            let binary = materialize_daemon(
                &DaemonSource {
                    path: None,
                    url: Some(url),
                },
                tools,
                &mut log,
            )?;
            if probe.running {
                let old_bin = probe
                    .binary
                    .as_deref()
                    .context("remote probe has no binary path")?;
                log("Stopping the remote fresh-gui session…");
                ssh_simple(
                    target,
                    tools,
                    &format!("{} close", probe::shell_single_quote(old_bin)),
                )?;
                if ssh_probe(target, tools)?.running {
                    bail!("remote fresh-gui session is still running after close; upgrade cancelled");
                }
            }
            log("Installing the matching Linux daemon…");
            ssh_simple(target, tools, "mkdir -p \"$HOME/.local/bin\"")?;
            scp_binary(target, tools, &binary)?;
            ssh_simple(target, tools, FINISH_INSTALL)?;
            probe = ssh_probe(target, tools)?;
            if probe.version.as_deref() != Some(env!("CARGO_PKG_VERSION")) {
                bail!(
                    "remote daemon version after upgrade does not match this client ({})",
                    env!("CARGO_PKG_VERSION")
                );
            }
            if probe.running {
                bail!("remote session remained running during the daemon upgrade; reconnect after stopping it");
            }
        }
    }

    if !probe.installed {
        log("Remote fresh-gui is not installed.");
        let binary = materialize_daemon(daemon, tools, &mut log)?;
        log("Copying the Linux daemon to ~/.local/bin/fresh-gui…");
        ssh_simple(target, tools, "mkdir -p \"$HOME/.local/bin\"")?;
        scp_binary(target, tools, &binary)?;
        ssh_simple(target, tools, FINISH_INSTALL)?;
        log("Starting headless fresh-gui…");
        start_output = Some(ssh_start(
            target,
            tools,
            &start_command(INSTALLED_BIN, target.remote_root.as_deref()),
        )?);
        probe = probe_after_start(target, tools)?;
    } else if !probe.running {
        let bin = probe
            .binary
            .clone()
            .context("remote fresh-gui is installed but the probe did not report its path")?;
        log("Remote fresh-gui is installed but no session is running. Starting it…");
        start_output = Some(ssh_start(
            target,
            tools,
            &start_command(&bin, target.remote_root.as_deref()),
        )?);
        probe = probe_after_start(target, tools)?;
    } else {
        log("Remote fresh-gui session is already running.");
    }

    if !probe.running {
        let mut detail = String::from("remote fresh-gui is not running after start.");
        append_remote_log(&mut detail, target, tools);
        if let Some(output) = start_output {
            let stderr = tail_redacted(&output.stderr);
            if !stderr.is_empty() {
                detail.push_str("\n--- start stderr ---\n");
                detail.push_str(&stderr);
            }
        }
        bail!(detail);
    }

    let remote_port = probe
        .port
        .context("remote session is running but session.json has no bound port")?;
    let local_port = pick_local_port(target.local_port)?;
    log(&format!(
        "Opening SSH tunnel 127.0.0.1:{local_port} → {} port {remote_port}",
        target.destination
    ));
    let args = tunnel_args(target, local_port, remote_port);
    let mut child = Command::new(&tools.ssh)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn {}", tools.ssh.display()))?;
    if let Err(err) = wait_local_port(local_port, &mut child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }

    Ok(RemoteSession {
        destination: target.destination.clone(),
        remote_port,
        local_port,
        token: probe.token,
        ws_url: format!("ws://127.0.0.1:{local_port}/ws"),
        tunnel: child,
    })
}

pub fn start_command(binary: &str, remote_root: Option<&str>) -> String {
    let mut cmd = String::from("env FRESH_GUI_QUIET=1 ");
    if binary == INSTALLED_BIN {
        cmd.push_str("\"$HOME/.local/bin/fresh-gui\"");
    } else {
        cmd.push_str(&probe::shell_single_quote(binary));
    }
    cmd.push_str(" --no-ui --json");
    if let Some(root) = remote_root.map(str::trim).filter(|s| !s.is_empty()) {
        cmd.push_str(" --root ");
        cmd.push_str(&probe::shell_single_quote(root));
    }
    cmd
}

fn ssh_probe(target: &SshTarget, tools: &Toolchain) -> Result<Probe> {
    let args = ssh_probe_args(target);
    let output = run_capturing(&tools.ssh, &args, Some(PROBE_SCRIPT.as_bytes()))?;
    ensure_success("ssh probe", &output)?;
    probe::parse_probe(&String::from_utf8_lossy(&output.stdout))
}

fn probe_after_start(target: &SshTarget, tools: &Toolchain) -> Result<Probe> {
    ssh_probe(target, tools).map_err(|error| {
        let mut detail = format!("remote session check after start failed: {error:#}");
        append_remote_log(&mut detail, target, tools);
        anyhow::anyhow!(detail)
    })
}

fn ssh_simple(
    target: &SshTarget,
    tools: &Toolchain,
    remote_cmd: &str,
) -> Result<std::process::Output> {
    let args = ssh_command_args(target, remote_cmd);
    let output = run_capturing(&tools.ssh, &args, None)?;
    ensure_success("ssh", &output)?;
    Ok(output)
}

fn ssh_start(target: &SshTarget, tools: &Toolchain, remote_cmd: &str) -> Result<std::process::Output> {
    let output = run_capturing(&tools.ssh, &ssh_command_args(target, remote_cmd), None)?;
    if !output.status.success() {
        let mut detail = format!("remote fresh-gui start failed ({})", output.status);
        append_remote_log(&mut detail, target, tools);
        let stderr = tail_redacted(&output.stderr);
        if !stderr.is_empty() {
            detail.push_str("\n--- start stderr ---\n");
            detail.push_str(&stderr);
        }
        bail!(detail);
    }
    Ok(output)
}

fn append_remote_log(detail: &mut String, target: &SshTarget, tools: &Toolchain) {
    let command = "log=\"${XDG_STATE_HOME:-$HOME/.local/state}/fresh-gui/fresh-gui.log\"; printf '\\nFRESH_GUI_LOG_BEGIN\\n'; if [ -f \"$log\" ]; then tail -n 80 \"$log\"; else printf 'No daemon log at %s\\n' \"$log\"; fi; printf '\\nFRESH_GUI_LOG_END\\n'";
    if let Ok(output) = ssh_simple(target, tools, command) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some((_, rest)) = stdout.rsplit_once("FRESH_GUI_LOG_BEGIN\n") {
            if let Some((log, _)) = rest.split_once("\nFRESH_GUI_LOG_END") {
                detail.push_str("\n--- remote daemon log ---\n");
                detail.push_str(&probe::redact_secrets(log));
            }
        }
    }
}

fn parse_version(version: &str) -> Result<Vec<u64>> {
    let value = version.trim().trim_start_matches('v');
    let parts: Vec<u64> = value.split('.').map(str::parse).collect::<std::result::Result<_, _>>()
        .with_context(|| format!("invalid fresh-gui version {version:?}"))?;
    if parts.len() < 2 || parts.len() > 3 {
        bail!("invalid fresh-gui version {version:?}");
    }
    Ok(parts)
}

fn compare_versions(local: &str, remote: &str) -> Result<std::cmp::Ordering> {
    let mut local = parse_version(local)?;
    let mut remote = parse_version(remote)?;
    local.resize(3, 0);
    remote.resize(3, 0);
    Ok(local.cmp(&remote))
}

fn consent_to_upgrade(destination: &str, remote_version: &str) -> Result<()> {
    let local_version = env!("CARGO_PKG_VERSION");
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("remote fresh-gui on {destination} is {remote_version}, but this client is {local_version}. An upgrade is required. Re-run in an interactive terminal to approve stopping the remote session and installing the matching daemon.");
    }
    eprint!("Remote fresh-gui on {destination} is {remote_version}; this client is {local_version}. Stop the remote session, install the matching Linux daemon, and reconnect? [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        bail!("remote upgrade declined; connection cancelled");
    }
    Ok(())
}

fn remote_libc(target: &SshTarget, tools: &Toolchain) -> Result<&'static str> {
    let output = ssh_simple(target, tools, "printf 'FRESH_GUI_ARCH=%s\\n' \"$(uname -m)\"; if [ -f /etc/alpine-release ]; then printf 'FRESH_GUI_LDD=musl\\n'; else printf 'FRESH_GUI_LDD=%s\\n' \"$(ldd --version 2>&1 | head -n 1)\"; fi")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let arch = text.lines().rev().find_map(|line| line.strip_prefix("FRESH_GUI_ARCH="))
        .context("remote architecture check returned no marker")?;
    if arch != "x86_64" && arch != "amd64" {
        bail!("no published Linux daemon for remote architecture {arch}");
    }
    let ldd = text.lines().rev().find_map(|line| line.strip_prefix("FRESH_GUI_LDD="))
        .unwrap_or("");
    if ldd.to_ascii_lowercase().contains("musl") {
        return Ok("musl");
    }
    let glibc = ldd.split_whitespace().rev().find_map(|word| {
        let word = word.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        parse_version(word).ok()
    });
    Ok(if glibc.is_some_and(|v| v.as_slice() >= [2, 31].as_slice()) { "gnu" } else { "musl" })
}

fn release_daemon_url(version: &str, libc: &str) -> String {
    format!("https://github.com/amirhosseindavoody/fresh-gui/releases/download/v{version}/fresh-gui-{version}-x86_64-unknown-linux-{libc}.tar.gz")
}

fn scp_binary(target: &SshTarget, tools: &Toolchain, local: &Path) -> Result<()> {
    let args = scp_args(target, local);
    let output = run_capturing(&tools.scp, &args, None)?;
    ensure_success("scp", &output)?;
    Ok(())
}

pub(crate) fn ssh_probe_args(target: &SshTarget) -> Vec<String> {
    let mut args = common_opts(target, "-p");
    args.push("-T".into());
    args.push(target.destination.clone());
    args.push("sh".into());
    args.push("-s".into());
    args
}

pub(crate) fn ssh_command_args(target: &SshTarget, remote_cmd: &str) -> Vec<String> {
    let mut args = common_opts(target, "-p");
    args.push("-T".into());
    args.push(target.destination.clone());
    args.push(format!("sh -c {}", probe::shell_single_quote(remote_cmd)));
    args
}

pub(crate) fn scp_args(target: &SshTarget, local: &Path) -> Vec<String> {
    let mut args = common_opts(target, "-P");
    args.push(local.display().to_string());
    args.push(format!("{}:.local/bin/fresh-gui.partial", target.destination));
    args
}

pub(crate) fn tunnel_args(target: &SshTarget, local_port: u16, remote_port: u16) -> Vec<String> {
    let mut args = common_opts(target, "-p");
    args.push("-N".into());
    args.push("-T".into());
    args.push("-o".into());
    args.push("ExitOnForwardFailure=yes".into());
    args.push("-L".into());
    args.push(format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"));
    args.push(target.destination.clone());
    args
}

fn common_opts(target: &SshTarget, port_flag: &str) -> Vec<String> {
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=20".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
    ];
    if let Some(port) = target.port {
        args.push(port_flag.into());
        args.push(port.to_string());
    }
    if let Some(id) = target.identity.as_ref().filter(|s| !s.is_empty()) {
        args.push("-i".into());
        args.push(id.clone());
    }
    args
}

fn run_capturing(
    program: &Path,
    args: &[String],
    stdin_bytes: Option<&[u8]>,
) -> Result<std::process::Output> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "failed to run {} (OpenSSH client and curl/tar must be on PATH)",
            program.display()
        )
    })?;
    if let Some(bytes) = stdin_bytes
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(bytes);
    }
    child.wait_with_output().context("wait for child")
}

fn ensure_success(what: &str, output: &std::process::Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = tail_redacted(&output.stderr);
    let stdout = tail_redacted(&output.stdout);
    let mut msg = format!("{what} failed ({})", output.status);
    if stderr.contains("Permission denied") || stderr.contains("BatchMode") {
        msg.push_str(
            "\nOpenSSH refused the connection. fresh-gui uses the system ssh client only (keys, agent, ~/.ssh/config) and does not prompt for a password.",
        );
    }
    if !stderr.is_empty() {
        msg.push('\n');
        msg.push_str(&stderr);
    }
    if !stdout.is_empty() {
        msg.push('\n');
        msg.push_str(&stdout);
    }
    bail!(msg)
}

fn tail_redacted(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = probe::redact_secrets(&text);
    const MAX: usize = 4000;
    if text.len() <= MAX {
        text
    } else {
        let mut cut = text.len() - MAX;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text[cut..].to_string()
    }
}

fn materialize_daemon(
    daemon: &DaemonSource,
    tools: &Toolchain,
    log: &mut dyn FnMut(&str),
) -> Result<PathBuf> {
    if let Some(path) = &daemon.path {
        if !path.is_file() {
            bail!(
                "daemon binary path {} is not a file. Set it with `fresh-gui remote daemon --path` or FRESH_GUI_DAEMON_PATH.",
                path.display()
            );
        }
        if is_archive(path) {
            log(&format!("Extracting {}…", path.display()));
            let dest = temp_path("fresh-gui-daemon-bin");
            extract_daemon_binary(&tools.tar, path, &dest)?;
            return Ok(dest);
        }
        return Ok(path.clone());
    }

    let url = if let Some(url) = &daemon.url {
        url.clone()
    } else {
        log("Resolving the latest linux-gnu fresh-gui release…");
        fetch_latest_linux_gnu_url(tools)?
    };
    log("Downloading the Linux daemon…");
    let archive = temp_path("fresh-gui-daemon.tar.gz");
    curl_to(tools, &url, &archive)?;
    log("Extracting the Linux daemon…");
    let dest = temp_path("fresh-gui-daemon-bin");
    extract_daemon_binary(&tools.tar, &archive, &dest)?;
    Ok(dest)
}

fn fetch_latest_linux_gnu_url(tools: &Toolchain) -> Result<String> {
    let output = Command::new(&tools.curl)
        .args([
            "-fsSL",
            "--retry",
            "3",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: fresh-gui-app",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
        ])
        .arg(GITHUB_LATEST)
        .output()
        .with_context(|| format!("run {}", tools.curl.display()))?;
    if !output.status.success() {
        bail!(
            "could not query GitHub releases ({})\n{}",
            output.status,
            tail_redacted(&output.stderr)
        );
    }
    let text = String::from_utf8(output.stdout).context("GitHub release JSON is not UTF-8")?;
    probe::linux_gnu_asset_url(&text)
}

fn curl_to(tools: &Toolchain, url: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let status = Command::new(&tools.curl)
        .args(["-fsSL", "--retry", "3", "-L", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .with_context(|| format!("run {}", tools.curl.display()))?;
    if !status.success() {
        bail!("download failed: {url}");
    }
    Ok(())
}

pub(crate) fn extract_daemon_binary(tar: &Path, archive: &Path, dest: &Path) -> Result<()> {
    let listing = Command::new(tar)
        .args(["-tzf"])
        .arg(archive)
        .output()
        .with_context(|| format!("run {}", tar.display()))?;
    if !listing.status.success() {
        bail!(
            "tar failed reading {}\n{}",
            archive.display(),
            tail_redacted(&listing.stderr)
        );
    }
    let names = String::from_utf8_lossy(&listing.stdout);
    let member = names
        .lines()
        .map(str::trim)
        .find(|line| *line == "bin/fresh-gui" || line.ends_with("/bin/fresh-gui"))
        .context("archive has no bin/fresh-gui member")?
        .to_string();
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let status = Command::new(tar)
        .args(["-xOzf"])
        .arg(archive)
        .arg(&member)
        .stdout(Stdio::from(file))
        .stderr(Stdio::piped())
        .status()
        .with_context(|| format!("extract {member}"))?;
    if !status.success() {
        bail!("tar failed extracting {member}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn is_archive(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    name.ends_with(".tar.gz") || name.ends_with(".tgz") || name.ends_with(".tar")
}

fn temp_path(stem: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("{stem}-{}-{nanos}", std::process::id()))
}

fn pick_local_port(preferred: Option<u16>) -> Result<u16> {
    if let Some(port) = preferred
        && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    {
        return Ok(port);
    }
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).context("bind ephemeral port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn wait_local_port(port: u16, child: &mut Child) -> Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("SSH tunnel exited before 127.0.0.1:{port} was accepting ({status})");
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for SSH tunnel on 127.0.0.1:{port}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn scratch() -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("fresh-gui-ssh-{}-{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_exe(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn target(dir: &Path) -> SshTarget {
        SshTarget {
            name: "lab".into(),
            destination: "ada@lab".into(),
            port: Some(2222),
            identity: Some(dir.join("id").display().to_string()),
            remote_root: Some("/work/proj".into()),
            local_port: None,
        }
    }

    #[cfg(unix)]
    fn listen_snippet() -> &'static str {
        r#"
if [[ "$*" == *"-N"* ]]; then
  port=""
  for a in "$@"; do
    case "$a" in
      127.0.0.1:*)
        port="${a#127.0.0.1:}"
        port="${port%%:*}"
        ;;
    esac
  done
  export FRESH_GUI_TEST_PORT="$port"
  exec python3 -c 'import os,socket,time
p=int(os.environ["FRESH_GUI_TEST_PORT"])
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", p)); s.listen(1); time.sleep(30)'
fi
"#
    }

    #[test]
    fn source_precedence_is_env_then_file_then_github() {
        let file = RemotesFile {
            daemon_path: Some("/file".into()),
            daemon_url: Some("https://file.example/a.tar.gz".into()),
            ..RemotesFile::default()
        };
        let from_env = DaemonSource::resolve(&file, Some("/env"), Some("https://env"));
        assert_eq!(from_env.path, Some(PathBuf::from("/env")));
        let from_url = DaemonSource::resolve(&file, Some("  "), Some("https://env"));
        assert_eq!(from_url.url.as_deref(), Some("https://env"));
        let from_file = DaemonSource::resolve(&file, None, None);
        assert_eq!(from_file.path, Some(PathBuf::from("/file")));
        let github = DaemonSource::resolve(&RemotesFile::default(), None, None);
        assert!(github.path.is_none() && github.url.is_none());
    }

    #[test]
    fn compares_calver_components_numerically() {
        assert_eq!(compare_versions("2026.923.10", "2026.923.2").unwrap(), std::cmp::Ordering::Greater);
        assert_eq!(compare_versions("2026.923.2", "2026.923.2").unwrap(), std::cmp::Ordering::Equal);
        assert_eq!(compare_versions("2026.922.9", "2026.923.2").unwrap(), std::cmp::Ordering::Less);
        assert!(compare_versions("2026.923.2", "hello").is_err());
        assert!(release_daemon_url("2026.923.2", "musl").ends_with("fresh-gui-2026.923.2-x86_64-unknown-linux-musl.tar.gz"));
    }

    #[test]
    fn start_command_quotes_root_and_uses_home_bin() {
        assert_eq!(
            start_command(INSTALLED_BIN, Some("/work/o'brien")),
            "env FRESH_GUI_QUIET=1 \"$HOME/.local/bin/fresh-gui\" --no-ui --json --root '/work/o'\\''brien'"
        );
        assert_eq!(
            start_command("/opt/bin/fresh-gui", None),
            "env FRESH_GUI_QUIET=1 '/opt/bin/fresh-gui' --no-ui --json"
        );
    }

    #[test]
    fn ssh_argv_is_batch_mode_and_splits_destination() {
        let dir = scratch();
        let t = target(&dir);
        let args = ssh_command_args(&t, "echo hi");
        assert!(args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(args.windows(2).any(|w| w == ["-p", "2222"]));
        assert_eq!(args[args.len() - 2], "ada@lab");
        assert_eq!(args.last().unwrap(), "sh -c 'echo hi'");
        let quoted = ssh_command_args(&t, "echo 'hi'");
        assert_eq!(quoted.last().unwrap(), "sh -c 'echo '\\''hi'\\'''");
        assert!(!args.iter().any(|a| a.contains("token")));
        let scp = scp_args(&t, Path::new("/tmp/fresh-gui"));
        assert!(scp.windows(2).any(|w| w == ["-P", "2222"]));
        assert!(scp.iter().any(|a| a == "ada@lab:.local/bin/fresh-gui.partial"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_bin_fresh_gui_from_release_layout() {
        let dir = scratch();
        let stage = dir.join("fresh-gui-1-x86_64-unknown-linux-gnu");
        fs::create_dir_all(stage.join("bin")).unwrap();
        fs::write(stage.join("bin/fresh-gui"), b"#!/bin/sh\necho daemon\n").unwrap();
        let archive = dir.join("daemon.tar.gz");
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&dir)
            .arg("fresh-gui-1-x86_64-unknown-linux-gnu")
            .status()
            .unwrap();
        assert!(status.success());
        let dest = dir.join("out-bin");
        extract_daemon_binary(Path::new("tar"), &archive, &dest).unwrap();
        let body = fs::read(&dest).unwrap();
        assert!(body.starts_with(b"#!/bin/sh\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_installs_when_probe_says_missing() {
        let dir = scratch();
        fs::write(dir.join("id"), b"key").unwrap();
        let daemon = dir.join("linux-fresh-gui");
        fs::write(&daemon, b"elf").unwrap();
        let ssh_log = dir.join("ssh.log");
        let scp_log = dir.join("scp.log");
        let ssh = dir.join("ssh");
        let count = dir.join("n");
        let mut script = format!(
            "#!/bin/bash\nprintf '%s\\n' \"$*\" >> {}\n",
            ssh_log.display()
        );
        script.push_str(listen_snippet());
        script.push_str(
            "\nif [[ \"$*\" == *\"sh -s\"* ]]; then\n  cat >/dev/null\n  n=0\n  count_file=\"",
        );
        script.push_str(&count.display().to_string());
        script.push_str("\"\n  if [[ -f \"$count_file\" ]]; then n=$(cat \"$count_file\"); fi\n");
        script.push_str("  n=$((n + 1))\n  echo \"$n\" > \"$count_file\"\n");
        script.push_str("  if [[ \"$n\" == \"1\" ]]; then\n");
        script.push_str(
            "    echo 'FRESH_GUI_PROBE v=1 installed=0 running=0 port= token= version= binary='\n",
        );
        script.push_str("  else\n");
        script.push_str(&format!("    echo 'FRESH_GUI_PROBE v=1 installed=1 running=1 port=7420 token=sekret version={} binary=/home/u/.local/bin/fresh-gui'\n", env!("CARGO_PKG_VERSION")));
        script.push_str("  fi\n  exit 0\nfi\nexit 0\n");
        write_exe(&ssh, &script);
        write_exe(
            &dir.join("scp"),
            &format!(
                "#!/bin/bash\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n",
                scp_log.display()
            ),
        );
        let tools = Toolchain {
            ssh: ssh.clone(),
            scp: dir.join("scp"),
            curl: PathBuf::from("/bin/false"),
            tar: PathBuf::from("/bin/false"),
        };
        let mut notes = Vec::new();
        let session = bootstrap(
            &target(&dir),
            &DaemonSource {
                path: Some(daemon.clone()),
                url: None,
            },
            &tools,
            |m| notes.push(m.to_string()),
        )
        .unwrap();
        assert!(notes.iter().any(|n| n.contains("not installed")));
        assert_eq!(session.token.as_deref(), Some("sekret"));
        assert_eq!(session.remote_port, 7420);
        assert!(session.ws_url.starts_with("ws://127.0.0.1:"));
        assert!(session.ws_url.ends_with("/ws"));
        let ssh_text = fs::read_to_string(&ssh_log).unwrap();
        assert!(ssh_text.contains("mkdir -p"));
        assert!(ssh_text.contains("--no-ui"));
        assert!(ssh_text.contains("--root"));
        assert!(ssh_text.contains("-N"));
        assert!(ssh_text.contains("127.0.0.1:"));
        let scp_text = fs::read_to_string(&scp_log).unwrap();
        assert!(scp_text.contains(".local/bin/fresh-gui.partial"));
        assert!(scp_text.contains(&daemon.display().to_string()));
        let debug = format!("{session:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("sekret"));
        drop(session);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_reuses_running_session_without_scp() {
        let dir = scratch();
        fs::write(dir.join("id"), b"key").unwrap();
        let ssh_log = dir.join("ssh.log");
        let scp_log = dir.join("scp.log");
        write_exe(
            &dir.join("ssh"),
            &format!(
                r#"#!/bin/bash
printf '%s\n' "$*" >> {log}
{listen}
if [[ "$*" == *"sh -s"* ]]; then
  cat >/dev/null
  echo 'FRESH_GUI_PROBE v=1 installed=1 running=1 port=7421 token=sekret version={version} binary=/usr/bin/fresh-gui'
  exit 0
fi
echo "unexpected ssh: $*" >&2
exit 1
"#,
                log = ssh_log.display(),
                listen = listen_snippet(),
                version = env!("CARGO_PKG_VERSION"),
            ),
        );
        write_exe(
            &dir.join("scp"),
            &format!(
                "#!/bin/bash\nprintf '%s\\n' \"$*\" >> {}\necho scp-should-not-run >&2\nexit 1\n",
                scp_log.display()
            ),
        );
        let tools = Toolchain {
            ssh: dir.join("ssh"),
            scp: dir.join("scp"),
            curl: PathBuf::from("/bin/false"),
            tar: PathBuf::from("/bin/false"),
        };
        let session = bootstrap(&target(&dir), &DaemonSource::default(), &tools, |_| {}).unwrap();
        assert_eq!(session.remote_port, 7421);
        assert!(!scp_log.exists());
        let ssh_text = fs::read_to_string(&ssh_log).unwrap();
        assert!(!ssh_text.contains("--no-ui"));
        assert!(ssh_text.contains("-N"));
        drop(session);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn start_failure_reports_daemon_log_after_ssh_banner() {
        let dir = scratch();
        fs::write(dir.join("id"), b"key").unwrap();
        let ssh = dir.join("ssh");
        write_exe(&ssh, &format!(
            r#"#!/bin/bash
echo 'Welcome to the server' >&2
if [[ "$*" == *"sh -s"* ]]; then
  cat >/dev/null
  echo 'Welcome to the server'
  echo 'FRESH_GUI_PROBE v=1 installed=1 running=0 port= token= version={version} binary=/home/u/.local/bin/fresh-gui'
elif [[ "$*" == *"FRESH_GUI_LOG_BEGIN"* ]]; then
  printf '\nFRESH_GUI_LOG_BEGIN\nfailed to bind socket: address in use\nFRESH_GUI_LOG_END\n'
elif [[ "$*" == *"--no-ui"* ]]; then
  exit 1
fi
"#,
            version = env!("CARGO_PKG_VERSION")
        ));
        let tools = Toolchain {
            ssh,
            scp: PathBuf::from("/bin/false"),
            curl: PathBuf::from("/bin/false"),
            tar: PathBuf::from("/bin/false"),
        };
        let err = bootstrap(&target(&dir), &DaemonSource::default(), &tools, |_| {}).err().unwrap();
        let message = format!("{err:#}");
        assert!(message.contains("failed to bind socket: address in use"), "{message}");
        assert!(message.contains("remote daemon log"), "{message}");
        let _ = fs::remove_dir_all(&dir);
    }
}
