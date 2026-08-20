//! Pure-Rust SSH transport via `russh` — the default on Windows, where
//! OpenSSH's `ControlMaster` multiplexing is unusable (`getsockname failed:
//! Not a socket`). It is also the transport mobile embeddings (iOS /
//! Android) will build on: one connection, one authentication, one channel
//! per session (platform probe, deploy, sync) — RFC 4254 channel reuse, no
//! `ControlMaster` machinery. On Unix the system `ssh` transport
//! (`super::ssh`) is the only one — there is no russh fallback there.
//!
//! Auth coverage (OpenSSH order): key files (`~/.ssh/id_ed25519`,
//! `id_ecdsa`, `id_rsa`, encrypted keys prompt for a passphrase), OpenSSH
//! user certificates next to the key, the SSH agent (Unix socket, Windows
//! named pipe, or Pageant), keyboard-interactive, then password. GSSAPI and
//! FIDO security keys are not supported — system ssh covers those on Unix.
//!
//! Host keys follow OpenSSH semantics: `~/.ssh/known_hosts` (plain,
//! `|1|`-hashed, and `@revoked` entries; a host with several keys — rotated
//! or replaced — is accepted when any entry matches) with
//! trust-on-first-use, or an OpenSSH host certificate verified against the
//! file's `@cert-authority` entries (signature, validity window, principals,
//! and critical options).
//!
//! `--jump-host` implements OpenSSH `ProxyJump`: the target connection rides
//! a `direct-tcpip` channel through the jump host (`connect_stream` over the
//! jump channel), keeping both connections authenticated and alive for the
//! session.

use crate::target::RemoteTarget;
use std::path::Path;
use crate::transport::ssh::{
    deploy_command, deploy_payload, parse_remote_version, parse_uname, remote_command,
};
use crate::transport::{JumpHost, Sudo};
use anyhow::{anyhow, bail, Context as _};
use base64::Engine as _;
use russh::client::{self, Config, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::ssh_key::{self, certificate::CertType, public::KeyData, Fingerprint, HashAlg, PublicKey};
use russh::keys::{self, PrivateKeyWithHashAlg, PublicKeyBase64};
use russh::ChannelMsg;
use ssh_key::known_hosts::{Entry, HostPatterns, Marker};
use zeroize::Zeroize;
use std::io::{IsTerminal, Write};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Shared connection tuning: keepalive so long syncs survive idle NATs and
/// firewalls, and `TCP_NODELAY` for the chatty protocol frames.
fn ssh_config() -> Arc<Config> {
    Arc::new(Config {
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..Config::default()
    })
}

/// The client handler: host-key policy for one connection (jump host or
/// target), evaluated inside russh's event loop.
pub(crate) struct ClientHandler {
    host: String,
    port: u16,
}

impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        host_key_ok(&self.host, self.port, server_public_key)
    }

    async fn auth_banner(
        &mut self,
        banner: &str,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        if !banner.trim().is_empty() {
            let mut stderr = std::io::stderr();
            let _ = stderr.write_all(banner.as_bytes());
            let _ = stderr.flush();
        }
        Ok(())
    }
}

/// OpenSSH host-key policy for `key` presented by `host`:
///
/// - a host **certificate** is verified against `@cert-authority` entries in
///   `~/.ssh/known_hosts` (signature, validity, principals, critical options);
/// - a plain key is checked against `~/.ssh/known_hosts` (plain, `|1|`-hashed,
///   and `@revoked` entries): accepted when **any** entry for the host holds
///   the presented key (a host may legitimately rotate keys — old and new
///   entries coexist), rejected when entries exist but none matches, and
///   prompted trust-on-first-use when there are no entries at all.
///
/// Malformed or unsupported lines are skipped with a warning, never fatal.
/// The host is matched by its literal name and its resolved addresses
/// (OpenSSH semantics).
///
/// # Errors
///
/// Returns an error (rejecting the key) when the key is unknown and the user
/// declines, the key changed or was revoked, or a certificate fails
/// validation.
fn host_key_ok(host: &str, port: u16, key: &PublicKey) -> anyhow::Result<bool> {
    match key.key_data() {
        KeyData::Certificate(cert) => verify_host_certificate(host, cert),
        _ => match check_plain_host_key(host, port, key)? {
            KnownHostCheck::Accepted => Ok(true),
            KnownHostCheck::Changed { line } => bail!(
                "REMOTE HOST IDENTIFICATION HAS CHANGED for host '{host}' \
                 (known_hosts line {line}). Possible man-in-the-middle attack."
            ),
            KnownHostCheck::Revoked { line } => bail!(
                "host key for '{host}' was revoked in known_hosts (line {line}); refusing to connect"
            ),
            KnownHostCheck::Unknown => trust_on_first_use(host, port, key),
        },
    }
}

/// The outcome of matching a presented host key against the `known_hosts`
/// entries for the host.
enum KnownHostCheck {
    /// Some entry for the host holds this exact key.
    Accepted,
    /// Entries exist for the host but none matches the presented key
    /// (`line` is the first such entry).
    Changed { line: usize },
    /// The host's key was explicitly revoked (`@revoked` entry).
    Revoked { line: usize },
    /// No entries at all for the host: trust on first use applies.
    Unknown,
}

/// Match `key` against the `~/.ssh/known_hosts` entries for `host`,
/// following OpenSSH semantics: unparseable or unsupported lines are skipped
/// with a warning, any matching entry accepts, a host with entries but no
/// match is a key change, and a host with no entries at all is unknown
/// (trust-on-first-use). `@revoked` entries reject their key outright, and
/// a revocation wins even over a later plain acceptance.
fn check_plain_host_key(host: &str, port: u16, key: &PublicKey) -> anyhow::Result<KnownHostCheck> {
    // The file read and the address resolution are synchronous (the latter
    // may hit DNS); keep them off the event loop (the `block_in_place`
    // idiom).
    let (content, candidates) =
        tokio::task::block_in_place(|| -> anyhow::Result<(Option<String>, Vec<String>)> {
            Ok((read_known_hosts()?, host_candidates(host, port)))
        })?;
    let Some(content) = content else {
        return Ok(KnownHostCheck::Unknown);
    };
    Ok(match_known_hosts(&content, &candidates, key))
}

/// The core of [`check_plain_host_key`]: walk the parsed entries of a
/// `known_hosts` file and decide the verdict for `key` against any of the
/// `candidates` host strings. Extracted for testability.
fn match_known_hosts(content: &str, candidates: &[String], key: &PublicKey) -> KnownHostCheck {
    let mut first_entry: Option<usize> = None;
    let mut revoked_line: Option<usize> = None;
    let mut accepted = false;
    for (line_no, raw) in content.lines().enumerate() {
        // Strip comments and blank lines (sshd(8) semantics) before parsing.
        let line = raw.split_once('#').map_or(raw, |(head, _)| head).trim();
        if line.is_empty() {
            continue;
        }
        let entry = match line.parse::<Entry>() {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!("skipping malformed known_hosts line {}: {e}", line_no + 1);
                continue;
            }
        };
        // `@cert-authority` entries belong to the certificate path.
        if matches!(entry.marker(), Some(Marker::CertAuthority)) {
            continue;
        }
        if !entry_matches_host(&entry, candidates) {
            continue;
        }
        first_entry.get_or_insert(line_no + 1);
        if entry.public_key() == key {
            accepted = true;
        }
        if matches!(entry.marker(), Some(Marker::Revoked)) && entry.public_key() == key {
            revoked_line = Some(line_no + 1);
        }
    }
    // Revocation is checked independently of the key matching (OpenSSH
    // semantics), so it wins even when a plain entry also accepts the key.
    if let Some(line) = revoked_line {
        return KnownHostCheck::Revoked { line };
    }
    if accepted {
        return KnownHostCheck::Accepted;
    }
    match first_entry {
        Some(line) => KnownHostCheck::Changed { line },
        None => KnownHostCheck::Unknown,
    }
}

/// Whether a `known_hosts` entry's host patterns cover any of the candidate
/// host strings (the literal host and its resolved addresses). A
/// `!`-prefixed pattern negates the whole entry (OpenSSH semantics): the
/// entry applies only when some positive pattern matches and no negated
/// pattern does.
fn entry_matches_host(entry: &Entry, candidates: &[String]) -> bool {
    match entry.host_patterns() {
        HostPatterns::Patterns(patterns) => candidates.iter().any(|candidate| {
            patterns_match_host(candidate, patterns.iter().map(String::as_str))
        }),
        HostPatterns::HashedName { salt, hash } => candidates
            .iter()
            .any(|candidate| hmac_sha1_verify(salt, hash, candidate.as_bytes())),
    }
}

/// Match `host` against a list of individual `known_hosts` host patterns
/// (comma-separated alternatives, each possibly `!`-negated): a `!` prefix
/// negates the whole list (OpenSSH semantics — the entry does not apply when
/// any negated pattern matches). `|1|`-hashed patterns cannot be matched by
/// name — they are skipped with a warning instead of silently treating the
/// literal `|1|...` string as a hostname.
fn patterns_match_host<'a>(host: &str, patterns: impl IntoIterator<Item = &'a str>) -> bool {
    let mut negated = false;
    let mut positive = false;
    for pattern in patterns {
        if let Some(rest) = pattern.strip_prefix('!') {
            if glob_match(host, rest) {
                negated = true;
            }
        } else if pattern.starts_with("|1|") {
            tracing::warn!(
                "skipping hashed pattern '{pattern}': hashed patterns cannot be matched by name"
            );
        } else if glob_match(host, pattern) {
            positive = true;
        }
    }
    positive && !negated
}

/// The candidate host strings to match against `known_hosts`: the literal
/// host plus its resolved addresses (OpenSSH checks both), in the canonical
/// `known_hosts` form — bare hostname (IPv6 literals bracketed) at port 22,
/// `[host]:port` otherwise. May block on DNS — callers run it off the event
/// loop.
fn host_candidates(host: &str, port: u16) -> Vec<String> {
    let mut names = vec![host.to_string()];
    if let Ok(addrs) = (host, 0u16).to_socket_addrs() {
        for addr in addrs {
            let ip = addr.ip().to_string();
            if !names.contains(&ip) {
                names.push(ip);
            }
        }
    }
    if port == 22 {
        names
            .into_iter()
            .map(|name| {
                if name.contains(':') {
                    format!("[{name}]")
                } else {
                    name
                }
            })
            .collect()
    } else {
        names
            .into_iter()
            .map(|name| format!("[{name}]:{port}"))
            .collect()
    }
}

/// Verify an HMAC-SHA1 (RFC 2104) tag over `data` with the entry's `salt` —
/// the `|1|salt|hash|` `known_hosts` scheme. The crate has no SHA-1
/// dependency, so the fixed digest is implemented in [`sha1`]; this is only
/// used for host-key matching, never for key derivation.
fn hmac_sha1_verify(salt: &[u8], expected: &[u8; 20], data: &[u8]) -> bool {
    const BLOCK: usize = 64;
    let mut key = salt.to_vec();
    if key.len() > BLOCK {
        key = sha1(&key).to_vec();
    }
    key.resize(BLOCK, 0);
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for (byte, key_byte) in ipad.iter_mut().zip(key.iter()) {
        *byte ^= key_byte;
    }
    for (byte, key_byte) in opad.iter_mut().zip(key.iter()) {
        *byte ^= key_byte;
    }
    let mut inner = ipad.to_vec();
    inner.extend_from_slice(data);
    let inner_digest = sha1(&inner);
    let mut outer = opad.to_vec();
    outer.extend_from_slice(&inner_digest);
    sha1(&outer) == *expected
}

/// SHA-1 (FIPS 180-4) message digest.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut state = [
        0x6745_2301u32,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut chunks = data.chunks_exact(64);
    for block in &mut chunks {
        let block: &[u8; 64] = block.try_into().unwrap();
        sha1_compress(&mut state, block);
    }
    let rem_bytes = chunks.remainder();
    let rem = rem_bytes.len();
    let bit_len = u64::try_from(data.len())
        .unwrap_or(u64::MAX)
        .wrapping_mul(8);
    let mut final_block = [0u8; 64];
    final_block[..rem].copy_from_slice(rem_bytes);
    final_block[rem] = 0x80;
    if rem < 56 {
        final_block[56..].copy_from_slice(&bit_len.to_be_bytes());
        sha1_compress(&mut state, &final_block);
    } else {
        sha1_compress(&mut state, &final_block);
        let mut tail = [0u8; 64];
        tail[56..].copy_from_slice(&bit_len.to_be_bytes());
        sha1_compress(&mut state, &tail);
    }
    let mut digest = [0u8; 20];
    for (word, slot) in state.iter().zip(digest.chunks_exact_mut(4)) {
        slot.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// One SHA-1 compression round over a 64-byte block, mutating the 160-bit
/// state.
fn sha1_compress(state: &mut [u32; 5], block: &[u8; 64]) {
    let mut words = [0u32; 80];
    for (word, bytes) in words[..16].iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes(bytes.try_into().unwrap());
    }
    for idx in 16..80 {
        words[idx] = (words[idx - 3] ^ words[idx - 8] ^ words[idx - 14] ^ words[idx - 16])
            .rotate_left(1);
    }
    let (mut h0, mut h1, mut h2, mut h3, mut h4) =
        (state[0], state[1], state[2], state[3], state[4]);
    for (round, &word) in words.iter().enumerate() {
        let (f, k) = match round {
            0..=19 => ((h1 & h2) | (!h1 & h3), 0x5A82_7999u32),
            20..=39 => (h1 ^ h2 ^ h3, 0x6ED9_EBA1),
            40..=59 => ((h1 & h2) | (h1 & h3) | (h2 & h3), 0x8F1B_BCDC),
            _ => (h1 ^ h2 ^ h3, 0xCA62_C1D6),
        };
        let temp = h0
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(h4)
            .wrapping_add(k)
            .wrapping_add(word);
        h4 = h3;
        h3 = h2;
        h2 = h1.rotate_left(30);
        h1 = h0;
        h0 = temp;
    }
    state[0] = state[0].wrapping_add(h0);
    state[1] = state[1].wrapping_add(h1);
    state[2] = state[2].wrapping_add(h2);
    state[3] = state[3].wrapping_add(h3);
    state[4] = state[4].wrapping_add(h4);
}

/// Verify an OpenSSH host certificate against the `@cert-authority` entries
/// whose pattern matches `host`: CA fingerprint, signature, validity window,
/// principals, and critical options (OpenSSH semantics — a host cert without
/// a matching CA is rejected, never trusted on first use).
///
/// # Errors
///
/// Returns an error when no CA matches, validation fails, the certificate
/// carries unsupported critical options, or the host is not among the
/// certificate's principals.
fn verify_host_certificate(host: &str, cert: &ssh_key::Certificate) -> anyhow::Result<bool> {
    let cas = cert_authorities_for(host)?;
    if cas.is_empty() {
        bail!(
            "no @cert-authority entry in known_hosts matches host '{host}'; \
             cannot verify its host certificate"
        );
    }
    if cert.cert_type() != CertType::Host {
        bail!(
            "host '{host}' presented a certificate of type {:?}, not a host certificate",
            cert.cert_type()
        );
    }
    cert.validate(cas.iter())
        .map_err(|e| anyhow!("host certificate for '{host}' failed validation: {e}"))?;
    // ssh-key's `validate` deliberately leaves critical options to the
    // caller: every option is "critical", so any present option is
    // unsupported here and the certificate must be refused.
    if !cert.critical_options().is_empty() {
        bail!(
            "host certificate for '{host}' carries unsupported critical options: {:?}",
            cert.critical_options()
        );
    }
    if !cert.valid_principals().is_empty()
        && !cert
            .valid_principals()
            .iter()
            .any(|principal| principal == host)
    {
        bail!(
            "host certificate for '{host}' does not list '{host}' among its principals"
        );
    }
    Ok(true)
}

/// CA fingerprints from `@cert-authority` lines in `~/.ssh/known_hosts`
/// whose host pattern matches `host`.
fn cert_authorities_for(host: &str) -> anyhow::Result<Vec<Fingerprint>> {
    // The known_hosts read is synchronous; keep it off the event loop.
    let Some(content) = tokio::task::block_in_place(read_known_hosts)? else {
        return Ok(Vec::new());
    };
    let mut cas = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if !line.starts_with("@cert-authority") {
            continue;
        }
        let mut fields = line.split_whitespace();
        fields.next(); // "@cert-authority"
        let (Some(patterns), Some(_key_type), Some(b64)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !host_matches_pattern(host, patterns) {
            continue;
        }
        match keys::parse_public_key_base64(b64) {
            Ok(ca) => cas.push(Fingerprint::new(HashAlg::Sha256, ca.key_data())),
            Err(e) => tracing::warn!(
                "skipping malformed @cert-authority line matching '{host}': {e}"
            ),
        }
    }
    Ok(cas)
}

/// OpenSSH `known_hosts` pattern matching for `@cert-authority` lines, whose
/// host field is a comma-separated list (`*`/`?` wildcards, `!` negation,
/// `|1|`-hashed patterns warned-and-skipped).
fn host_matches_pattern(host: &str, pattern: &str) -> bool {
    patterns_match_host(host, pattern.split(','))
}

/// Wildcard match supporting `*` (any run) and `?` (any single char).
fn glob_match(host: &str, pattern: &str) -> bool {
    fn rec(host: &[char], pattern: &[char]) -> bool {
        match (host.first(), pattern.first()) {
            (None, None) => true,
            (_, Some('*')) => rec(host, &pattern[1..]) || (!host.is_empty() && rec(&host[1..], pattern)),
            (Some(_), Some('?')) => rec(&host[1..], &pattern[1..]),
            (Some(a), Some(b)) if a == b => rec(&host[1..], &pattern[1..]),
            _ => false,
        }
    }
    let host: Vec<char> = host.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    rec(&host, &pattern)
}

/// Trust-on-first-use prompt, ssh-style. The prompt reads the terminal from a
/// blocking task (the event loop must not stall on stdin), and the accepted
/// key is recorded into `~/.ssh/known_hosts`.
///
/// # Errors
///
/// Returns an error when the user declines (or there is no terminal to ask).
fn trust_on_first_use(host: &str, port: u16, key: &PublicKey) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "host key for '{host}' is not in known_hosts and no terminal is \
             available to confirm it (run with an interactive terminal, or add \
             it with `ssh-keyscan`/ssh)"
        );
    }
    eprintln!("The authenticity of host '{host}' can't be established.");
    eprintln!(
        "{} key fingerprint is SHA256:{}.",
        key.algorithm(),
        fingerprint_b64(&Fingerprint::new(HashAlg::Sha256, key.key_data()))
    );
    eprint!("Are you sure you want to continue connecting (yes/no)? ");
    std::io::stderr().flush()?;
    let answer = tokio::task::block_in_place(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_ascii_lowercase()
    });
    if answer == "yes" {
        // The known_hosts write is synchronous; keep it off the event loop.
        tokio::task::block_in_place(|| learn_known_hosts(host, port, key))?;
        Ok(true)
    } else {
        bail!("host key verification failed for '{host}'");
    }
}

/// The OpenSSH-style `SHA256:<base64>` fingerprint of a key.
fn fingerprint_b64(fingerprint: &Fingerprint) -> String {
    let bytes: &[u8] = match fingerprint {
        Fingerprint::Sha256(bytes) => bytes,
        Fingerprint::Sha512(bytes) => bytes,
        _ => &[], // non_exhaustive: future hash algorithms
    };
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}

/// The authenticated connection shared by every operation of a run (platform
/// probe, version check, deploy, sync session): one connection, one
/// authentication, one channel per operation — so password auth prompts at
/// most once per run. A jump host's password is reused for the target when
/// the target needs one.
pub(crate) struct RusshConnection {
    /// The authenticated target connection; operations open channels on it.
    pub(crate) handle: Handle<ClientHandler>,
    // Held only for its Drop side effect: keeping the jump connection alive
    // (and authenticated) for the whole run. Never read.
    #[expect(dead_code, reason = "drop-order anchor for the jump connection")]
    jump_handle: Option<Handle<ClientHandler>>,
}

/// A password or passphrase held for the duration of `connect`: zeroized on
/// drop, whatever exit path the connection attempt takes — including the
/// early `?` returns below, which used to drop the plain `String`s
/// unzeroized.
struct SecretVault(Option<String>);

impl SecretVault {
    fn new(secret: Option<String>) -> Self {
        Self(secret)
    }

    /// The stored secret, if any.
    fn get(&self) -> Option<&str> {
        self.0.as_deref()
    }

    /// Replace the stored secret.
    fn set(&mut self, secret: String) {
        self.0 = Some(secret);
    }

    /// Whether a secret is stored.
    fn is_none(&self) -> bool {
        self.0.is_none()
    }

    /// Copy another vault's secret into this one (the same-password
    /// assumption between jump host and target).
    fn clone_from(&mut self, other: &Self) {
        if let Some(secret) = &other.0 {
            self.0 = Some(secret.clone());
        }
    }
}

impl Drop for SecretVault {
    fn drop(&mut self) {
        if let Some(secret) = &mut self.0 {
            secret.zeroize();
        }
    }
}

/// A batch of secrets (keyboard-interactive answers) zeroized on drop, so an
/// error mid-collection cannot leave password data in memory.
struct SecretStrings(Vec<String>);

impl Drop for SecretStrings {
    fn drop(&mut self) {
        for secret in &mut self.0 {
            secret.zeroize();
        }
    }
}

/// Connect (optionally through a jump host) and authenticate once, returning
/// the run's shared connection. A `--password` value seeds the auth vault;
/// whatever ends up in it (the flag value or an entered prompt) is scrubbed
/// from memory once the connection attempt ends.
///
/// # Errors
///
/// Returns an error when connecting or authenticating fails (host key
/// rejected, auth exhausted, network).
pub(crate) async fn connect(
    peer: &RemoteTarget,
    jump: Option<&JumpHost>,
    password: Option<String>,
    jump_password: Option<String>,
) -> anyhow::Result<RusshConnection> {
    // Two slots: the target's password (`--password` or a prompt) and the
    // jump's (`--jump-password`). Without `--jump-password`, the jump reuses
    // the target value (same-password assumption), so a jump run still
    // prompts at most once. Both vaults zeroize on drop, so every exit path
    // below scrubs them.
    let mut target_vault = SecretVault::new(password);
    let mut jump_vault = SecretVault::new(jump_password);
    if jump_vault.is_none() {
        jump_vault.clone_from(&target_vault);
    }
    let (handle, jump_handle) = if let Some(jump_host) = jump {
        let jump_handler = ClientHandler {
            host: jump_host.host.clone(),
            port: jump_host.port,
        };
        let jump_handle = connect_one(
            &jump_host.host,
            jump_host.port,
            &jump_host.user,
            jump_handler,
            &mut jump_vault,
        )
        .await?;
        // No target password was given and the jump prompted for one: reuse
        // it for the target (the common same-password case).
        if target_vault.is_none() {
            target_vault.clone_from(&jump_vault);
        }
        let channel = jump_handle
            .channel_open_direct_tcpip(peer.host.clone(), u32::from(peer.port), "127.0.0.1", 0)
            .await
            .context("failed to open a direct-tcpip channel to the target through the jump host")?;
        let stream = channel.into_stream();
        let handle = client::connect_stream(ssh_config(), stream, ClientHandler {
            host: peer.host.clone(),
            port: peer.port,
        })
        .await
        .context("failed to connect to the target through the jump host")?;
        (handle, Some(jump_handle))
    } else {
        let handle = client::connect(ssh_config(), (peer.host.as_str(), peer.port), ClientHandler {
            host: peer.host.clone(),
            port: peer.port,
        })
        .await
        .context("failed to connect")?;
        (handle, None)
    };
    let mut handle = handle;
    let auth = authenticate(
        &mut handle,
        &peer.user,
        &format!("{}@{}", peer.user, peer.host),
        &mut target_vault,
    )
    .await;
    // The passwords have been emitted (sent to the server) — the vaults
    // zeroize when they drop, whether authentication succeeded or not.
    auth?;
    Ok(RusshConnection {
        handle,
        jump_handle,
    })
}

/// Connect to one host and run the full authentication chain, sharing the
/// password vault with later hosts.
async fn connect_one(
    host: &str,
    port: u16,
    user: &str,
    handler: ClientHandler,
    vault: &mut SecretVault,
) -> anyhow::Result<Handle<ClientHandler>> {
    let mut handle = client::connect(ssh_config(), (host, port), handler)
        .await
        .context("failed to connect")?;
    authenticate(&mut handle, user, &format!("{user}@{host}"), vault).await?;
    Ok(handle)
}

/// The OpenSSH auth chain, in order: key files (with adjacent OpenSSH user
/// certificates), the SSH agent, keyboard-interactive, password. GSSAPI and
/// FIDO are not supported.
///
/// # Errors
///
/// Returns an error when every method is exhausted or a prompt fails.
async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    display: &str,
    vault: &mut SecretVault,
) -> anyhow::Result<()> {
    let rsa_hash = handle.best_supported_rsa_hash().await?.flatten();
    for path in candidate_keys() {
        if !path.is_file() {
            continue;
        }
        let key = match keys::load_secret_key(&path, None) {
            Ok(key) => key,
            Err(keys::Error::KeyIsEncrypted) => {
                // A failed prompt (headless, stdin consumed) is not fatal:
                // like the other key errors, skip this key and let the next
                // auth method be tried. The passphrase itself is held in a
                // zeroizing guard so every exit path scrubs it.
                let passphrase = match prompt_secret(&format!(
                    "Enter passphrase for {}: ",
                    path.display()
                )) {
                    Ok(passphrase) => passphrase,
                    Err(e) => {
                        tracing::debug!(
                            "skipping key {}: cannot read a passphrase: {e}",
                            path.display()
                        );
                        continue;
                    }
                };
                let passphrase = SecretVault::new(Some(passphrase));
                keys::load_secret_key(&path, passphrase.get())
                    .map_err(|e| anyhow!("failed to decrypt {}: {e}", path.display()))?
            }
            Err(e) => {
                tracing::debug!("skipping key {}: {e}", path.display());
                continue;
            }
        };
        let result = handle
            .authenticate_publickey(
                user,
                PrivateKeyWithHashAlg::new(Arc::new(key.clone()), rsa_hash),
            )
            .await?;
        if result.success() {
            return Ok(());
        }
        // The adjacent OpenSSH user certificate (`<key>-cert.pub`), if any.
        let cert_path = cert_path_for(&path);
        if cert_path.is_file() && let Ok(cert) = keys::load_openssh_certificate(&cert_path) {
            let result = handle
                .authenticate_openssh_cert(user, Arc::new(key), cert)
                .await?;
            if result.success() {
                return Ok(());
            }
        }
    }

    if let Some(mut agent) = connect_agent().await
        && let Ok(identities) = agent.request_identities().await
    {
        for identity in identities {
            let Some(key) = identity_public_key(&identity) else {
                continue;
            };
            // RSA agent identities need the negotiated rsa-sha2 hash too —
            // a server with an rsa-sha2-only policy rejects SHA-1 agent
            // signatures (the key-file path already uses `rsa_hash`).
            match handle
                .authenticate_publickey_with(user, key, rsa_hash, &mut agent)
                .await
            {
                Ok(result) if result.success() => return Ok(()),
                Ok(_) => {}
                Err(e) => tracing::debug!("agent key rejected: {e}"),
            }
        }
    }

    if let Err(e) = keyboard_interactive_auth(handle, user, vault).await {
        tracing::debug!("keyboard-interactive authentication failed: {e}");
        if password_auth(handle, user, display, vault).await? {
            return Ok(());
        }
    } else {
        return Ok(());
    }
    bail!("authentication failed for {display} (tried keys, agent, keyboard-interactive, password)")
}

/// The public key of an agent identity (certificates are tried via the
/// key-auth path only when a matching key file exists).
fn identity_public_key(identity: &russh::keys::agent::AgentIdentity) -> Option<PublicKey> {
    match identity {
        russh::keys::agent::AgentIdentity::PublicKey { key, .. } => Some(key.clone()),
        russh::keys::agent::AgentIdentity::Certificate { .. } => None,
    }
}

/// Connect to the SSH agent: the OpenSSH agent (Unix socket via
/// `SSH_AUTH_SOCK`; Windows named pipe), falling back to Pageant on Windows.
async fn connect_agent(
) -> Option<AgentClient<Box<dyn AgentStream + Send + Unpin>>> {
    #[cfg(not(windows))]
    {
        keys::agent::client::AgentClient::connect_env()
            .await
            .ok()
            .map(AgentClient::dynamic)
    }
    #[cfg(windows)]
    {
        let named_pipe = keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
            .await;
        match named_pipe {
            Ok(client) => Some(AgentClient::dynamic(client)),
            // The `or_else` closure would be sync and cannot await.
            Err(_) => keys::agent::client::AgentClient::connect_pageant()
                .await
                .ok()
                .map(AgentClient::dynamic),
        }
    }
}

/// Keyboard-interactive authentication: start, then answer each round of
/// prompts until success or failure.
///
/// # Errors
///
/// Returns an error when the server rejects the method or a prompt fails.
async fn keyboard_interactive_auth(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    vault: &mut SecretVault,
) -> anyhow::Result<()> {
    let mut response = handle.authenticate_keyboard_interactive_start(user, None).await?;
    loop {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(()),
            KeyboardInteractiveAuthResponse::Failure { .. } => {
                bail!("keyboard-interactive authentication failed")
            }
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                if !name.is_empty() {
                    eprintln!("{name}");
                }
                if !instructions.is_empty() {
                    eprintln!("{instructions}");
                }
                // The answers may hold the password; they are collected in a
                // zeroizing guard, so a mid-loop prompt failure scrubs what
                // was already collected, and the send moves them out — no
                // copy lingers after the response.
                let mut answers = SecretStrings(Vec::with_capacity(prompts.len()));
                for prompt in prompts {
                    let answer = if prompt.echo {
                        read_line_visible(&prompt.prompt)
                    } else if let Some(cached) = vault.get() {
                        cached.to_string()
                    } else {
                        let entered = prompt_secret(&prompt.prompt)?;
                        vault.set(entered.clone());
                        entered
                    };
                    answers.0.push(answer);
                }
                response = handle
                    .authenticate_keyboard_interactive_respond(std::mem::take(&mut answers.0))
                    .await?;
            }
        }
    }
}

/// Password authentication (the last resort, ssh's order). The password is
/// prompted once per run and reused for later hosts (jump + target).
async fn password_auth(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    display: &str,
    vault: &mut SecretVault,
) -> anyhow::Result<bool> {
    let mut password = if let Some(cached) = vault.get() {
        cached.to_string()
    } else {
        let entered = prompt_secret(&format!("{display}'s password: "))?;
        vault.set(entered.clone());
        entered
    };
    let result = handle.authenticate_password(user, &password).await;
    // The prompt/cached copy has been emitted — scrub it on every exit path.
    password.zeroize();
    Ok(result?.success())
}

/// Candidate identity files, OpenSSH order.
fn candidate_keys() -> Vec<PathBuf> {
    let dir = ssh_dir();
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .iter()
        .map(|name| dir.join(name))
        .collect()
}

/// The OpenSSH certificate path next to a key (`<key>-cert.pub`).
fn cert_path_for(key: &Path) -> PathBuf {
    let mut name = key.as_os_str().to_os_string();
    name.push("-cert.pub");
    PathBuf::from(name)
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn ssh_dir() -> PathBuf {
    home_dir().join(".ssh")
}

fn known_hosts_path() -> PathBuf {
    ssh_dir().join("known_hosts")
}

/// Read `~/.ssh/known_hosts`, returning `None` when the file does not exist
/// (a host with no records at all). The single source of the file location
/// for every path that touches it (check, certificate authorities, learn),
/// so TOFU records and checks can never disagree.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read.
fn read_known_hosts() -> anyhow::Result<Option<String>> {
    let path = known_hosts_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("failed to read {}: {e}", path.display())),
    }
}

/// Record an accepted host key into `~/.ssh/known_hosts` (append, OpenSSH
/// format), at the same [`known_hosts_path`] the check path reads. Entries
/// are written in plaintext: OpenSSH's `HashKnownHosts` hashing is not
/// implemented on the write side (the check side reads and matches `|1|`
/// hashed entries fine; ssh-key exposes no hashing helper to write them).
///
/// # Errors
///
/// Returns an error when the file cannot be opened or written.
fn learn_known_hosts(host: &str, port: u16, key: &PublicKey) -> anyhow::Result<()> {
    let path = known_hosts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("failed to create {}: {e}", parent.display()))?;
    }
    // An IPv6 host (brackets already stripped by address parsing) must be
    // written bracketed, or the entry would be unreadable by any client.
    let host_entry = if host.contains(':') {
        if port == 22 {
            format!("[{host}]")
        } else {
            format!("[{host}]:{port}")
        }
    } else if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| anyhow!("failed to open {} for append: {e}", path.display()))?;
    writeln!(
        file,
        "{host_entry} {} {}",
        key.algorithm(),
        key.public_key_base64()
    )
    .map_err(|e| anyhow!("failed to record host key in {}: {e}", path.display()))?;
    Ok(())
}

/// Prompt for a secret (password or passphrase) without echo, off the event
/// loop.
///
/// # Errors
///
/// Returns an error when the terminal cannot be read.
fn prompt_secret(prompt: &str) -> anyhow::Result<String> {
    tokio::task::block_in_place(|| rpassword::prompt_password(prompt))
        .map_err(|e| anyhow!("failed to read secret: {e}"))
}

/// Read a visible line (echoed keyboard-interactive prompts).
///
/// # Errors
///
/// Returns an error when stdin cannot be read.
fn read_line_visible(prompt: &str) -> String {
    tokio::task::block_in_place(|| {
        let mut line = String::new();
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim_end().to_string()
    })
}

/// Run a remote command over a fresh session channel, optionally streaming a
/// payload to its stdin, and collect its exit status and stdout. Remote
/// stderr is forwarded to the local stderr so diagnostics (a missing
/// `uname`, shell errors) are not silently dropped.
///
/// # Errors
///
/// Returns an error when the channel or the remote execution fails, or when
/// the remote command does not finish within [`REMOTE_COMMAND_TIMEOUT`].
const REMOTE_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn exec_capture(
    handle: &Handle<ClientHandler>,
    command: &str,
    stdin_payload: Option<&[u8]>,
) -> anyhow::Result<(u32, String, String)> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command).await?;
    if let Some(payload) = stdin_payload {
        channel.data(payload).await?;
    }
    channel.eof().await?;
    // The payload has already been streamed above; only the exit wait is
    // bounded, so a remote command that never closes the channel (a missing
    // `uname`, a wedged shell) cannot hang probe/version/deploy forever.
    let (code, stdout, stderr) = tokio::time::timeout(
        REMOTE_COMMAND_TIMEOUT,
        async {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut code = None;
            loop {
                let Some(msg) = channel.wait().await else {
                    break;
                };
                match msg {
                    ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, ext } if ext == 1 => {
                        stderr.extend_from_slice(&data);
                    }
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    _ => {}
                }
            }
            (code.unwrap_or(255), stdout, stderr)
        },
    )
    .await
    .map_err(|_| anyhow!("remote command timed out after 10s: {command}"))?;
    Ok((
        code,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    ))
}

/// Quote a path for a POSIX remote shell, but only when it contains
/// characters the shell treats specially: a bare safe path (letters, digits,
/// `~`, `/`, `.`, `-`, `_`, `@`, `%`, `:`, `+`) passes through so `~` still
/// expands remotely; anything else is wrapped in single quotes, with an
/// embedded quote escaped as `'\''`.
fn sh_quote(path: &str) -> String {
    if path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '~' | '/' | '.' | '-' | '_' | '@' | '%' | ':' | '+'))
    {
        return path.to_string();
    }
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('\'');
    for c in path.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    quoted
}

/// Detect the remote platform over the shared connection, mirroring the
/// system-ssh probe.
///
/// # Errors
///
/// Returns an error when the channel or the remote execution fails.
pub(crate) async fn probe_on(handle: &Handle<ClientHandler>) -> anyhow::Result<Option<(String, String)>> {
    let (code, stdout, _stderr) = exec_capture(handle, "uname -s -m", None).await?;
    if code == 0 && let Some(platform) = parse_uname(&stdout) {
        return Ok(Some(platform));
    }
    // Windows fallback: `%PROCESSOR_ARCHITECTURE%` (AMD64/ARM64/x86).
    let (_, stdout, _stderr) = exec_capture(handle, "cmd /c echo %PROCESSOR_ARCHITECTURE%", None).await?;
    let arch = stdout.split_whitespace().next().unwrap_or("");
    let arch = match arch {
        "AMD64" => "x86_64",
        "ARM64" => "aarch64",
        _ => return Ok(None),
    };
    Ok(Some(("windows".to_string(), arch.to_string())))
}

/// Check the remote binary's version over the shared connection, mirroring
/// the system-ssh check.
///
/// `sudo` mirrors the system-ssh probe: `NonInteractive` prefixes the probe
/// with `sudo -n`, `Password` with `sudo -S` plus the password as the first
/// stdin line.
///
/// # Errors
///
/// Returns an error when the channel or the remote execution fails.
pub(crate) async fn check_version_on(
    handle: &Handle<ClientHandler>,
    remote_path: &str,
    remote_os: &str,
    sudo: Sudo,
    sudo_password: Option<&str>,
) -> anyhow::Result<Option<(String, Option<String>)>> {
    let mut command = match remote_os {
        "windows" => remote_command(remote_os, &format!("{remote_path} --version")),
        // POSIX branch: quote the user-supplied path so shell metacharacters
        // in `--remote-path` cannot inject into the remote command.
        _ => {
            let path = sh_quote(remote_path);
            format!("test -x {path} && {path} --version")
        }
    };
    let stdin_payload = match sudo {
        Sudo::NonInteractive => {
            command = format!("sudo -n {command}");
            None
        }
        Sudo::Password => {
            command = format!("sudo -S {command}");
            Some(format!("{}\n", sudo_password.unwrap_or_default()).into_bytes())
        }
        Sudo::None => None,
    };
    let (code, stdout, _stderr) = exec_capture(handle, &command, stdin_payload.as_deref()).await?;
    if code != 0 {
        return Ok(None);
    }
    Ok(parse_remote_version(&stdout))
}

/// Push the local binary to the remote over the shared connection,
/// mirroring the system-ssh deploy.
///
/// # Errors
///
/// Returns an error when the channel or the remote command fails.
pub(crate) async fn deploy_on(
    handle: &Handle<ClientHandler>,
    remote_path: &str,
    local_binary: &Path,
    remote_os: &str,
) -> anyhow::Result<()> {
    let payload = deploy_payload(remote_os, local_binary)?;
    let command = deploy_command(remote_os, remote_path);
    let (code, _stdout, _stderr) = exec_capture(handle, &command, Some(&payload)).await?;
    if code != 0 {
        bail!("failed to deploy cp2 to the server: remote command exited with {code}");
    }
    Ok(())
}

/// Open the long-lived sync session: the executor's byte-stream halves plus
/// the connection state whose `finish` waits for the channel to drain.
///
/// # Errors
///
/// Returns an error when connecting, authenticating, or opening the channel
/// fails.
pub(crate) async fn open_session_on(
    handle: &Handle<ClientHandler>,
    remote_path: &str,
    remote_os: &str,
    server_args: &str,
    sudo: Sudo,
    sudo_password: Option<&str>,
) -> anyhow::Result<(
    Box<dyn AsyncWrite + Unpin + Send>,
    Box<dyn AsyncRead + Unpin + Send>,
    RusshSession,
)> {
    // Quote the user-supplied `--remote-path` on POSIX remotes (shell
    // metacharacters must not inject into the server invocation); the
    // Windows branch keeps the raw form for `cmd /c`.
    let base = if remote_os == "windows" {
        format!("{remote_path} --server")
    } else {
        format!("{} --server", sh_quote(remote_path))
    };
    let base = if server_args.is_empty() {
        base
    } else {
        format!("{base} {server_args}")
    };
    let remote_cmd = match sudo {
        Sudo::None => base,
        Sudo::NonInteractive => format!("sudo -n {base}"),
        Sudo::Password => format!("sudo -S {base}"),
    };
    let remote_cmd = remote_command(remote_os, &remote_cmd);
    let channel = handle.channel_open_session().await?;
    channel.exec(true, remote_cmd).await?;

    let (read_half, write_half) = channel.split();
    let mut send: Box<dyn AsyncWrite + Unpin + Send> = Box::new(write_half.make_writer());
    if sudo == Sudo::Password {
        // `sudo -S` consumes exactly one stdin line as the password; write
        // it before any protocol frame so the frames pass through untouched.
        use tokio::io::AsyncWriteExt;
        let send_mut = send.as_mut();
        send_mut
            .write_all(format!("{}
", sudo_password.unwrap_or_default()).as_bytes())
            .await?;
    }
    let (bytes_tx, bytes_rx) = mpsc::channel::<bytes::Bytes>(64);
    // Forward the channel's incoming data into the mpsc; the executor reads
    // them as an AsyncRead. EOF (remote closed the channel) ends the task.
    let recv_task = tokio::spawn(async move {
        let mut read_half = read_half;
        let mut reader = read_half.make_reader();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if bytes_tx
                        .send(bytes::Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    // A helper task owns the write half and sends the channel EOF when the
    // session finishes. Without it the remote `--server` would keep waiting
    // for more input on a watch-pull Ctrl-C, and `finish` would burn its
    // whole timeout.
    let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = eof_rx.await;
        let _ = write_half.eof().await;
    });
    let recv: Box<dyn AsyncRead + Unpin + Send> = Box::new(RxBytes::new(bytes_rx));
    Ok((send, recv, RusshSession {
        recv_task,
        eof_trigger: Some(eof_tx),
    }))
}

/// Open the sync channel with the in-band platform preamble (the merged
/// single-session flow — the Windows client's probe + version-check channels
/// collapse into this one): the remote command prints `uname -s -m` + the
/// marker, then `exec`s the server; the client reads the platform from the
/// channel's stream before handing it to the executor. Returns `None` when
/// the remote does not speak the preamble (a Windows sshd — the caller
/// falls back to the classic probe+version+sync channels).
pub(crate) async fn open_preamble_on(
    handle: &Handle<ClientHandler>,
    remote_path: &str,
    server_args: &str,
) -> anyhow::Result<Option<(
    String,
    String,
    Box<dyn AsyncWrite + Unpin + Send>,
    Box<dyn AsyncRead + Unpin + Send>,
    RusshSession,
)>> {
    let remote_cmd = crate::transport::ssh::preamble_command(remote_path, server_args);
    let channel = handle.channel_open_session().await?;
    channel.exec(true, remote_cmd).await?;
    let (read_half, write_half) = channel.split();
    let send: Box<dyn AsyncWrite + Unpin + Send> = Box::new(write_half.make_writer());
    let (bytes_tx, bytes_rx) = mpsc::channel::<bytes::Bytes>(64);
    let recv_task = tokio::spawn(async move {
        let mut reader = read_half.make_reader();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if bytes_tx
                        .send(bytes::Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = eof_rx.await;
        let _ = write_half.eof().await;
    });
    let rx = RxBytes::new(bytes_rx);
    // The preamble: read the platform line + the marker off the channel
    // stream; the buffered remainder plus the live stream go to the
    // executor. EOF before the marker (a Windows sshd) → the classic flow.
    match crate::transport::ssh::read_preamble_platform(rx).await {
        Ok(Some((os, arch, reader))) => Ok(Some((
            os,
            arch,
            send,
            Box::new(reader),
            RusshSession {
                recv_task,
                eof_trigger: Some(eof_tx),
            },
        ))),
        Ok(None) => Ok(None),
        Err(_) => Ok(None),
    }
}

/// The merged deploy-and-serve over the russh connection: stream the
/// binary (the remote `head -c SIZE` consumes exactly the payload, then
/// the protocol frames follow on the same channel), exec it as the server,
/// and return the executor halves — the deploy channel *is* the sync
/// channel, and the Hello verifies the deployed binary.
pub(crate) async fn deploy_and_serve_on(
    handle: &Handle<ClientHandler>,
    remote_path: &str,
    server_args: &str,
    local_binary: &Path,
) -> anyhow::Result<(
    Box<dyn AsyncWrite + Unpin + Send>,
    Box<dyn AsyncRead + Unpin + Send>,
    RusshSession,
)> {
    use tokio::io::AsyncWriteExt;
    let payload = std::fs::read(local_binary)?;
    let remote_cmd =
        crate::transport::ssh::deploy_serve_command(remote_path, server_args, payload.len() as u64);
    let channel = handle.channel_open_session().await?;
    channel.exec(true, remote_cmd).await?;
    let (read_half, write_half) = channel.split();
    let mut send: Box<dyn AsyncWrite + Unpin + Send> = Box::new(write_half.make_writer());
    // The payload first; the protocol frames ride the same channel.
    send.write_all(&payload).await?;
    let (bytes_tx, bytes_rx) = mpsc::channel::<bytes::Bytes>(64);
    let recv_task = tokio::spawn(async move {
        let mut reader = read_half.make_reader();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if bytes_tx
                        .send(bytes::Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = eof_rx.await;
        let _ = write_half.eof().await;
    });
    let recv: Box<dyn AsyncRead + Unpin + Send> = Box::new(RxBytes::new(bytes_rx));
    Ok((
        send,
        recv,
        RusshSession {
            recv_task,
            eof_trigger: Some(eof_tx),
        },
    ))
}

/// The russh session state: the channel's read-forwarding task. The
/// connection itself belongs to the run's [`RusshConnection`] (shared by
/// probe/version/deploy/sync), so it stays open for the next operation —
/// `finish` only waits for this channel to drain.
#[expect(clippy::module_name_repetitions)]
pub struct RusshSession {
    recv_task: JoinHandle<()>,
    /// Fires the channel EOF (sent by the helper task spawned in
    /// [`open_session_on`]) so the remote sees our disconnect when the
    /// session finishes.
    eof_trigger: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for RusshSession {
    fn drop(&mut self) {
        // Never linger: a dropped session (aborted run) must not leave the
        // read-forwarding task — and the channel it borrows — alive.
        self.recv_task.abort();
        // Best-effort: wake the eof task so the remote sees the disconnect
        // even when `finish` was never called.
        if let Some(trigger) = self.eof_trigger.take() {
            let _ = trigger.send(());
        }
    }
}

impl RusshSession {
    /// Signal EOF on the channel (the remote exits promptly instead of
    /// waiting for more input — a watch-pull Ctrl-C used to burn the whole
    /// timeout below), then wait for the read-forwarding task. The transfer
    /// result is authoritative — transport failures already surfaced as
    /// stream errors — but the wait is bounded so a stuck peer cannot hang
    /// the run.
    ///
    /// # Errors
    ///
    /// Returns an error when the transport itself failed (auth denied, host
    /// key rejected, network).
    pub async fn finish<T>(&mut self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        // Best-effort EOF; harmless on a one-shot push/pull, where the
        // server has already exited and closed the channel.
        if let Some(trigger) = self.eof_trigger.take() {
            let _ = trigger.send(());
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), &mut self.recv_task).await;
        result
    }
}

/// Adapter turning an mpsc stream of `Bytes` into an `AsyncRead`; EOF when
/// the sender closes (the read-forwarding task ended).
struct RxBytes {
    rx: mpsc::Receiver<bytes::Bytes>,
    current: bytes::Bytes,
}

impl RxBytes {
    fn new(rx: mpsc::Receiver<bytes::Bytes>) -> Self {
        Self {
            rx,
            current: bytes::Bytes::new(),
        }
    }
}

impl AsyncRead for RxBytes {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.current.is_empty() {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => self.current = chunk,
                Poll::Ready(None) => return Poll::Ready(Ok(())), // EOF
                Poll::Pending => return Poll::Pending,
            }
        }
        let n = self.current.len().min(buf.remaining());
        buf.put_slice(&self.current[..n]);
        self.current = self.current.slice(n..);
        Poll::Ready(Ok(()))
    }
}
