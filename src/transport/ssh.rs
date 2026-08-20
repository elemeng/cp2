//! SSH transport: the system `ssh` client is the byte-stream carrier
//! (rsync's model). The remote end runs `cp2 --server`; authentication
//! (PAM on Linux/macOS, LogonUser/keys on Windows OpenSSH) and permission
//! enforcement (identity switching per user) are entirely sshd's — cp2
//! itself has no auth code.
//!
//! A run opens several sequential ssh sessions (platform probe, version
//! check, deploy, sync); on Unix they multiplex over one master connection
//! (`ControlMaster=auto`), so password auth prompts once per run. Windows
//! OpenSSH cannot use the Unix-domain control socket, so sessions there each
//! open their own connection (prompt per session, but every session works).
//!
//! The client can also **deploy** the server binary: by default every sync
//! verifies that `cp2` exists at [`DEFAULT_REMOTE_PATH`] on the remote and
//! matches the local version, pushing the running binary over ssh when it is
//! missing or stale — so `cp2 SRC user@host:DEST` needs zero server setup
//! (rsync's `--rsync-path` made automatic).

use super::{Sudo, VersionProbe};
use crate::target::RemoteTarget;
use crate::{Error, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Where the server binary is expected (and deployed to) by default.
pub const DEFAULT_REMOTE_PATH: &str = "~/.cargo/bin/cp2";

/// Upper bound for a single ssh sub-step (platform probe, version check,
/// deploy, password-prompted spawn): a hung remote, a never-matching auth
/// prompt, or a stuck pty must not stall the run forever.
const SSH_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);

/// Spawn options for the system-ssh transport.
///
/// A `--password` is injected on a pty by the first, master-creating spawn
/// (the sshpass mechanism, native); later spawns attach to the authenticated
/// master and need no password. A run-unique `ControlPath` goes with it: the
/// master is always fresh, so the injection is guaranteed to see a prompt
/// (no reuse of a previous run's still-alive master, which would prompt
/// nothing and stall the injector).
#[derive(Default)]
pub struct SshAuth<'a> {
    /// The target password for the master-creating spawn (`None` afterwards).
    pub(crate) password: Option<&'a str>,
    /// The jump host's own password (`--jump-password`), when it differs
    /// from [`Self::password`]; prompts are answered in order (jump first,
    /// then target), reusing the last value for extra prompts.
    pub(crate) jump_password: Option<&'a str>,
    /// A run-unique `ControlPath` override (fresh master per run).
    pub(crate) control_path: Option<&'a str>,
}

/// A spawned ssh session: the child process plus its stdio pipe halves.
pub struct SshChild {
    /// The ssh process (drive it to completion with `wait`).
    pub child: Child,
    /// Frames are written here (ssh forwards stdin to the remote `--server`).
    pub stdin: ChildStdin,
    /// The remote `--server`'s stdout (ssh forwards it back).
    pub stdout: ChildStdout,
}

/// OpenSSH connection-multiplexing options shared by every ssh spawn in this
/// module. A run opens several sequential sessions to the same peer (platform
/// probe, version check, deploy, verify, sync); multiplexing makes them ride
/// one master connection, so password auth prompts **once per run** instead of
/// once per session.
///
/// `ControlMaster=auto` starts a master on the first session; `ControlPersist`
/// keeps it alive in the background after that session closes, so the next
/// (otherwise fresh) spawn attaches to it without re-authenticating — and the
/// socket is keyed by `%C` (a hash of user@host:port), so concurrent runs to
/// different peers do not collide.
///
/// Windows clients get **no** multiplexing options: Windows OpenSSH cannot
/// create or use the Unix-domain control socket (`getsockname failed: Not a
/// socket`), so every muxed ssh spawn fails before any session can run.
/// Sessions there each open their own connection instead — key auth is
/// silent, password auth prompts once per session.
/// Multiplexing options for a spawn: `ControlMaster=auto` with a
/// `ControlPath` — the shared `cp2-ssh-%C` in the temp dir, or the run's
/// unique override (used when a `--password`/`--jump-password` is in play,
/// so the master is always fresh and the pty injection is guaranteed a
/// prompt). With a password in play the master must outlive the whole run:
/// `--watch` sessions can stretch well past the default 60s persist, and an
/// expired master would make a later watch-cycle spawn re-create one and
/// re-prompt mid-stream. `ControlPersist=86400` (valid alongside
/// `ControlMaster=auto` per OpenSSH option semantics) keeps the run's master
/// alive for a day; the stale socket is harmless — every password run uses a
/// fresh pid-suffixed path.
fn multiplex_args_with(auth: &SshAuth<'_>) -> Vec<String> {
    if cfg!(windows) {
        // Windows OpenSSH cannot use the Unix-domain control socket at all.
        return Vec::new();
    }
    // Forward slashes in the control path: the ssh config parser would
    // otherwise treat backslashes as escapes in the option value.
    let path = auth.control_path.map_or_else(
        || {
            std::env::temp_dir()
                .join("cp2-ssh-%C")
                .to_string_lossy()
                .replace('\\', "/")
        },
        str::to_string,
    );
    let persist = if auth.password.is_some() || auth.jump_password.is_some() {
        "86400"
    } else {
        "60"
    };
    vec![
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        format!("ControlPath={path}"),
        "-o".to_string(),
        format!("ControlPersist={persist}"),
    ]
}

/// A fresh `ssh` command to `peer` running `remote_cmd`, with the shared
/// multiplexing options pre-applied. Callers configure stdio and spawn; a
/// `--password` is injected separately on a pty — see [`run_remote_with_password`].
fn ssh_command(peer: &RemoteTarget, remote_cmd: &str, auth: &SshAuth<'_>) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-p")
        .arg(peer.port.to_string())
        .arg(format!("{}@{}", peer.user, peer.host))
        .args(multiplex_args_with(auth))
        .arg(remote_cmd);
    cmd
}

impl SshChild {
    /// Split into boxed executor halves plus the child handle.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Box<dyn AsyncWrite + Unpin + Send>,
        Box<dyn AsyncRead + Unpin + Send>,
        Child,
    ) {
        (Box::new(self.stdin), Box::new(self.stdout), self.child)
    }
}

/// Spawn `ssh -p PORT user@host <remote-command> --server [server_args]` with
/// piped stdio.
///
/// `server_args` are appended to the remote `--server` invocation (e.g.
/// `"--jobs 4"` to tune the remote side's worker count); empty passes nothing.
/// Since the client auto-deploys a matching binary, the remote understands
/// the same flags the client does.
///
/// On Windows remotes the command is wrapped in `cmd /c` so `%USERPROFILE%`-style
/// paths expand regardless of the sshd default shell. Stderr is inherited so
/// ssh's own prompts, banners, and errors reach the user's terminal directly.
///
/// # Errors
///
/// Returns an error if the `ssh` binary cannot be spawned.
pub async fn spawn_ssh(
    peer: &RemoteTarget,
    remote_path: &str,
    remote_os: &str,
    server_args: &str,
    auth: &SshAuth<'_>,
    sudo: Sudo,
    sudo_password: Option<&str>,
) -> Result<SshChild> {
    let remote_cmd = server_invocation(remote_path, remote_os, server_args, sudo);
    let mut cmd = ssh_command(peer, &remote_command(remote_os, &remote_cmd), auth);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().map_err(Error::Io)?;
    if sudo == Sudo::Password {
        // `sudo -S` consumes exactly one stdin line as the password; write it
        // before any protocol frame so the frames pass through untouched.
        use tokio::io::AsyncWriteExt;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Other("ssh stdin unavailable".to_string()))?;
        stdin
            .write_all(format!("{}
", sudo_password.unwrap_or_default()).as_bytes())
            .await
            .map_err(Error::Io)?;
        child.stdin = Some(stdin);
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Other("ssh stdin unavailable".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Other("ssh stdout unavailable".to_string()))?;

    // The sync data flows over these pipes; a 64 KiB default capacity would
    // add a wakeup round trip every 64 KiB (see `platform::fs::enlarge_pipe`).
    crate::platform::fs::enlarge_pipe(&stdin);
    crate::platform::fs::enlarge_pipe(&stdout);

    Ok(SshChild {
        child,
        stdin,
        stdout,
    })
}

/// The remote server invocation: `<quoted-path> --server [server_args]`,
/// optionally prefixed with `sudo -n` (NOPASSWD) or `sudo -S` (password on
/// the first stdin line). Windows paths stay raw so `%VAR%` expands under the
/// `cmd /c` wrap; POSIX paths are shell-quoted so metacharacters in a
/// user-supplied `--remote-path` cannot be executed remotely.
fn server_invocation(remote_path: &str, remote_os: &str, server_args: &str, sudo: Sudo) -> String {
    let quoted_path = if remote_os == "windows" {
        remote_path.to_string()
    } else {
        shell_quote(remote_path)
    };
    let base = if server_args.is_empty() {
        format!("{quoted_path} --server")
    } else {
        format!("{quoted_path} --server {server_args}")
    };
    match sudo {
        Sudo::None => base,
        Sudo::NonInteractive => format!("sudo -n {base}"),
        Sudo::Password => format!("sudo -S {base}"),
    }
}

/// The merged single-session remote command (rsync-style): print the remote
/// platform, then `exec` the server on the same stream. The marker
/// ([`COMPOUND_SEP`]) separates the preamble from the protocol bytes; the
/// client reads the platform via [`read_preamble_platform`] before handing
/// the remainder to the executor. No sudo — `--remote-sudo` keeps the
/// classic probe flow, since the sudo mode is probe-discovered.
pub(crate) fn preamble_command(remote_path: &str, server_args: &str) -> String {
    let quoted = shell_quote(remote_path);
    let base = if server_args.is_empty() {
        format!("{quoted} --server")
    } else {
        format!("{quoted} --server {server_args}")
    };
    format!("uname -s -m 2>/dev/null; printf '\\n{COMPOUND_SEP}\\n'; exec {base}")
}

/// Spawn the sync session with the in-band platform preamble: one ssh
/// session carries both the platform probe and the sync (rsync's model —
/// the previous two-session flow opened a separate probe session whose
/// sshd session setup costs ~0.5 s). Same pipe plumbing as [`spawn_ssh`];
/// no sudo and no password (both keep the classic probe flow).
///
/// # Errors
///
/// Returns an error if the `ssh` binary cannot be spawned.
pub fn spawn_ssh_preamble(
    peer: &RemoteTarget,
    remote_path: &str,
    server_args: &str,
    auth: &SshAuth<'_>,
) -> Result<SshChild> {
    let remote_cmd = preamble_command(remote_path, server_args);
    let mut cmd = ssh_command(peer, &remote_cmd, auth);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().map_err(Error::Io)?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Other("ssh stdin unavailable".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Other("ssh stdout unavailable".to_string()))?;
    crate::platform::fs::enlarge_pipe(&stdin);
    crate::platform::fs::enlarge_pipe(&stdout);

    Ok(SshChild {
        child,
        stdin,
        stdout,
    })
}

/// The sync stream with the consumed preamble prefix: the platform line and
/// the marker were read off, and the buffered remainder plus the live stream
/// are served to the protocol reader (the executor reads the server's frames
/// through this wrapper).
pub(crate) struct PrefixedReader {
    prefix: Vec<u8>,
    pos: usize,
    inner: Box<dyn AsyncRead + Unpin + Send>,
}

impl PrefixedReader {
    fn new(prefix: Vec<u8>, inner: Box<dyn AsyncRead + Unpin + Send>) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl AsyncRead for PrefixedReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = std::cmp::min(buf.remaining(), self.prefix.len() - self.pos);
            buf.put_slice(&self.prefix[self.pos..self.pos + n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// Read the in-band platform preamble off the merged session's stdout: the
/// `uname -s -m` line up to [`COMPOUND_SEP`]. Returns `Ok(None)` when the
/// stream ends without the marker (the remote does not speak the preamble —
/// a Windows sshd, an unusual login shell) or the platform is unparseable;
/// the caller then kills the session and falls back to the classic
/// two-session probe flow.
///
/// # Errors
///
/// Returns an error when the stream read fails.
pub(crate) async fn read_preamble_platform<R: AsyncRead + Unpin + Send + 'static>(
    mut stdout: R,
) -> std::io::Result<Option<(String, String, PrefixedReader)>> {
    use tokio::io::AsyncReadExt;
    // Bound the preamble: the platform line is a few bytes; a remote that
    // never prints the marker would otherwise buffer forever.
    const PREAMBLE_MAX: usize = 64 * 1024;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if let Some(idx) = find_marker(&buf) {
            let head = String::from_utf8_lossy(&buf[..idx]);
            let platform = parse_uname(head.trim());
            let marker_end = idx + COMPOUND_SEP.len();
            // The marker's trailing newline is the preamble's, not a frame
            // byte — strip it so the protocol stream starts clean.
            let mut tail = marker_end;
            while tail < buf.len() && buf[tail] == b'\n' {
                tail += 1;
            }
            let prefix = buf[tail..].to_vec();
            return Ok(platform.map(|(os, arch)| {
                (os, arch, PrefixedReader::new(prefix, Box::new(stdout)))
            }));
        }
        if buf.len() >= PREAMBLE_MAX {
            return Ok(None);
        }
        let mut chunk = [0u8; 1024];
        let n = stdout.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Locate the preamble marker in the buffered bytes.
fn find_marker(buf: &[u8]) -> Option<usize> {
    let marker = COMPOUND_SEP.as_bytes();
    buf.windows(marker.len()).position(|w| w == marker)
}

/// Check the remote binary's version: `test -x <path> && <path> --version`.
///
/// Returns `Ok(None)` when the binary is missing or `--version` fails, and
/// `Ok(Some((version, protocol)))` otherwise. A failing ssh connection (auth,
/// host key, unreachable) surfaces as `Err` — the caller lets the real sync
/// report it.
///
/// Under `--remote-sudo` the probe doubles as the sudo-policy check: it runs
/// `sudo -n`, which reveals whether a NOPASSWD rule covers the remote path
/// (`sudo` prints "a password is required" when one is needed), and re-runs
/// with `sudo -S` + the client password when one is available. The resolved
/// invocation is reported back in [`VersionProbe::mode`].
///
/// # Errors
///
/// Returns an error if ssh itself cannot be spawned, or the `sudo -S`
/// password is rejected.
pub async fn check_remote_version(
    peer: &RemoteTarget,
    remote_path: &str,
    remote_os: &str,
    auth: &SshAuth<'_>,
    sudo: Sudo,
    sudo_password: Option<&str>,
) -> Result<VersionProbe> {
    if sudo == Sudo::None {
        let remote_cmd = if remote_os == "windows" {
            remote_command(remote_os, &format!("{remote_path} --version"))
        } else {
            let quoted = shell_quote(remote_path);
            format!("test -x {quoted} && {quoted} --version")
        };
        let output = tokio::time::timeout(
            SSH_COMMAND_TIMEOUT,
            ssh_command(peer, &remote_cmd, auth)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .output(),
        )
        .await
        .map_err(|_| {
            Error::Other("timed out checking the remote cp2 version — the remote may be hung".to_string())
        })?
        .map_err(Error::Io)?;

        if output.status.code() == Some(255) {
            return Err(connection_error(peer));
        }
        let version = output
            .status
            .success()
            .then(|| parse_remote_version(&String::from_utf8_lossy(&output.stdout)))
            .flatten();
        return Ok(VersionProbe {
            version,
            sudo_password_required: false,
            mode: Sudo::None,
        });
    }

    // Sudo requested: probe with `sudo -n` (its stderr reveals whether a
    // password is required — the message only appears when `test -x`
    // succeeded, so "binary missing" stays distinguishable).
    let quoted = shell_quote(remote_path);
    let probe_cmd = format!("test -x {quoted} && sudo -n {quoted} --version");
    let (code, stdout, stderr) =
        run_version_probe(peer, &probe_cmd, auth, Sudo::NonInteractive, None).await?;
    if code == 255 {
        return Err(connection_error(peer));
    }
    if code == 0 {
        return Ok(VersionProbe {
            version: parse_remote_version(&String::from_utf8_lossy(&stdout)),
            sudo_password_required: false,
            mode: Sudo::NonInteractive,
        });
    }
    if String::from_utf8_lossy(&stderr)
        .to_ascii_lowercase()
        .contains("password")
    {
        // sudo needs a password: re-probe with `sudo -S` + the prelude when
        // one is available, else report it for the caller to guide the user.
        let Some(pw) = sudo_password else {
            return Ok(VersionProbe {
                version: None,
                sudo_password_required: true,
                mode: Sudo::NonInteractive,
            });
        };
        let probe_cmd = format!("test -x {quoted} && sudo -S {quoted} --version");
        let (code, stdout, _) =
            run_version_probe(peer, &probe_cmd, auth, Sudo::Password, Some(pw)).await?;
        if code == 255 {
            return Err(connection_error(peer));
        }
        if code == 0 {
            return Ok(VersionProbe {
                version: parse_remote_version(&String::from_utf8_lossy(&stdout)),
                sudo_password_required: false,
                mode: Sudo::Password,
            });
        }
        // `test -x` already passed in the -n probe, so a failure here means
        // sudo rejected the password.
        return Err(Error::Other(
            "the remote sudo password was rejected (sudo -S failed); check --sudo-password              / --password"
                .to_string(),
        ));
    }
    // The binary is missing or not executable (sudo never ran).
    Ok(VersionProbe {
        version: None,
        sudo_password_required: false,
        mode: Sudo::NonInteractive,
    })
}

/// One-session platform + version probe (Unix remotes): `uname -s -m` and
/// `test -x <path> && <path> --version` ride a single ssh session,
/// separated by a marker line — a ControlMaster-multiplexed session on
/// WSL2 still costs ~0.37 s, so merging the two probes saves one session
/// per run (the sync session then stays the only other one).
///
/// The sudo policy chain mirrors [`check_remote_version`] (`-n`, then `-S`
/// with the client password, then a clear "password rejected" error).
///
/// Returns `(platform, version_probe)`. A remote that does not speak the
/// compound command (a Windows sshd, an unusual login shell) yields
/// `platform = None`; the caller falls back to the two-session path.
///
/// # Errors
///
/// Returns an error if ssh itself cannot be spawned, the connection fails
/// (exit 255), or the `sudo -S` password is rejected.
/// The marker line separating the platform and version halves of the
/// compound probe output.
const COMPOUND_SEP: &str = "__CP2_PROBE_SEP__";

/// Parse the marker-separated output of the compound probe into
/// `(platform, version)` — `platform = None` when the remote does not
/// speak the compound command (a Windows sshd, an unusual login shell).
fn parse_compound_probe(out: &str) -> (PlatformProbe, Option<(String, Option<String>)>) {
    let (head, tail) = out.split_once(COMPOUND_SEP).unwrap_or((out.trim(), ""));
    (
        parse_uname(head.trim()),
        parse_remote_version(tail.trim()),
    )
}

/// A probed remote platform: `Some((os, arch))` when it could be
/// determined.
pub(crate) type PlatformProbe = Option<(String, String)>;

/// One-session platform + version probe (Unix remotes): `uname -s -m` and
/// `test -x <path> && <path> --version` ride a single ssh session,
/// separated by a marker line — a ControlMaster-multiplexed session on
/// WSL2 still costs ~0.37 s, so merging the two probes saves one session
/// per run (the sync session then stays the only other one).
///
/// The sudo policy chain mirrors [`check_remote_version`] (`-n`, then `-S`
/// with the client password, then a clear "password rejected" error).
///
/// Returns `(platform, version_probe)`. A remote that does not speak the
/// compound command (a Windows sshd, an unusual login shell) yields
/// `platform = None`; the caller falls back to the two-session path.
///
/// # Errors
///
/// Returns an error if ssh itself cannot be spawned, the connection fails
/// (exit 255), or the `sudo -S` password is rejected.
pub async fn check_remote_platform_and_version(
    peer: &RemoteTarget,
    remote_path: &str,
    auth: &SshAuth<'_>,
    sudo: Sudo,
    sudo_password: Option<&str>,
) -> Result<(PlatformProbe, VersionProbe)> {
    /// Run the compound probe and parse the marker-separated output.
    async fn run_compound(
        peer: &RemoteTarget,
        remote_cmd: &str,
        auth: &SshAuth<'_>,
        sudo: Sudo,
        pw: Option<&str>,
    ) -> Result<(u32, PlatformProbe, Option<(String, Option<String>)>, Vec<u8>)> {
        let (code, stdout, stderr) = run_version_probe(peer, remote_cmd, auth, sudo, pw).await?;
        if code == 255 {
            return Err(connection_error(peer));
        }
        let out = String::from_utf8_lossy(&stdout);
        let (platform, version) = parse_compound_probe(&out);
        Ok((code, platform, version, stderr))
    }

    let quoted = shell_quote(remote_path);
    let plain = format!(
        "uname -s -m 2>/dev/null; printf '\\n{COMPOUND_SEP}\\n'; test -x {quoted} && {quoted} --version"
    );
    let nonint = format!(
        "uname -s -m 2>/dev/null; printf '\\n{COMPOUND_SEP}\\n'; test -x {quoted} && sudo -n {quoted} --version"
    );
    let passwd = format!(
        "uname -s -m 2>/dev/null; printf '\\n{COMPOUND_SEP}\\n'; test -x {quoted} && sudo -S {quoted} --version"
    );

    if sudo == Sudo::None {
        let (_, platform, version, _) = run_compound(peer, &plain, auth, Sudo::None, None).await?;
        return Ok((
            platform,
            VersionProbe {
                version,
                sudo_password_required: false,
                mode: Sudo::None,
            },
        ));
    }

    // Sudo: `-n` first; its stderr reveals whether a password is required.
    let (code, platform, version, stderr) =
        run_compound(peer, &nonint, auth, Sudo::NonInteractive, None).await?;
    if code == 0 {
        return Ok((
            platform,
            VersionProbe {
                version,
                sudo_password_required: false,
                mode: Sudo::NonInteractive,
            },
        ));
    }
    if String::from_utf8_lossy(&stderr)
        .to_ascii_lowercase()
        .contains("password")
    {
        let Some(pw) = sudo_password else {
            return Ok((
                platform,
                VersionProbe {
                    version: None,
                    sudo_password_required: true,
                    mode: Sudo::NonInteractive,
                },
            ));
        };
        let (code, platform, version, _) =
            run_compound(peer, &passwd, auth, Sudo::Password, Some(pw)).await?;
        if code == 0 {
            return Ok((
                platform,
                VersionProbe {
                    version,
                    sudo_password_required: false,
                    mode: Sudo::Password,
                },
            ));
        }
        // `test -x` passed in the -n probe, so a failure here means sudo
        // rejected the password.
        return Err(Error::Other(
            "the remote sudo password was rejected (sudo -S failed); check --sudo-password / --password"
                .to_string(),
        ));
    }
    // The binary is missing or not executable (sudo never ran), or the
    // remote does not speak the compound command (platform None).
    Ok((
        platform,
        VersionProbe {
            version,
            sudo_password_required: false,
            mode: Sudo::NonInteractive,
        },
    ))
}

/// Run one version probe under the given sudo invocation, returning
/// `(exit_code, stdout, stderr)`. The `-n` probe captures stderr (the sudo
/// password message is detected there); `-S` probes get the password as the
/// first stdin line before the protocol would start.
async fn run_version_probe(
    peer: &RemoteTarget,
    remote_cmd: &str,
    auth: &SshAuth<'_>,
    sudo: Sudo,
    sudo_password: Option<&str>,
) -> Result<(u32, Vec<u8>, Vec<u8>)> {
    use tokio::io::AsyncWriteExt;
    let stdin = if sudo == Sudo::Password {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let stderr = if sudo == Sudo::NonInteractive {
        Stdio::piped()
    } else {
        Stdio::inherit()
    };
    let mut child = ssh_command(peer, remote_cmd, auth)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .map_err(Error::Io)?;
    if sudo == Sudo::Password {
        // The prelude line is consumed by `sudo -S` as the password; the
        // remaining stdin (the version output request is stdin-less) is
        // untouched. Written right after spawn, before any protocol data.
        let mut child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Other("ssh stdin unavailable".to_string()))?;
        child_stdin
            .write_all(format!("{}
", sudo_password.unwrap_or_default()).as_bytes())
            .await
            .map_err(Error::Io)?;
        child.stdin = Some(child_stdin);
    }
    let output = tokio::time::timeout(SSH_COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            Error::Other("timed out checking the remote cp2 version — the remote may be hung".to_string())
        })?
        .map_err(Error::Io)?;
    let code = u32::try_from(output.status.code().unwrap_or(255)).unwrap_or(255);
    Ok((code, output.stdout, output.stderr))
}

/// The standard "ssh connection failed" error for the version probes.
fn connection_error(peer: &RemoteTarget) -> Error {
    Error::Other(format!(
        "ssh connection to {}@{} failed (exit 255) while checking the remote cp2 — \
         check the host, credentials, and host key",
        peer.user, peer.host
    ))
}

/// Detect the remote platform, normalized to `(os, arch)` in the same terms
/// as `std::env::consts`. `Ok(None)` when it cannot be determined.
///
/// Detection chain: `uname -s -m` for Unix remotes; if that fails (e.g. a
/// Windows sshd, which has no `uname`), falls back to
/// `cmd /c echo %PROCESSOR_ARCHITECTURE%` — instant and locale-independent
/// (unlike `systeminfo`, whose "System Type" field is localized).
///
/// # Errors
///
/// Returns an error if ssh itself cannot be spawned.
pub async fn check_remote_platform(
    peer: &RemoteTarget,
    auth: &SshAuth<'_>,
) -> Result<Option<(String, String)>> {
    let unix = probe_output(peer, "uname -s -m", auth).await?;
    if let Some(platform) = parse_uname(&unix) {
        return Ok(Some(platform));
    }
    // Windows fallback: `%PROCESSOR_ARCHITECTURE%` (AMD64/ARM64/x86).
    let win = probe_output(peer, "cmd /c echo %PROCESSOR_ARCHITECTURE%", auth).await?;
    let arch = win.split_whitespace().next().unwrap_or("");
    let arch = match arch {
        "AMD64" => "x86_64",
        "ARM64" => "aarch64",
        // No i686 or other sidecars are shipped (`scripts/build-release.sh`
        // builds x86_64/aarch64 only), so a 32-bit or otherwise unknown
        // Windows remote has no deployable binary: warn loudly — the caller
        // still falls back to the local platform, but no longer silently,
        // and the deploy fails visibly instead of shipping the wrong sidecar.
        other => {
            tracing::warn!(
                "remote Windows architecture '{other}' is unsupported: no cp2 sidecar exists \
                 for it; the deploy to this host will fail"
            );
            return Ok(None);
        }
    };
    Ok(Some(("windows".to_string(), arch.to_string())))
}

/// Run a probe command and return its stdout. With a `--password` (or a
/// `--jump-password` on its own), the master-creating spawn rides a pty so
/// the password can be injected at the prompt (sshpass mechanism, native);
/// the command's stdout stays on a clean pipe.
async fn probe_output(peer: &RemoteTarget, remote_cmd: &str, auth: &SshAuth<'_>) -> Result<String> {
    if auth.password.is_some() || auth.jump_password.is_some() {
        #[cfg(unix)]
        {
            let (code, out) = run_remote_with_password(peer, remote_cmd, auth).await?;
            if code == 255 {
                return Err(Error::Other(format!(
                    "ssh connection to {}@{} failed (exit 255) — check the credentials and \
                     host key",
                    peer.user, peer.host
                )));
            }
            return Ok(String::from_utf8_lossy(&out).into_owned());
        }
        #[cfg(not(unix))]
        {
            // No pty on Windows: the caller routes `--password` to the russh
            // transport before reaching here, so this is defensive only.
            return Err(Error::Other(
                "--password/--jump-password needs the russh transport on Windows".to_string(),
            ));
        }
    }
    ssh_output(peer, remote_cmd, auth).await
}

/// Run a remote command and return its stdout (empty on a non-255 failure —
/// e.g. a missing remote command — which the callers treat as "could not
/// determine"). A 255 exit (ssh's own connection/auth/host-key failure) is an
/// error, not an empty probe.
async fn ssh_output(peer: &RemoteTarget, remote_cmd: &str, auth: &SshAuth<'_>) -> Result<String> {
    let output = tokio::time::timeout(
        SSH_COMMAND_TIMEOUT,
        ssh_command(peer, remote_cmd, auth)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .output(),
    )
    .await
    .map_err(|_| {
        Error::Other("timed out probing the remote — the remote may be hung".to_string())
    })?
    .map_err(Error::Io)?;
    if output.status.code() == Some(255) {
        return Err(Error::Other(format!(
            "ssh connection to {}@{} failed (exit 255) — check the host, credentials, and \
             host key",
            peer.user, peer.host
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `uname -s -m` output (e.g. `Linux x86_64`) into a platform.
/// Shared by the system-ssh and russh transports.
pub(crate) fn parse_uname(stdout: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = stdout.split_whitespace().collect();
    let (os, arch) = (parts.first()?, parts.get(1)?);
    normalize_platform(os, arch)
}

/// Map `uname -s`/`uname -m` output to `std::env::consts` terms.
fn normalize_platform(os: &str, arch: &str) -> Option<(String, String)> {
    let os = os.to_ascii_lowercase();
    let arch = arch.to_ascii_lowercase();
    let os = match os.as_str() {
        "linux" => "linux",
        "darwin" => "macos",
        "windows" => "windows",
        _ => return None,
    };
    let arch = match arch.as_str() {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => return None,
    };
    Some((os.to_string(), arch.to_string()))
}

/// Map a normalized `(os, arch)` (from `uname` via [`normalize_platform`]) to
/// the target-triple name of a sidecar binary (`cp2-<triple>` next to the
/// client). Linux sidecars are musl builds (libc-agnostic); macOS uses the
/// native Darwin builds.
pub(crate) fn remote_triple(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Some("aarch64-pc-windows-msvc"),
        _ => None,
    }
}

/// The default remote path for the server binary on `os`.
#[must_use]
pub fn default_remote_path(os: &str) -> String {
    match os {
        "windows" => r"%USERPROFILE%\.cargo\bin\cp2.exe".to_string(),
        _ => DEFAULT_REMOTE_PATH.to_string(),
    }
}

/// Wrap a remote command for execution on `os`. Windows commands go through
/// `cmd /c` so `%VAR%` expansion works regardless of the sshd default shell.
/// Shared by the system-ssh and russh transports.
pub(crate) fn remote_command(os: &str, command: &str) -> String {
    match os {
        "windows" => format!("cmd /c {command}"),
        _ => command.to_string(),
    }
}

/// Sidecar names to try for a remote platform, in preference order. Windows
/// accepts either the MSVC or the GNU build (both run on any Windows); the
/// GNU one can be cross-compiled from Linux.
#[must_use]
pub fn sidecar_candidates(os: &str, arch: &str) -> Vec<&'static str> {
    match remote_triple(os, arch) {
        Some("x86_64-pc-windows-msvc") => {
            vec!["x86_64-pc-windows-msvc", "x86_64-pc-windows-gnu"]
        }
        Some("aarch64-pc-windows-msvc") => {
            vec!["aarch64-pc-windows-msvc", "aarch64-pc-windows-gnu"]
        }
        Some(triple) => vec![triple],
        None => Vec::new(),
    }
}

/// Path of the sidecar binary for `triple`, next to the running client
/// (`<client-dir>/cp2-<triple>`).
#[must_use]
pub fn sidecar_path(triple: &str) -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(format!("cp2-{triple}"))
}

/// The platform this binary was built for, in the same terms as
/// [`normalize_platform`].
pub(crate) fn local_platform() -> (String, String) {
    (
        std::env::consts::OS.to_string(),
        std::env::consts::ARCH.to_string(),
    )
}

/// Parse the version from a remote `--version` run (clap prints
/// `cp2 <version> (build <fingerprint>)`). Returns `(crate version, build
/// fingerprint)`; the fingerprint is `None` for binaries from before the
/// banner existed — which the deploy check treats as stale (it cannot verify
/// them). Returns `None` when the output is not recognizable. Shared by the
/// system-ssh and russh transports.
pub(crate) fn parse_remote_version(stdout: &str) -> Option<(String, Option<String>)> {
    let tokens: Vec<&str> = stdout.split_whitespace().collect();
    let version = tokens.get(1)?.to_string();
    // Clap renders `... (build 0123...)` — the closing paren lands on the
    // last token, so strip it before parsing.
    let fingerprint = tokens
        .windows(2)
        .find(|w| w[0] == "(build")
        .and_then(|w| {
            let fp = w[1].trim_end_matches(')');
            // 16 hex digits, or treat the token as unparseable (unknown
            // build, not a parse failure).
            (fp.len() == 16 && fp.chars().all(|c| c.is_ascii_hexdigit())).then(|| fp.to_string())
        });
    Some((version, fingerprint))
}

/// Quote `path` for interpolation into a remote POSIX shell command so that
/// shell metacharacters in a user-supplied `--remote-path` cannot be executed
/// on the remote account. The value is single-quoted with embedded `'`
/// escaped as `'\''`. A leading `~` (bare, or `~/...`) keeps its tilde and
/// the first slash unquoted — POSIX tilde-prefix expansion applies only up
/// to the first *unquoted* `/` (or the whole word when there is none), so a
/// `~` immediately followed by a quoted string would stay literal — and the
/// remainder is single-quoted. Thus the default `~/.cargo/bin/cp2` becomes
/// `~/'.cargo/bin/cp2'` and still expands to the account home. A `~user/...`
/// prefix (no `/` right after the tilde) is quoted whole: no login-name
/// lookup happens.
fn shell_quote(path: &str) -> String {
    let Some(rest) = path.strip_prefix('~') else {
        return format!("'{}'", path.replace('\'', "'\\''"));
    };
    if rest.is_empty() {
        // Bare `~`: wholly unquoted so the tilde expands.
        return "~".to_string();
    }
    if let Some(after_slash) = rest.strip_prefix('/') {
        // `~/…`: the tilde and the first slash stay unquoted (`~/` expands to
        // `$HOME/`); the rest is single-quoted.
        format!("~/'{}'", after_slash.replace('\'', "'\\''"))
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

/// The remote deploy command for `os`: create the parent directory, stream
/// the payload to stdin, move it into place, and set the executable bit.
/// On POSIX the payload streams to a unique temp name (`<path>.tmp.$$`, the
/// remote shell's pid) and is `mv`ed onto the destination, so an interrupted
/// deploy leaves a stray temp file — never a truncated binary at the
/// destination; `chmod +x` runs after the move. The path is shell-quoted
/// (metacharacters in `--remote-path` cannot execute remotely) while `~` and
/// relative paths still expand. Shared by the system-ssh and russh
/// transports.
pub(crate) fn deploy_command(remote_os: &str, remote_path: &str) -> String {
    if remote_os == "windows" {
        windows_push_command(remote_path)
    } else {
        let dir = match remote_path.rfind('/') {
            Some(i) => &remote_path[..i],
            None => ".",
        };
        let quoted = shell_quote(remote_path);
        let tmp = format!("{quoted}.tmp.$$");
        format!(
            "mkdir -p {} && cat > {tmp} && mv {tmp} {quoted} && chmod +x {quoted}",
            shell_quote(dir)
        )
    }
}

/// The merged deploy-and-serve command (the single-session flow's deploy
/// retry): stream the binary (exactly `size` bytes, consumed by `head -c` —
/// the stdin then carries the protocol frames, so `cat`'s read-to-EOF would
/// swallow them), move it into place, and `exec` it as the server on the
/// same session — the deploy session *is* the sync session, and the Hello
/// handshake verifies the deployed binary (the separate post-deploy version
/// check is gone). POSIX only; the sudo/Windows paths keep the classic
/// two-phase deploy.
pub(crate) fn deploy_serve_command(remote_path: &str, server_args: &str, size: u64) -> String {
    let dir = match remote_path.rfind('/') {
        Some(i) => &remote_path[..i],
        None => ".",
    };
    let quoted = shell_quote(remote_path);
    let tmp = format!("{quoted}.tmp.$$");
    let base = if server_args.is_empty() {
        format!("{quoted} --server")
    } else {
        format!("{quoted} --server {server_args}")
    };
    format!(
        "mkdir -p {} && head -c {size} > {tmp} && mv {tmp} {quoted} && chmod +x {quoted} && exec {base}",
        shell_quote(dir)
    )
}

/// The deploy payload for `os`: the raw binary, or its base64 encoding on
/// Windows (which has no `cat` — `certutil -decode` recovers it remotely;
/// command-line length limits rule out inline base64). Shared by the
/// system-ssh and russh transports.
pub(crate) fn deploy_payload(remote_os: &str, local_binary: &Path) -> Result<Vec<u8>> {
    match remote_os {
        "windows" => Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            std::fs::read(local_binary).map_err(Error::Io)?,
        )
        .into_bytes()),
        _ => std::fs::read(local_binary).map_err(Error::Io),
    }
}

/// Push a local cp2 binary to the remote at `remote_path`.
///
/// Streams the binary over the ssh channel to a unique temp name, then moves
/// it into place (`mv`) and sets the executable bit — see [`deploy_command`]
/// for the exact remote command. Bounded by [`SSH_COMMAND_TIMEOUT`] so a
/// remote that never drains stdin cannot stall the run.
///
/// # Errors
///
/// Returns an error if ssh fails, the remote command exits non-zero, or the
/// deploy exceeds the timeout.
pub async fn push_remote_binary(
    peer: &RemoteTarget,
    remote_path: &str,
    local_binary: &Path,
    remote_os: &str,
    auth: &SshAuth<'_>,
) -> Result<()> {
    let remote_cmd = deploy_command(remote_os, remote_path);
    let payload = deploy_payload(remote_os, local_binary)?;

    let mut child = ssh_command(peer, &remote_cmd, auth)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(Error::Io)?;

    let timed = tokio::time::timeout(SSH_COMMAND_TIMEOUT, async {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Other("ssh stdin unavailable".to_string()))?;
        stdin.write_all(&payload).await.map_err(Error::Io)?;
        drop(stdin); // EOF → the remote receiver finishes
        child.wait().await.map_err(Error::Io)
    })
    .await;

    match timed {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(Error::Other(format!(
            "failed to deploy cp2 to the server: ssh exited with {status}"
        ))),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            let _ = child.kill().await;
            Err(Error::Other(
                "timed out deploying cp2 to the server — the remote may be hung".to_string(),
            ))
        }
    }
}

/// The merged deploy-and-serve spawn (see [`deploy_serve_command`]): the
/// payload is written first (exactly the size the remote `head -c` consumes),
/// then the session's send half is handed over for the protocol frames — the
/// same ssh session that just received the binary serves the sync. Returns
/// the session halves plus the child handle, like [`spawn_ssh`].
///
/// # Errors
///
/// Returns an error if the `ssh` binary cannot be spawned or the payload
/// write fails.
pub async fn push_remote_binary_and_serve(
    peer: &RemoteTarget,
    remote_path: &str,
    server_args: &str,
    local_binary: &Path,
    auth: &SshAuth<'_>,
) -> Result<(Child, ChildStdin, ChildStdout)> {
    let payload = std::fs::read(local_binary).map_err(Error::Io)?;
    let remote_cmd = deploy_serve_command(remote_path, server_args, payload.len() as u64);
    let mut cmd = ssh_command(peer, &remote_cmd, auth);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().map_err(Error::Io)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Other("ssh stdin unavailable".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Other("ssh stdout unavailable".to_string()))?;
    crate::platform::fs::enlarge_pipe(&stdin);
    crate::platform::fs::enlarge_pipe(&stdout);
    // The payload first, then the protocol frames ride the same pipe.
    stdin.write_all(&payload).await.map_err(Error::Io)?;
    Ok((child, stdin, stdout))
}

/// The Windows deploy command: PowerShell creates the parent directory,
/// writes the base64 stdin to a temp file (`cp2-$PID.b64` — unique per
/// deploy, so concurrent runs do not collide on a shared name and a dead
/// session leaves no misleading fixed-name litter), `certutil -decode`
/// decodes it into place, and the temp is removed. Wrapped in `cmd /c` so
/// `%USERPROFILE%` in the path expands under either default shell.
///
/// The path is embedded in PowerShell single quotes, which is *not* shell
/// escaping: `remote_path` must not contain a `'` (the PowerShell string
/// delimiter) or a `"` (the surrounding `cmd` quoting) — the CLI documents
/// this constraint for `--remote-path` on Windows remotes.
pub(crate) fn windows_push_command(remote_path: &str) -> String {
    format!(
        r#"cmd /c powershell -NoProfile -Command "New-Item -Force -ItemType Directory (Split-Path -Parent '{remote_path}') | Out-Null; $t = $env:TEMP\cp2-$PID.b64; $input | Out-File -Encoding ascii -NoNewline $t; certutil -decode -f $t '{remote_path}'; del $t""#
    )
}

/// Spawn `ssh` on a pty (the sshpass mechanism, native): ssh's stdin and
/// stderr ride the pty so the injector can answer the password prompt, while
/// stdout stays on a clean pipe carrying only the command's output. Password
/// auth is forced, so the prompt is guaranteed to appear before the session
/// starts — the injector never waits on an already-started session.
///
/// The whole spawn+inject+read+wait is bounded by [`SSH_COMMAND_TIMEOUT`]: a
/// never-matching prompt (e.g. a keyboard-interactive `OTP:` challenge) or a
/// hung remote cannot stall the run — ssh is killed and a clear error naming
/// the stage is returned. Our copy of the pty master is closed before the
/// wait so ssh can never block forever in `readpassphrase` on a dead prompt.
#[cfg(unix)]
async fn run_remote_with_password(
    peer: &RemoteTarget,
    remote_cmd: &str,
    auth: &SshAuth<'_>,
) -> Result<(u32, Vec<u8>)> {
    use tokio::io::AsyncReadExt;
    let (master, slave) = open_pty()?;
    let mut cmd = Command::new("ssh");
    cmd.arg("-p")
        .arg(peer.port.to_string())
        .arg(format!("{}@{}", peer.user, peer.host))
        .args(multiplex_args_with(auth))
        .arg("-o")
        .arg("PubkeyAuthentication=no")
        .arg("-o")
        .arg("PreferredAuthentications=password,keyboard-interactive")
        .arg(remote_cmd)
        .stdin(Stdio::from(slave.try_clone().map_err(Error::Io)?))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(slave));
    let mut child = cmd.spawn().map_err(Error::Io)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Other("ssh stdout unavailable".to_string()))?;

    // The injector answers the prompts on a cloned master fd; the original
    // is dropped right away, so no pty master stays open on this side while
    // we wait (an open master would keep the slave's readers alive and let
    // ssh block in readpassphrase after the injector has stopped).
    // Prompts are answered in order: the jump host's password first (if
    // given), then the target's, reusing the last for any extra prompt.
    let injector_master = master.try_clone().map_err(Error::Io)?;
    drop(master);
    let mut passwords: Vec<String> = Vec::new();
    if let Some(jump_password) = auth.jump_password {
        passwords.push(jump_password.to_string());
    }
    if let Some(password) = auth.password {
        passwords.push(password.to_string());
    }
    let injector = tokio::task::spawn_blocking(move || {
        inject_password(injector_master, passwords);
    });

    let timed = tokio::time::timeout(SSH_COMMAND_TIMEOUT, async {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).await.map_err(Error::Io)?;
        let status = child.wait().await.map_err(Error::Io)?;
        let _ = injector.await;
        let code = u32::try_from(status.code().unwrap_or(255)).unwrap_or(255);
        Ok::<_, Error>((code, out))
    })
    .await;

    match timed {
        Ok(Ok((code, out))) => Ok((code, out)),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            // Killing ssh closes the pty slave, which unblocks the injector's
            // read (EIO on the master) so the spawned thread finishes.
            let _ = child.kill().await;
            Err(Error::Other(format!(
                "timed out waiting for the ssh password prompt ({}s) — check the host key \
                 and credentials",
                SSH_COMMAND_TIMEOUT.as_secs()
            )))
        }
    }
}

/// Open a pty pair with the slave in raw mode, so bytes pass through
/// unmodified (no echo, no CR/LF translation, no line buffering) — the
/// protocol frames must not be mangled by the terminal.
#[cfg(unix)]
fn open_pty() -> std::io::Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::FromRawFd;
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: openpty writes the two fds we own; the termios pointers are
    // NULL, requesting the defaults.
    let rc = unsafe {
        libc::openpty(
            std::ptr::addr_of_mut!(master),
            std::ptr::addr_of_mut!(slave),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: tcgetattr/tcsetattr operate on the slave fd we just opened;
    // cfmakeraw mutates the termios in place.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(slave, std::ptr::addr_of_mut!(termios)) } != 0 {
        let e = std::io::Error::last_os_error();
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return Err(e);
    }
    unsafe { libc::cfmakeraw(std::ptr::addr_of_mut!(termios)) };
    if unsafe { libc::tcsetattr(slave, libc::TCSANOW, std::ptr::addr_of!(termios)) } != 0 {
        let e = std::io::Error::last_os_error();
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return Err(e);
    }
    // SAFETY: from_raw_fd takes ownership of the fds.
    Ok((
        unsafe { std::fs::File::from_raw_fd(master) },
        unsafe { std::fs::File::from_raw_fd(slave) },
    ))
}

/// Watch the pty for the ssh password prompt (`...assword:`) and answer
/// every occurrence (sshpass behavior) with the matching password: prompts
/// are answered in order — the jump host's password first (if a separate
/// `--jump-password` was given), then the target's — reusing the last value
/// for any extra prompt, so a jump run with one password still works. The
/// pty keeps draining so it never fills; a wrong password just makes ssh
/// retry until it gives up. A host-key prompt (`yes/no` / `continue
/// connecting`) is answered `no` — fail-closed: ssh aborts with a host-key
/// rejection instead of blocking forever on the pty (the injector must
/// answer, not `break`: an unanswered prompt would leave ssh spinning in
/// `readpassphrase`, and the open pty master would prevent EOF until the
/// child is reaped). Exits when the slave closes (no prompt: the spawn
/// attached to an already-authenticated master). The password copies are
/// scrubbed when the injector exits.
#[cfg(unix)]
fn inject_password(master: std::fs::File, mut passwords: Vec<String>) {
    use std::io::{Read, Write};
    use zeroize::Zeroize;
    let mut reader = master;
    let Ok(mut writer) = reader.try_clone() else {
        return;
    };
    let mut buf = [0u8; 1024];
    let mut window: Vec<u8> = Vec::new();
    let mut answered = 0usize;
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 {
            break;
        }
        window.extend_from_slice(&buf[..n]);
        if window.windows(8).any(|w| w == b"assword:") {
            // At least one password is always present (the pty path is only
            // entered when a target or jump password was given); `saturating`
            // only guards the unreachable empty-list case.
            let password = &passwords[answered.min(passwords.len().saturating_sub(1))];
            let _ = writer.write_all(password.as_bytes());
            let _ = writer.write_all(b"\n");
            answered += 1;
            window.clear();
        } else if window.windows(6).any(|w| w == b"yes/no")
            || window.windows(19).any(|w| w == b"continue connecting")
        {
            // Unknown host key: fail closed — never auto-accept it.
            let _ = writer.write_all(b"no\n");
            window.clear();
            break;
        }
        // Bound the window: prompts arrive early, before the session.
        if window.len() > 8192 {
            window.drain(..window.len() - 1024);
        }
    }
    for secret in &mut passwords {
        secret.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Location;

    #[test]
    fn server_invocation_plain_and_sudo() {
        // Plain: the quoted path plus the server flag.
        assert_eq!(
            server_invocation("~/.cargo/bin/cp2", "linux", "", Sudo::None),
            "~/'.cargo/bin/cp2' --server"
        );
        // Server args are appended after --server.
        assert_eq!(
            server_invocation("~/.cargo/bin/cp2", "linux", "--jobs 4", Sudo::None),
            "~/'.cargo/bin/cp2' --server --jobs 4"
        );
        // `sudo -n` (NOPASSWD) and `sudo -S` (password prelude) prefixes.
        assert_eq!(
            server_invocation("~/.cargo/bin/cp2", "linux", "--jobs 4", Sudo::NonInteractive),
            "sudo -n ~/'.cargo/bin/cp2' --server --jobs 4"
        );
        assert_eq!(
            server_invocation("~/.cargo/bin/cp2", "linux", "", Sudo::Password),
            "sudo -S ~/'.cargo/bin/cp2' --server"
        );
        // Windows remotes keep the raw path for the cmd /c wrap.
        assert_eq!(
            server_invocation("C:\\cp2.exe", "windows", "", Sudo::Password),
            "sudo -S C:\\cp2.exe --server"
        );
    }

    #[test]
    fn parse_version_from_clap_output() {
        assert_eq!(
            parse_remote_version("cp2 0.1.0\n"),
            Some(("0.1.0".to_string(), None))
        );
        assert_eq!(
            parse_remote_version("cp2 0.1.0"),
            Some(("0.1.0".to_string(), None))
        );
        // The build-fingerprint banner used by current binaries (clap
        // renders the closing paren attached to the hex).
        assert_eq!(
            parse_remote_version("cp2 0.1.0 (build 0123456789abcdef)"),
            Some(("0.1.0".to_string(), Some("0123456789abcdef".to_string())))
        );
        assert_eq!(
            parse_remote_version("cp2 0.2.0 (build fedcba9876543210)\n"),
            Some(("0.2.0".to_string(), Some("fedcba9876543210".to_string())))
        );
        assert_eq!(parse_remote_version(""), None);
        assert_eq!(parse_remote_version("cp2\n"), None);
        // Missing fingerprint token → unknown build, not a parse failure.
        assert_eq!(
            parse_remote_version("cp2 0.1.0 (build ?)\n"),
            Some(("0.1.0".to_string(), None))
        );
    }

    /// The pty password-injection core (the sshpass mechanism, native): a
    /// child prints a password prompt, the injector detects it and types the
    /// password, and the child's read receives it unmodified (raw mode).
    #[cfg(unix)]
    #[test]
    fn pty_injection_answers_password_prompt() {
        use std::process::{Command as StdCommand, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let (master, slave) = open_pty().unwrap();
        let script = format!(
            "printf 'user@host password: '; IFS= read -r line; printf 'got:%s\\n' \"$line\" > {}",
            out.display()
        );
        let slave_in = slave.try_clone().unwrap();
        let mut child = StdCommand::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::from(slave_in))
            .stdout(Stdio::from(slave))
            .spawn()
            .unwrap();

        let injector_master = master.try_clone().unwrap();
        let handle =
            std::thread::spawn(move || inject_password(injector_master, vec!["s3cret".to_string()]));
        let status = child.wait().unwrap();
        handle.join().unwrap();
        assert!(status.success());
        let got = std::fs::read_to_string(&out).unwrap();
        assert_eq!(got.trim(), "got:s3cret", "the injected password must arrive intact");
    }

    #[test]
    fn command_shape_is_rsync_like() {
        // Parse the same target the CLI produces and check the args we build.
        // Ports live in `--port`, never in the target string.
        let target = Location::parse("alice@backup.example.com:restore");
        let Location::Remote(remote) = target else {
            panic!("expected remote");
        };
        assert_eq!(remote.user, "alice");
        assert_eq!(remote.host, "backup.example.com");
        assert_eq!(remote.port, 22);
        assert_eq!(remote.path, "restore");
    }

    #[test]
    fn multiplex_args_share_one_master_per_peer() {
        let args = multiplex_args_with(&SshAuth::default());
        // Windows OpenSSH cannot use the Unix-domain control socket, so
        // Windows clients spawn unmuxed (one connection per session).
        if cfg!(windows) {
            assert!(args.is_empty(), "{args:?}");
            return;
        }
        assert_eq!(args.len(), 6);
        assert_eq!(args[0], "-o");
        assert_eq!(args[1], "ControlMaster=auto");
        assert_eq!(args[2], "-o");
        let path = args[3].strip_prefix("ControlPath=").unwrap();
        // `%C` keys the socket by user@host:port, so peers do not collide.
        assert!(path.contains("%C"), "{path}");
        // Forward slashes even on Windows: the ssh config parser would
        // otherwise treat backslashes as escapes in the option value.
        assert!(!path.contains('\\'), "{path}");
        assert_eq!(args[4], "-o");
        assert_eq!(args[5], "ControlPersist=60");

        // A password in play must keep the master alive across the whole run:
        // `--watch` cycles can stretch past the 60s default, and an expired
        // master would make a later spawn re-create one and re-prompt
        // mid-stream.
        let args = multiplex_args_with(&SshAuth {
            password: Some("pw"),
            ..SshAuth::default()
        });
        assert_eq!(args[5], "ControlPersist=86400");
        let args = multiplex_args_with(&SshAuth {
            jump_password: Some("pw"),
            ..SshAuth::default()
        });
        assert_eq!(args[5], "ControlPersist=86400");
    }
}

#[cfg(test)]
mod platform_tests {
    use super::*;

    #[test]
    fn compound_probe_parses_platform_and_version() {
        let out = "Linux x86_64\n\n__CP2_PROBE_SEP__\ncp2 0.1.1 (build 5ea78b018f24a474)\n";
        let (platform, version) = parse_compound_probe(out);
        assert_eq!(platform, Some(("linux".to_string(), "x86_64".to_string())));
        let (v, fp) = version.unwrap();
        assert_eq!(v, "0.1.1");
        assert_eq!(fp.as_deref(), Some("5ea78b018f24a474"));
    }

    #[test]
    fn compound_probe_missing_binary_reports_version_none() {
        // `test -x` failed: the marker is still printed, the tail is empty.
        let out = "Linux x86_64\n\n__CP2_PROBE_SEP__\n";
        let (platform, version) = parse_compound_probe(out);
        assert_eq!(platform, Some(("linux".to_string(), "x86_64".to_string())));
        assert_eq!(version, None);
    }

    #[test]
    fn compound_probe_non_unix_remote_yields_no_platform() {
        // A Windows sshd / cmd shell: no uname line, no marker.
        let out = "The system cannot find the path specified.\n";
        let (platform, version) = parse_compound_probe(out);
        assert_eq!(platform, None);
        assert_eq!(version, None);
    }

    #[test]
    fn parse_uname_chain() {
        assert_eq!(
            parse_uname("Linux x86_64\n"),
            Some(("linux".to_string(), "x86_64".to_string()))
        );
        assert_eq!(parse_uname(""), None);
        assert_eq!(parse_uname("cmd: uname not found"), None);
    }

    #[test]
    fn preamble_command_quotes_and_execs() {
        let cmd = preamble_command("~/.cargo/bin/cp2", "--jobs 4");
        assert!(cmd.starts_with("uname -s -m 2>/dev/null; printf '\\n__CP2_PROBE_SEP__\\n'; exec "));
        assert!(cmd.ends_with("--server --jobs 4"));
        // The default path is shell-quoted (`~/'...'` — the quote protects
        // metacharacters while `~` still expands).
        assert!(cmd.contains("~/'.cargo/bin/cp2'"));
        // A metacharacter in `--remote-path` must stay quoted.
        let evil = preamble_command("x; rm -rf /", "");
        assert!(evil.contains("'x; rm -rf /'"));
    }

    #[test]
    fn preamble_marker_split_and_strip() {
        // The marker plus its trailing newline are consumed; the remainder
        // (the first protocol bytes, possibly arriving in the same read) is
        // the prefix. The platform head parses like the compound probe's.
        let bytes = b"Linux x86_64\n\n__CP2_PROBE_SEP__\n\x00\x01\x02";
        let idx = find_marker(bytes).expect("marker present");
        let head = String::from_utf8_lossy(&bytes[..idx]);
        assert_eq!(
            parse_uname(head.trim()),
            Some(("linux".to_string(), "x86_64".to_string()))
        );
        let marker_end = idx + COMPOUND_SEP.len();
        let mut tail = marker_end;
        while tail < bytes.len() && bytes[tail] == b'\n' {
            tail += 1;
        }
        assert_eq!(&bytes[tail..], &[0x00, 0x01, 0x02]);
        assert!(find_marker(b"no marker here").is_none());
    }

    #[test]
    fn prefixed_reader_serves_prefix_then_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // A tiny in-memory duplex pair: the prefix plus the live bytes must
        // come out in order through the wrapper.
        let (mut writer, reader) = tokio::io::duplex(64);
        let mut prefixed = PrefixedReader::new(vec![0xAA, 0xBB], Box::new(reader));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        rt.block_on(async {
            writer.write_all(&[0xCC, 0xDD]).await.unwrap();
            drop(writer);
            let mut out = Vec::new();
            prefixed.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        });
    }

    #[test]
    fn windows_defaults_and_triples() {
        assert_eq!(
            default_remote_path("windows"),
            r"%USERPROFILE%\.cargo\bin\cp2.exe"
        );
        assert_eq!(default_remote_path("linux"), DEFAULT_REMOTE_PATH);
        assert_eq!(
            remote_triple("windows", "x86_64"),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(
            remote_triple("windows", "aarch64"),
            Some("aarch64-pc-windows-msvc")
        );
        assert_eq!(
            remote_triple("linux", "x86_64"),
            Some("x86_64-unknown-linux-musl")
        );
    }

    #[test]
    fn windows_push_command_shape() {
        let cmd = windows_push_command(r"%USERPROFILE%\.local\\bin\\cp2.exe");
        assert!(cmd.starts_with("cmd /c powershell -NoProfile -Command"));
        assert!(cmd.contains("certutil -decode -f"));
        assert!(cmd.contains("Split-Path -Parent"));
        // The temp name is unique per deploy (`$PID`), not a fixed file two
        // concurrent runs could collide on.
        assert!(cmd.contains(r"$env:TEMP\cp2-$PID.b64"), "{cmd}");
    }

    #[test]
    fn shell_quote_escapes_metacharacters() {
        // `~/…` keeps the tilde and first slash unquoted so it expands to
        // `$HOME/`; the remainder is single-quoted.
        assert_eq!(shell_quote("~/.cargo/bin/cp2"), "~/'.cargo/bin/cp2'");
        assert_eq!(shell_quote("~"), "~");
        assert_eq!(shell_quote("/opt/cp2"), "'/opt/cp2'");
        assert_eq!(shell_quote("rel/path"), "'rel/path'");
        // Embedded `'` is escaped; a leading `~` without `/` is fully quoted.
        assert_eq!(shell_quote("a;rm -rf /'x"), "'a;rm -rf /'\\''x'");
        assert_eq!(shell_quote("~alice/bin"), "'~alice/bin'");
    }

    #[test]
    fn posix_deploy_streams_to_unique_temp_then_moves() {
        let cmd = deploy_command("linux", "~/.cargo/bin/cp2");
        // The payload lands at a unique temp name (`$$` = remote shell pid)
        // and is moved into place; `chmod +x` runs after the move, so the
        // destination is never a truncated or non-executable binary.
        assert!(cmd.contains("mkdir -p ~/'.cargo/bin'"), "{cmd}");
        assert!(cmd.contains("cat > ~/'.cargo/bin/cp2'.tmp.$$"), "{cmd}");
        assert!(
            cmd.contains("mv ~/'.cargo/bin/cp2'.tmp.$$ ~/'.cargo/bin/cp2'"),
            "{cmd}"
        );
        assert!(cmd.contains("chmod +x ~/'.cargo/bin/cp2'"), "{cmd}");
        // Nothing ever streams straight onto the final path, and the tilde is
        // never followed by a quoted string (which would suppress expansion).
        assert!(!cmd.contains("cat > ~/'.cargo/bin/cp2' &&"), "{cmd}");
        assert!(!cmd.contains("~'/.cargo/bin/cp2'"), "{cmd}");
    }

    #[test]
    fn normalize_uname_output() {
        assert_eq!(
            normalize_platform("Linux", "x86_64"),
            Some(("linux".to_string(), "x86_64".to_string()))
        );
        assert_eq!(
            normalize_platform("Darwin", "arm64"),
            Some(("macos".to_string(), "aarch64".to_string()))
        );
        assert_eq!(
            normalize_platform("Windows", "AMD64"),
            Some(("windows".to_string(), "x86_64".to_string()))
        );
        assert_eq!(normalize_platform("Linux", "mips64"), None);
        assert_eq!(normalize_platform("FreeBSD", "x86_64"), None);
    }
}
