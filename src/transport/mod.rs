//! Transport layer.
//!
//! Two client transports carry the sync protocol to the remote:
//!
//! - [`ssh`] — the system `ssh` process (rsync's model, and the default on
//!   Unix): battle-tested, honors the user's ssh config, agent, GSSAPI, and
//!   FIDO security keys, and multiplexes a run's sequential sessions over one
//!   master connection (`ControlMaster=auto`).
//! - [`russh`] — a pure-Rust SSH client (the default on Windows, where
//!   OpenSSH's `ControlMaster` multiplexing is broken; also the transport
//!   mobile embeddings will use). One connection, one authentication, one
//!   channel per session — RFC 4254 channel reuse, no `ControlMaster`.
//!
//! [`Transport::default_transport`] picks per platform; the rest of cp2 goes
//! through the [`Transport`] methods, so a run never knows which one it rides.

pub mod ssh;
#[cfg(target_os = "windows")]
pub mod russh;

pub use ssh::spawn_ssh;

use crate::target::RemoteTarget;
use crate::Result;
#[cfg(target_os = "windows")]
use crate::Error;
use anyhow::bail;
use zeroize::Zeroize;
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};

/// A jump host (`--jump-host user@host[:port]`): the russh transport connects
/// to it first and tunnels the target connection through a `direct-tcpip`
/// channel (OpenSSH `ProxyJump` semantics). System ssh honors `ProxyJump`
/// from the user's ssh config instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpHost {
    /// Account on the jump host.
    pub user: String,
    /// Hostname or IP of the jump host.
    pub host: String,
    /// SSH port of the jump host (default 22).
    pub port: u16,
}

impl JumpHost {
    /// Parse `user@host[:port]`, where `host` may be a bracketed IPv6 literal
    /// (`user@[::1]` / `user@[::1]:port`). A numeric suffix after the last
    /// `:` (or the bracketed tail) is the port — the one deliberate exception
    /// to the "no port in the target string" rule (the sync targets keep
    /// rsync semantics). An unbracketed host may not itself contain `:`.
    ///
    /// # Errors
    ///
    /// Returns an error when the string is not `user@host[:port]`, the host
    /// contains a `:` outside brackets, or the port is malformed or zero.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let (user, host_port) = s
            .split_once('@')
            .ok_or_else(|| anyhow::anyhow!("--jump-host must be user@host, got '{s}'"))?;
        if user.is_empty() {
            bail!("--jump-host must be user@host, got '{s}'");
        }

        let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
            // Bracketed IPv6 literal: `[::1]` or `[::1]:port`. The brackets
            // are stripped — `connect` needs the bare address, not `[::1]`.
            let (host, tail) = rest
                .split_once(']')
                .ok_or_else(|| anyhow::anyhow!("--jump-host must be user@host, got '{s}'"))?;
            let port = match tail.strip_prefix(':') {
                Some(port) => parse_port(port, s)?,
                None if tail.is_empty() => 22,
                None => bail!("--jump-host must be user@host, got '{s}'"),
            };
            (host, port)
        } else {
            // Unbracketed: `host` or `host:port`. The host part may not
            // itself contain ':' — `alice@::1` and `alice@host:abc` are
            // rejected outright rather than absorbed into the host.
            let (host, port) = match host_port.rsplit_once(':') {
                Some((host, port))
                    if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    (host, Some(parse_port(port, s)?))
                }
                _ => (host_port, None),
            };
            if host.contains(':') {
                bail!("--jump-host host may not contain ':' outside brackets, got '{s}'");
            }
            (host, port.unwrap_or(22))
        };

        if host.is_empty() {
            bail!("--jump-host must be user@host, got '{s}'");
        }
        Ok(Self {
            user: user.to_string(),
            host: host.to_string(),
            port,
        })
    }
}

/// Parse a `--jump-host` port suffix: digits that fit `u16`, and non-zero.
fn parse_port(s: &str, original: &str) -> anyhow::Result<u16> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        bail!("--jump-host must be user@host[:port], got '{original}'");
    }
    let port = s
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("invalid port in --jump-host '{original}'"))?;
    if port == 0 {
        bail!("--jump-host port must be > 0, got '{original}'");
    }
    Ok(port)
}

/// The client transport for a run. The [`Self::Russh`] variant exists only
/// on Windows, where the russh backend is compiled in (`cfg(windows)`), so
/// match arms on it carry the same cfg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// The system `ssh` process (the Unix default).
    Ssh,
    /// The pure-Rust russh client (the Windows default).
    #[cfg(target_os = "windows")]
    Russh,
}

impl Transport {
    /// The transport selected by default on this platform: russh on Windows
    /// (OpenSSH's `ControlMaster` multiplexing is unusable there —
    /// `getsockname failed: Not a socket`), system ssh elsewhere (the only
    /// Unix transport). The `CP2_TRANSPORT=ssh|russh` environment variable
    /// overrides the default (useful for testing and for pinning a transport;
    /// on Unix only `ssh` is available).
    #[must_use]
    pub fn default_transport() -> Self {
        if let Ok(value) = std::env::var("CP2_TRANSPORT") {
            match value.as_str() {
                "ssh" => return Self::Ssh,
                #[cfg(target_os = "windows")]
                "russh" => return Self::Russh,
                _ => {
                    eprintln!(
                        "warning: unknown CP2_TRANSPORT '{value}' (expected 'ssh' or 'russh'); \
                         using the platform default"
                    );
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            Self::Ssh
        }
        #[cfg(target_os = "windows")]
        {
            Self::Russh
        }
    }

    /// A short name for diagnostics.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Ssh => "ssh",
            #[cfg(target_os = "windows")]
            Self::Russh => "russh",
        }
    }

}

/// A run-scoped transport client: the operations of one run (platform probe,
/// version check, deploy, sync session) share one object, so the russh
/// transport authenticates **once per run** (one connection, one channel per
/// operation — RFC 4254 channel reuse) instead of once per operation. The
/// system-ssh transport is stateless here; its `ControlMaster` multiplexing
/// already gives one password prompt per run.
/// How the remote `cp2 --server` is invoked, decided by the version probe
/// under `--remote-sudo`: `NonInteractive` when a NOPASSWD sudoers rule
/// covers the remote path, `Password` when sudo needs the client's password
/// (injected as the first stdin line via `sudo -S` — `--password` and the
/// sudo password are the same login password in practice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sudo {
    /// No sudo: the server runs as the SSH connection user (0-Root).
    #[default]
    None,
    /// `sudo -n` — a NOPASSWD sudoers rule must cover the remote path.
    NonInteractive,
    /// `sudo -S` — the client's password is the first line of stdin.
    Password,
}

/// The outcome of a version probe under `--remote-sudo`.
pub struct VersionProbe {
    /// The remote version+protocol, when the probe could read it.
    pub version: Option<(String, Option<String>)>,
    /// sudo was requested and needs a password (the binary exists; no
    /// password was available to complete the probe).
    pub sudo_password_required: bool,
    /// The sudo invocation the probe resolved to.
    pub mode: Sudo,
}

pub struct RemoteClient {
    transport: Transport,
    /// `--password` values for the target and the jump host, moved into the
    /// russh connection (or the first system-ssh spawn) and scrubbed from
    /// memory as soon as they have been emitted; zeroized on drop if never
    /// emitted.
    password: Option<String>,
    jump_password: Option<String>,
    /// A run-unique ssh `ControlPath` (fresh master) when a `--password` is
    /// in play, so the pty injection is guaranteed to see a prompt.
    control_path: Option<String>,
    /// `--remote-sudo`: run the remote `cp2 --server` under sudo, so the
    /// destination can carry the source's owner/group and devices (`-a`
    /// full fidelity). The destination files are then owned by root — keep
    /// using the flag on every run.
    remote_sudo: bool,
    /// The password injected into `sudo -S`: the `--sudo-password` value, or
    /// the `--password` value (the same login password in practice). Kept
    /// separate from `password` so ssh-auth consumption does not eat it.
    sudo_password: Option<String>,
    /// The sudo invocation resolved by the version probe (None until then).
    sudo_mode: Sudo,
    /// The run's authenticated russh connection, created on first use and
    /// shared by every operation (on Windows it also serves as the fallback
    /// when a system-ssh spawn fails and the operation retries over russh).
    #[cfg(target_os = "windows")]
    russh: Option<russh::RusshConnection>,
    /// A failed connection attempt is cached so a run never re-prompts or
    /// re-dials after an auth/network failure — the first error surfaces
    /// once and the run aborts.
    #[cfg(target_os = "windows")]
    russh_err: Option<String>,
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        // Best-effort cleanup of this run's ssh control sockets (a run-unique
        // `cp2-ssh-%C-<pid>` ControlPath only exists when a password is in
        // play). The OpenSSH master owns the socket and re-creates it per
        // run, so removing the file is safe; without this, normal runs leave
        // it behind until the ControlPersist master exits.
        if self.control_path.is_some() {
            remove_control_sockets();
        }
        for secret in [
            &mut self.password,
            &mut self.jump_password,
            &mut self.sudo_password,
        ]
        .into_iter()
        .flatten()
        {
            secret.zeroize();
        }
    }
}

/// Remove this run's ssh control sockets from the temp dir. The `ControlPath`
/// embeds ssh's `%C` connection hash, which only OpenSSH expands — the literal
/// path is never the socket file itself — so match `cp2-ssh-*-<pid>` instead.
fn remove_control_sockets() {
    let pid = std::process::id();
    let suffix = format!("-{pid}");
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("cp2-ssh-") && name.ends_with(&suffix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

impl RemoteClient {
    /// Create a client for the given transport selection, optionally seeding
    /// the authentication with a `--password` value.
    #[must_use]
    pub fn new(
        transport: Transport,
        password: Option<String>,
        jump_password: Option<String>,
        remote_sudo: bool,
        sudo_password: Option<String>,
    ) -> Self {
        // A run-unique ControlPath forces a fresh master, so the pty
        // password injection always sees a prompt (a reused master would
        // prompt nothing and deadlock the injector).
        let control_path = password.as_ref().map(|_| {
            format!(
                "{}/cp2-ssh-%C-{}",
                std::env::temp_dir().to_string_lossy().replace('\\', "/"),
                std::process::id()
            )
        });
        // The effective sudo password: an explicit `--sudo-password`, else
        // the `--password` value (the same login password in practice).
        let sudo_password = if remote_sudo {
            sudo_password.or_else(|| password.clone())
        } else {
            None
        };
        Self {
            transport,
            password,
            jump_password,
            sudo_password,
            remote_sudo,
            sudo_mode: Sudo::None,
            control_path,
            #[cfg(target_os = "windows")]
            russh: None,
            #[cfg(target_os = "windows")]
            russh_err: None,
        }
    }

    /// The system-ssh spawn options for the current op: the password is
    /// present only until the first (master-creating) spawn has emitted it;
    /// the unique `ControlPath` applies to every spawn of the run.
    fn ssh_auth(&self) -> ssh::SshAuth<'_> {
        ssh::SshAuth {
            password: self.password.as_deref(),
            jump_password: self.jump_password.as_deref(),
            control_path: self.control_path.as_deref(),
        }
    }

    /// The passwords have been handed to a system-ssh spawn: scrub them now.
    fn consume_ssh_password(&mut self) {
        for secret in [&mut self.password, &mut self.jump_password] {
            if let Some(mut secret) = secret.take() {
                secret.zeroize();
            }
        }
    }

    /// On Windows a pending `--password`/`--jump-password` — which the
    /// system-ssh pty path cannot consume — routes the operation to russh, as
    /// does an already-established russh connection (reused across the run).
    /// Returns `true` when the operation should run on russh, warning once
    /// that an explicit `Transport::Ssh` selection is overridden.
    #[cfg(target_os = "windows")]
    fn forced_to_russh(&self) -> bool {
        if self.password.is_some() || self.jump_password.is_some() {
            tracing::warn!(
                "the ssh transport cannot consume passwords on Windows; using the russh transport"
            );
            true
        } else {
            self.russh.is_some()
        }
    }

    /// Probe the remote platform (`uname` / `cmd`), if it can be determined.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot reach, authenticate to,
    /// or probe the remote.
    pub async fn probe(
        &mut self,
        peer: &RemoteTarget,
        jump: Option<&JumpHost>,
    ) -> Result<Option<(String, String)>> {
        #[cfg(not(target_os = "windows"))]
        let _ = jump;
        match self.transport {
            Transport::Ssh => {
                #[cfg(target_os = "windows")]
                {
                    if self.forced_to_russh() {
                        return self.russh_probe(peer, jump).await;
                    }
                }
                let auth = self.ssh_auth();
                let result = ssh::check_remote_platform(peer, &auth).await;
                self.consume_ssh_password();
                match result {
                    Ok(value) => Ok(value),
                    Err(e) => {
                        #[cfg(target_os = "windows")]
                        {
                            tracing::warn!("system ssh failed ({e}); retrying with the russh transport");
                            self.russh_probe(peer, jump).await
                        }
                        #[cfg(not(target_os = "windows"))]
                        {
                            Err(e)
                        }
                    }
                }
            }
            #[cfg(target_os = "windows")]
            Transport::Russh => self.russh_probe(peer, jump).await,
        }
    }

    /// Check the remote binary's version and protocol.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot reach or authenticate to
    /// the remote.
    pub async fn check_version(
        &mut self,
        peer: &RemoteTarget,
        remote_path: &str,
        remote_os: &str,
        jump: Option<&JumpHost>,
    ) -> Result<Option<(String, Option<String>)>> {
        #[cfg(not(target_os = "windows"))]
        let _ = jump;
        match self.transport {
            Transport::Ssh => {
                #[cfg(target_os = "windows")]
                {
                    if self.forced_to_russh() {
                        return self.russh_check_version(peer, remote_path, remote_os, jump).await;
                    }
                }
                let auth = self.ssh_auth();
                let sudo = if self.remote_sudo {
                    Sudo::NonInteractive
                } else {
                    Sudo::None
                };
                let result = ssh::check_remote_version(
                    peer,
                    remote_path,
                    remote_os,
                    &auth,
                    sudo,
                    self.sudo_password.as_deref(),
                )
                .await;
                self.consume_ssh_password();
                match result {
                    Ok(probe) => self.apply_version_probe(probe),
                    Err(e) => {
                        #[cfg(target_os = "windows")]
                        {
                            tracing::warn!("system ssh failed ({e}); retrying with the russh transport");
                            self.russh_check_version(peer, remote_path, remote_os, jump).await
                        }
                        #[cfg(not(target_os = "windows"))]
                        {
                            Err(e)
                        }
                    }
                }
            }
            #[cfg(target_os = "windows")]
            Transport::Russh => self.russh_check_version(peer, remote_path, remote_os, jump).await,
        }
    }

    /// Fold a version probe result into the run state: remember the sudo
    /// invocation the probe resolved to, and surface a missing sudo password
    /// as a hard error (the sync must not silently drop `--remote-sudo`).
    fn apply_version_probe(
        &mut self,
        probe: VersionProbe,
    ) -> Result<Option<(String, Option<String>)>> {
        self.sudo_mode = probe.mode;
        if probe.sudo_password_required {
            return Err(crate::Error::Other(
                "sudo on the remote needs a password: pass --sudo-password (or --password,                  reused for sudo — they are the same login password in practice), or configure                  a NOPASSWD sudoers rule covering the remote path (e.g.                  `user ALL=(root) NOPASSWD: <remote-path> *`)"
                    .to_string(),
            ));
        }
        Ok(probe.version)
    }

    /// Probe the remote platform and the remote binary's version in one ssh
    /// session when the transport supports the combined probe (Unix
    /// system-ssh), falling back to the two-session path otherwise. Returns
    /// `(platform, version)`.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot reach or authenticate to
    /// the remote, or a sudo password is required but missing.
    pub async fn probe_and_version(
        &mut self,
        peer: &RemoteTarget,
        remote_path: &str,
        jump: Option<&JumpHost>,
    ) -> Result<(Option<(String, String)>, Option<(String, Option<String>)>)> {
        #[cfg(not(target_os = "windows"))]
        let _ = jump;
        match self.transport {
            Transport::Ssh => {
                #[cfg(target_os = "windows")]
                {
                    if self.forced_to_russh() {
                        let platform = self.russh_probe(peer, jump).await?;
                        let version = self
                            .russh_check_version(peer, remote_path, "unix", jump)
                            .await?;
                        return Ok((platform, version));
                    }
                }
                let auth = self.ssh_auth();
                let sudo = if self.remote_sudo {
                    Sudo::NonInteractive
                } else {
                    Sudo::None
                };
                let result = ssh::check_remote_platform_and_version(
                    peer,
                    remote_path,
                    &auth,
                    sudo,
                    self.sudo_password.as_deref(),
                )
                .await;
                self.consume_ssh_password();
                match result {
                    Ok((Some(platform), probe)) => {
                        let version = self.apply_version_probe(probe)?;
                        Ok((Some(platform), version))
                    }
                    Ok((None, _)) => {
                        // The compound command does not parse on this remote
                        // (Windows sshd, unusual shell): the two-session path.
                        let platform = self.probe(peer, jump).await?;
                        let version = self.check_version(peer, remote_path, "unix", jump).await?;
                        Ok((platform, version))
                    }
                    Err(e) => Err(e),
                }
            }
            #[cfg(target_os = "windows")]
            Transport::Russh => {
                let platform = self.russh_probe(peer, jump).await?;
                let version = self.russh_check_version(peer, remote_path, "unix", jump).await?;
                Ok((platform, version))
            }
        }
    }

    /// Push the local binary to the remote (deploy).
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot reach or authenticate to
    /// the remote, or the deployment fails.
    pub async fn deploy(
        &mut self,
        peer: &RemoteTarget,
        remote_path: &str,
        local_binary: &Path,
        remote_os: &str,
        jump: Option<&JumpHost>,
    ) -> Result<()> {
        #[cfg(not(target_os = "windows"))]
        let _ = jump;
        match self.transport {
            Transport::Ssh => {
                #[cfg(target_os = "windows")]
                {
                    if self.forced_to_russh() {
                        return self
                            .russh_deploy(peer, remote_path, local_binary, remote_os, jump)
                            .await;
                    }
                }
                let auth = self.ssh_auth();
                let result =
                    ssh::push_remote_binary(peer, remote_path, local_binary, remote_os, &auth).await;
                self.consume_ssh_password();
                match result {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        #[cfg(target_os = "windows")]
                        {
                            tracing::warn!("system ssh failed ({e}); retrying with the russh transport");
                            self.russh_deploy(peer, remote_path, local_binary, remote_os, jump).await
                        }
                        #[cfg(not(target_os = "windows"))]
                        {
                            Err(e)
                        }
                    }
                }
            }
            #[cfg(target_os = "windows")]
            Transport::Russh => {
                self.russh_deploy(peer, remote_path, local_binary, remote_os, jump)
                    .await
            }
        }
    }

    /// Open the long-lived sync session: the executor's byte-stream halves
    /// plus a handle that waits for the transport to finish cleanly.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport cannot reach, authenticate to,
    /// or open a session channel on the remote.
    pub async fn open_session(
        &mut self,
        peer: &RemoteTarget,
        remote_path: &str,
        remote_os: &str,
        server_args: &str,
        jump: Option<&JumpHost>,
    ) -> Result<Session> {
        #[cfg(not(target_os = "windows"))]
        let _ = jump;
        if self.remote_sudo {
            if remote_os == "windows" {
                tracing::warn!("--remote-sudo is ignored on a Windows remote");
                self.sudo_mode = Sudo::None;
            } else if self.sudo_mode == Sudo::None {
                // Never probed (e.g. `--no-auto-install` skips the version
                // check): assume a NOPASSWD rule — without a probe result we
                // must not risk the `sudo -S` prelude on a NOPASSWD sudo.
                self.sudo_mode = Sudo::NonInteractive;
            }
        }
        match self.transport {
            Transport::Ssh => {
                #[cfg(target_os = "windows")]
                {
                    if self.forced_to_russh() {
                        return self
                            .russh_session(peer, remote_path, remote_os, server_args, jump)
                            .await;
                    }
                }
                let auth = self.ssh_auth();
                let result = ssh_session(
                    peer,
                    remote_path,
                    remote_os,
                    server_args,
                    &auth,
                    self.sudo_mode,
                    self.sudo_password.as_deref(),
                )
                .await;
                self.consume_ssh_password();
                match result {
                    Ok(session) => Ok(session),
                    Err(e) => {
                        #[cfg(target_os = "windows")]
                        {
                            tracing::warn!("system ssh failed ({e}); retrying with the russh transport");
                            self.russh_session(
                                peer,
                                remote_path,
                                remote_os,
                                server_args,
                                jump,
                            )
                            .await
                        }
                        #[cfg(not(target_os = "windows"))]
                        {
                            Err(e)
                        }
                    }
                }
            }
            #[cfg(target_os = "windows")]
            Transport::Russh => {
                self.russh_session(peer, remote_path, remote_os, server_args, jump)
                    .await
            }
        }
    }

    /// Open the sync session with the platform preamble merged in-band
    /// (rsync-style single session): the remote command prints `uname -s -m`
    /// before exec'ing the server, so the sync needs no separate probe
    /// session — the common Unix push/pull run drops from two ssh sessions
    /// to one.
    ///
    /// Returns `Ok(None)` when the merged flow does not apply — a password
    /// needs the pty master session first, `--remote-sudo`'s mode is
    /// probe-discovered, the transport is russh, or the remote does not
    /// speak the preamble (a Windows sshd, an unusual login shell). The
    /// caller then falls back to the classic probe+sync flow. The session is
    /// killed before `None` is returned, so a fallback never leaks a stray
    /// connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the ssh spawn or the preamble read fails.
    pub async fn open_preamble_session(
        &mut self,
        peer: &RemoteTarget,
        remote_path: &str,
        server_args: &str,
        jump: Option<&JumpHost>,
    ) -> Result<Option<PreambleSession>> {
        #[cfg(not(target_os = "windows"))]
        let _ = jump;
        // A password must ride the pty master creation (the first spawn);
        // sudo's `-n`/`-S` policy is probe-discovered. Both keep the classic
        // two-session flow.
        if self.remote_sudo || self.password.is_some() || self.jump_password.is_some() {
            return Ok(None);
        }
        match self.transport {
            Transport::Ssh => {
                let auth = self.ssh_auth();
                let spawned = ssh::spawn_ssh_preamble(peer, remote_path, server_args, &auth);
                self.consume_ssh_password();
                let ssh = match spawned {
                    Ok(ssh) => ssh,
                    Err(e) => return Err(e),
                };
                let ssh::SshChild {
                    mut child,
                    stdin,
                    stdout,
                } = ssh;
                match ssh::read_preamble_platform(stdout).await {
                    Ok(Some((os, arch, reader))) => Ok(Some(PreambleSession {
                        os,
                        arch,
                        session: Session {
                            send: Box::new(stdin),
                            recv: Box::new(reader),
                            handle: SessionHandle::Ssh(child),
                        },
                    })),
                    Ok(None) => {
                        // The remote does not speak the preamble — kill the
                        // session and let the caller use the classic flow.
                        let _ = child.kill().await;
                        Ok(None)
                    }
                    Err(e) => Err(e.into()),
                }
            }
            #[cfg(target_os = "windows")]
            Transport::Russh => {
                let connection = self.ensure_russh(peer, jump).await?;
                match russh::open_preamble_on(&connection.handle, remote_path, server_args).await {
                    Ok(Some((os, arch, send, recv, session))) => Ok(Some(PreambleSession {
                        os,
                        arch,
                        session: Session {
                            send,
                            recv,
                            handle: SessionHandle::Russh(session),
                        },
                    })),
                    Ok(None) => Ok(None),
                    Err(e) => Err(Error::Other(format!("russh transport: {e}"))),
                }
            }
        }
    }

    /// The merged deploy-and-serve open (the single-session flow's deploy
    /// retry): stream the matching binary to the remote and serve the sync
    /// on the same session — the deploy session *is* the sync session, and
    /// the Hello handshake verifies the deployed binary (the separate
    /// post-deploy version check is gone). POSIX, no sudo, no password —
    /// the same conditions as [`Self::open_preamble_session`]; the other
    /// paths keep the classic two-phase deploy.
    ///
    /// # Errors
    ///
    /// Returns an error when the ssh spawn or the payload write fails.
    pub async fn deploy_and_open_session(
        &mut self,
        peer: &RemoteTarget,
        remote_path: &str,
        server_args: &str,
        local_binary: &Path,
        jump: Option<&JumpHost>,
    ) -> Result<Session> {
        #[cfg(not(target_os = "windows"))]
        let _ = jump;
        match self.transport {
            Transport::Ssh => {
                let auth = self.ssh_auth();
                let spawned = ssh::push_remote_binary_and_serve(
                    peer,
                    remote_path,
                    server_args,
                    local_binary,
                    &auth,
                )
                .await;
                self.consume_ssh_password();
                let (child, stdin, stdout) = match spawned {
                    Ok(parts) => parts,
                    Err(e) => return Err(e),
                };
                Ok(Session {
                    send: Box::new(stdin),
                    recv: Box::new(stdout),
                    handle: SessionHandle::Ssh(child),
                })
            }
            #[cfg(target_os = "windows")]
            Transport::Russh => {
                let connection = self.ensure_russh(peer, jump).await?;
                match russh::deploy_and_serve_on(&connection.handle, remote_path, server_args, local_binary)
                    .await
                {
                    Ok((send, recv, session)) => Ok(Session {
                        send,
                        recv,
                        handle: SessionHandle::Russh(session),
                    }),
                    Err(e) => Err(Error::Other(format!("russh transport: {e}"))),
                }
            }
        }
    }

    /// The russh connection for the run, connecting and authenticating once
    /// on first use. A failed attempt is cached so the run reports the same
    /// error once instead of re-prompting per operation.
    #[cfg(target_os = "windows")]
    async fn ensure_russh(
        &mut self,
        peer: &RemoteTarget,
        jump: Option<&JumpHost>,
    ) -> Result<&russh::RusshConnection> {
        if let Some(message) = &self.russh_err {
            return Err(Error::Other(message.clone()));
        }
        if self.russh.is_none() {
            match russh::connect(peer, jump, self.password.take(), self.jump_password.take()).await {
                Ok(connection) => self.russh = Some(connection),
                Err(e) => {
                    // Cache the message with the same `russh transport:`
                    // prefix `map_russh` applies, so the replay reads
                    // identically to the first failure.
                    let message = format!("russh transport: {e}");
                    self.russh_err = Some(message.clone());
                    return Err(Error::Other(message));
                }
            }
        }
        Ok(self.russh.as_ref().expect("connection set above"))
    }

    #[cfg(target_os = "windows")]
    async fn russh_probe(
        &mut self,
        peer: &RemoteTarget,
        jump: Option<&JumpHost>,
    ) -> Result<Option<(String, String)>> {
        let connection = self.ensure_russh(peer, jump).await?;
        russh::probe_on(&connection.handle).await.map_err(map_russh)
    }

    #[cfg(target_os = "windows")]
    async fn russh_check_version(
        &mut self,
        peer: &RemoteTarget,
        remote_path: &str,
        remote_os: &str,
        jump: Option<&JumpHost>,
    ) -> Result<Option<(String, Option<String>)>> {
        // Copy the fields before the call: `connection` borrows `self` for
        // the rest of this scope.
        let sudo = if self.remote_sudo {
            Sudo::NonInteractive
        } else {
            Sudo::None
        };
        let sudo_password = self.sudo_password.clone();
        let connection = self.ensure_russh(peer, jump).await?;
        let probe = russh::check_version_on(
            &connection.handle,
            remote_path,
            remote_os,
            sudo,
            sudo_password.as_deref(),
        )
        .await
        .map_err(map_russh)?;
        // The russh probe returns the bare version tuple; wrap it in a
        // `VersionProbe` exactly as the system-ssh path does.
        self.apply_version_probe(VersionProbe {
            version: probe,
            sudo_password_required: false,
            mode: sudo,
        })
    }

    #[cfg(target_os = "windows")]
    async fn russh_deploy(
        &mut self,
        peer: &RemoteTarget,
        remote_path: &str,
        local_binary: &Path,
        remote_os: &str,
        jump: Option<&JumpHost>,
    ) -> Result<()> {
        let connection = self.ensure_russh(peer, jump).await?;
        russh::deploy_on(&connection.handle, remote_path, local_binary, remote_os)
            .await
            .map_err(map_russh)
    }

    #[cfg(target_os = "windows")]
    async fn russh_session(
        &mut self,
        peer: &RemoteTarget,
        remote_path: &str,
        remote_os: &str,
        server_args: &str,
        jump: Option<&JumpHost>,
    ) -> Result<Session> {
        // Copy the fields before the call: `connection` borrows `self` for
        // the rest of this scope.
        let sudo_mode = self.sudo_mode;
        let sudo_password = self.sudo_password.clone();
        let connection = self.ensure_russh(peer, jump).await?;
        let (send, recv, session) = russh::open_session_on(
            &connection.handle,
            remote_path,
            remote_os,
            server_args,
            sudo_mode,
            sudo_password.as_deref(),
        )
        .await
                .map_err(map_russh)?;
        Ok(Session {
            send,
            recv,
            handle: SessionHandle::Russh(session),
        })
    }
}

/// Spawn the sync session over the system `ssh` process.
async fn ssh_session(
    peer: &RemoteTarget,
    remote_path: &str,
    remote_os: &str,
    server_args: &str,
    auth: &ssh::SshAuth<'_>,
    sudo: Sudo,
    sudo_password: Option<&str>,
) -> Result<Session> {
    let ssh = ssh::spawn_ssh(
        peer,
        remote_path,
        remote_os,
        server_args,
        auth,
        sudo,
        sudo_password,
    )
    .await?;
    let (send, recv, child) = ssh.into_parts();
    Ok(Session {
        send,
        recv,
        handle: SessionHandle::Ssh(child),
    })
}

/// Map a russh-backend error into the crate error type.
#[cfg(target_os = "windows")]
#[expect(clippy::needless_pass_by_value, reason = "map_err passes the error by value")]
fn map_russh(e: anyhow::Error) -> Error {
    Error::Other(format!("russh transport: {e}"))
}

/// A live sync session over the chosen transport: the executor's byte-stream
/// halves plus a [`SessionHandle`] whose `finish` mirrors rsync's "wait for
/// the transport to exit cleanly" step.
pub struct Session {
    send: Box<dyn AsyncWrite + Unpin + Send>,
    recv: Box<dyn AsyncRead + Unpin + Send>,
    handle: SessionHandle,
}

impl Session {
    /// Split into the executor halves plus the transport handle.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Box<dyn AsyncWrite + Unpin + Send>,
        Box<dyn AsyncRead + Unpin + Send>,
        SessionHandle,
    ) {
        (self.send, self.recv, self.handle)
    }
}

/// The merged single-session open: the remote platform was read from the
/// in-band preamble, so the sync can run on this one session (rsync-style)
/// without a separate probe session.
pub struct PreambleSession {
    /// Normalized remote OS (`linux`/`macos`/`windows`), from `uname`.
    pub os: String,
    /// Normalized remote architecture (`x86_64`/`aarch64`).
    pub arch: String,
    /// The sync session, ready for the executor.
    pub session: Session,
}

/// Transport-specific session teardown state.
pub enum SessionHandle {
    /// The spawned `ssh` process.
    Ssh(tokio::process::Child),
    /// The russh connection plus its read-forwarding task.
    #[cfg(target_os = "windows")]
    Russh(russh::RusshSession),
}

impl SessionHandle {
    /// Whether the ssh child has exited with a non-zero status, waiting
    /// briefly for the exit (the stream error arrives a moment before the
    /// process is reaped). Distinguishes "the remote server never started"
    /// (missing or unexecutable binary — a deploy is the right recovery)
    /// from a live-but-failed stream.
    pub(crate) async fn child_exited_nonzero(&mut self) -> bool {
        match self {
            Self::Ssh(child) => {
                matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await,
                    Ok(Ok(status)) if !status.success()
                )
            }
            #[cfg(target_os = "windows")]
            Self::Russh(_) => false,
        }
    }

    /// Wait for the transport to finish. The transfer result is authoritative:
    /// a peer error (auth denied, host key rejected, remote failure) is
    /// returned unchanged — the stream error is more meaningful than the ssh
    /// exit status. Only when the transfer succeeded is the child's status
    /// checked: a non-zero exit is an error, and a signal death (Ctrl-C) is
    /// reported as a cancellation. The wait is bounded so a remote that never
    /// exits cannot hang the run.
    ///
    /// # Errors
    ///
    /// Returns an error when the transfer failed, when the ssh child exited
    /// non-zero or died on a signal, or when the wait timed out.
    pub async fn finish<T>(&mut self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        match self {
            Self::Ssh(child) => {
                // A peer error (auth denied, host key rejected, remote
                // failure) is more meaningful than the ssh exit status —
                // propagate it unchanged.
                let result = result?;
                let status = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    child.wait(),
                )
                .await
                .map_err(|_| anyhow::anyhow!("timed out waiting for the ssh process to exit"))?
                .map_err(|e| anyhow::anyhow!("failed to wait for the ssh process: {e}"))?;
                if !status.success() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        if let Some(signal) = status.signal() {
                            bail!("ssh exited with signal: {signal}");
                        }
                    }
                    bail!("ssh exited with status: {}", status.code().unwrap_or(-1));
                }
                Ok(result)
            }
            #[cfg(target_os = "windows")]
            Self::Russh(session) => session.finish(result).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jump_host_parse_forms() {
        let jh = JumpHost::parse("alice@bastion.example.com").unwrap();
        assert_eq!(jh.user, "alice");
        assert_eq!(jh.host, "bastion.example.com");
        assert_eq!(jh.port, 22);

        let jh = JumpHost::parse("alice@bastion.example.com:2222").unwrap();
        assert_eq!(jh.user, "alice");
        assert_eq!(jh.host, "bastion.example.com");
        assert_eq!(jh.port, 2222);

        // Missing user, missing host, and empty parts are rejected.
        assert!(JumpHost::parse("bastion.example.com").is_err());
        assert!(JumpHost::parse("@bastion.example.com").is_err());
        assert!(JumpHost::parse("alice@").is_err());
    }

    #[test]
    fn jump_host_parse_ipv6() {
        // Bracketed IPv6 literals: the brackets are stripped from the host,
        // and a port is recognized only from the bracketed tail.
        let jh = JumpHost::parse("alice@[::1]:2222").unwrap();
        assert_eq!(jh.user, "alice");
        assert_eq!(jh.host, "::1");
        assert_eq!(jh.port, 2222);

        let jh = JumpHost::parse("alice@[::1]").unwrap();
        assert_eq!(jh.host, "::1");
        assert_eq!(jh.port, 22);

        // An unbracketed host may not contain ':' — `::1` would otherwise be
        // misread as host `:` on port 1.
        assert!(JumpHost::parse("alice@::1").is_err());
    }

    #[test]
    fn jump_host_parse_rejects_bad_ports() {
        // Zero, empty, and non-numeric suffixes are rejected outright rather
        // than absorbed into the host.
        assert!(JumpHost::parse("alice@host:0").is_err());
        assert!(JumpHost::parse("alice@host:").is_err());
        assert!(JumpHost::parse("alice@host:abc").is_err());
        assert!(JumpHost::parse("alice@host:99999").is_err());
        assert!(JumpHost::parse("alice@[::1]:").is_err());
        assert!(JumpHost::parse("alice@[::1]:0").is_err());
    }
}
