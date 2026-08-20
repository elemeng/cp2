//! The main `cp2 SRC DST` command: direction inferred from which side is
//! remote, mirroring rsync. Remote transfers ride over `ssh` — the remote
//! side runs `cp2 --server` and sshd handles auth and permissions.

use super::{exit_if_partial, options_from_cli, print_skipped, watch};
use crate::cli::Cli;
use crate::protocol::{BUILD_FINGERPRINT, TargetOs};
use crate::sync::SyncStats;
use crate::sync::{Executor, ExecutorOptions};
use crate::target::{Location, RemoteTarget};
use crate::transport::ssh::{default_remote_path, local_platform, sidecar_candidates, sidecar_path};
use crate::transport::{JumpHost, RemoteClient, Session, SessionHandle, Transport};
use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};
use zeroize::Zeroize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Execute a sync between two locations, inferring push/pull direction.
///
/// # Errors
///
/// Returns an error if the arguments are incomplete, the ssh session or
/// binary deployment fails, or the sync itself fails.
pub async fn execute(cli: &mut Cli) -> Result<()> {
    // Positionals fill in order (SOURCE then DESTINATION). `--files-from`
    // entries are absolute paths, so SRC is not used — the single positional
    // argument is the destination.
    let (source, destination) = match (&cli.source, &cli.destination, cli.files_from.is_some()) {
        (Some(single), None, true) => (String::from("."), single.clone()),
        (Some(_), Some(_), true) => {
            return Err(anyhow::anyhow!(
                "--files-from entries are absolute paths; SRC is not used — pass only the destination"
            ))
        }
        (None, _, true) => {
            return Err(anyhow::anyhow!(
                "Missing DESTINATION. Usage: cp2 --files-from FILE DST"
            ))
        }
        (Some(src), Some(dst), false) => (src.clone(), dst.clone()),
        (Some(_), None, false) => {
            return Err(anyhow::anyhow!(
                "Missing DESTINATION. Usage: cp2 [OPTIONS] SRC DST"
            ))
        }
        (None, _, false) => {
            return Err(anyhow::anyhow!("Missing SOURCE. Usage: cp2 [OPTIONS] SRC DST"))
        }
    };

    let mut src = Location::parse(&source);
    let mut dst = Location::parse(&destination);

    // A remote target with an empty user or host (`@host:path`, `user@:path`)
    // would reach ssh as a malformed destination and fail confusingly — reject
    // it up front. (The parser cannot error; it stays infallible.)
    for target in [&src, &dst] {
        if let Location::Remote(remote) = target
            && (remote.user.is_empty() || remote.host.is_empty())
        {
            anyhow::bail!(
                "invalid remote target '{target}': user and host must be non-empty \
                 (expected user@host[:path])"
            );
        }
    }

    // The parser decides up front whether the source is a directory: a
    // directory runs the full tree pipeline (recursion, globs, --files-from,
    // --watch); a plain file is handled directly as a single-file sync and
    // never enters the directory machinery. A remote source is a directory
    // on the server (resolved there).
    let src_is_dir = match &src {
        Location::Local(path) => path.is_dir(),
        Location::Remote(_) => true,
    };

    // A directory source can expand into several top-level entries of one
    // run: a quoted glob (`'./*.rs'`) or a `--files-from` list. Remote
    // sources are never expanded — the remote side has no such support, and
    // a remote path that literally contains `*` fails with a normal
    // not-found error.
    let source_multi: Option<(PathBuf, Vec<PathBuf>)> = match &src {
        Location::Local(path) => {
            if let Some(list_file) = &cli.files_from {
                // Entries are absolute paths; each syncs to the destination
                // mirroring its root-relative structure (`/data/a.txt` →
                // `DST/data/a.txt`). The base is the filesystem root, so
                // SRC is not used.
                let content = std::fs::read_to_string(list_file).map_err(|e| {
                    anyhow::anyhow!("failed to read --files-from list {}: {e}", list_file.display())
                })?;
                let mut roots = Vec::new();
                for entry in parse_file_list(&content) {
                    if !Path::new(&entry).is_absolute() {
                        anyhow::bail!(
                            "--files-from entries must be absolute paths, got '{entry}'"
                        );
                    }
                    let full = PathBuf::from(&entry);
                    if full.exists() {
                        roots.push(full);
                    } else {
                        eprintln!("warning: {entry}: no such file or directory (--files-from)");
                    }
                }
                let base = roots
                    .first()
                    .map_or_else(|| PathBuf::from("/"), |r| path_root(r));
                if roots.iter().any(|r| !r.starts_with(&base)) {
                    anyhow::bail!("--files-from entries must be on the same filesystem root");
                }
                Some((base, roots))
            } else if !src_is_dir {
                // A plain file: synced directly (single-file transfer). A
                // glob pattern that does not exist literally still expands —
                // it may match several files.
                match path.to_str() {
                    Some(pattern) if has_glob_metachars(pattern) && !path.exists() => {
                        expand_source(pattern)?
                    }
                    _ => None,
                }
            } else {
                match path.to_str() {
                    Some(pattern) => expand_source(pattern)?,
                    None => None, // non-UTF-8 path: never a pattern
                }
            }
        }
        Location::Remote(_) => None,
    };
    if source_multi.is_some() && cli.watch.is_some() {
        anyhow::bail!(
            "--watch is not supported with glob or --files-from sources; quote a single directory instead"
        );
    }
    if let Some(port) = cli.port {
        if let Location::Remote(r) = &mut src {
            r.port = port;
        }
        if let Location::Remote(r) = &mut dst {
            r.port = port;
        }
    }

    let mut options = options_from_cli(cli);
    if !cli.quiet {
        install_progress(&mut options);
    }

    // The transport (system ssh or the pure-Rust russh client), the
    // optional jump host, and the optional --password are resolved once per
    // run. The password moves into the transport client (which scrubs it
    // once emitted) — the CLI copy is consumed by the move.
    let transport = Transport::default_transport();
    let password = resolve_password(cli)?;
    let mut client = RemoteClient::new(
        transport,
        password,
        cli.jump_password.take(),
        cli.remote_sudo,
        cli.sudo_password.take(),
    );
    let jump = cli.jump_host.as_deref().map(JumpHost::parse).transpose()?;

    // The remote target's `:path` resolves on the server (relative to its
    // serve root): push target for `SRC local, DST remote`, pull source for
    // the reverse.
    match (&src, &dst) {
        (Location::Local(_), Location::Remote(r)) | (Location::Remote(r), Location::Local(_)) => {
            options.remote_path = r.path.clone();
        }
        _ => {}
    }

    match (src, dst) {
        // Local → remote: push.
        (Location::Local(src_path), Location::Remote(remote)) => {
            if cli.dry_run {
                if !cli.quiet {
                    println!("Dry run: would push {} to {remote}", src_path.display());
                }
                return Ok(());
            }
            if cli.watch.is_some() {
                if !src_path.is_dir() {
                    anyhow::bail!("--watch requires SRC to be a directory");
                }
                return Box::pin(watch::watch_push(cli, &remote, &src_path, &options, client, jump.clone()))
                    .await;
            }
            let stats = match &source_multi {
                Some((base, roots)) => {
                    push_multi_via_ssh(
                        &remote,
                        base,
                        roots,
                        &options,
                        cli.remote_path.as_deref(),
                        cli.binaries_dir.as_deref(),
                        !cli.no_auto_install,
                        cli.quiet,
                        &mut client,
                        jump.as_ref(),
                    )
                    .await?
                }
                None => {
                    push_via_ssh(
                        &remote,
                        &src_path,
                        &options,
                        cli.remote_path.as_deref(),
                        cli.binaries_dir.as_deref(),
                        !cli.no_auto_install,
                        cli.quiet,
                        &mut client,
                        jump.as_ref(),
                    )
                    .await?
                }
            };
            if !cli.quiet {
                println!(
                    "Done: {} files, {} bytes transferred",
                    stats.files_sent, stats.bytes_transferred
                );
            }
            exit_if_partial(print_skipped(&stats));
            Ok(())
        }
        // Remote → local: pull.
        (Location::Remote(remote), Location::Local(dst_path)) => {
            if cli.dry_run {
                if !cli.quiet {
                    println!("Dry run: would pull {remote} to {}", dst_path.display());
                }
                return Ok(());
            }
            // The client's local OS is the pull *target*: it rides in the
            // `PullRequest` so the server-side sender can build the
            // permission/link matrices (spec §2.2 / §3.2).
            let (local_os, _) = local_platform();
            options.target_os = TargetOs::from_os_name(&local_os);
            if cli.watch.is_some() {
                return Box::pin(watch::watch_pull(cli, &remote, &dst_path, &options, client, jump.clone()))
                    .await;
            }
            let stats = pull_via_ssh(
                &remote,
                &dst_path,
                &options,
                cli.remote_path.as_deref(),
                cli.binaries_dir.as_deref(),
                !cli.no_auto_install,
                cli.quiet,
                &mut client,
                jump.as_ref(),
            )
            .await?;
            if !cli.quiet {
                println!(
                    "Done: {} files, {} bytes transferred",
                    stats.files_received, stats.bytes_transferred
                );
            }
            exit_if_partial(print_skipped(&stats));
            Ok(())
        }
        // Local → local: copy. Runs the same protocol pipeline as a push
        // (over a spawned `cp2 --server` child rooted at the destination), so
        // symlinks, hard links, metadata, and `--delete` behave identically.
        (Location::Local(src_path), Location::Local(dst_path)) => {
            if cli.dry_run {
                if !cli.quiet {
                    println!(
                        "Dry run: would copy {} to {}",
                        src_path.display(),
                        dst_path.display()
                    );
                }
                return Ok(());
            }
            // Same machine on both sides: the target OS is the local one.
            let (local_os, _) = local_platform();
            options.target_os = TargetOs::from_os_name(&local_os);
            if cli.watch.is_some() {
                if !src_path.is_dir() {
                    anyhow::bail!("--watch requires SRC to be a directory");
                }
                return watch::watch_local(cli, &src_path, &dst_path, &options).await;
            }
            let stats = match &source_multi {
                Some((base, roots)) => {
                    watch::push_multi_local_over_server(base, roots, &dst_path, &options).await?
                }
                None => watch::push_local_over_server(&src_path, &dst_path, &options).await?,
            };
            if !cli.quiet {
                println!(
                    "Done: {} files, {} bytes transferred",
                    stats.files_sent, stats.bytes_transferred
                );
            }
            exit_if_partial(print_skipped(&stats));
            Ok(())
        }
        // Remote → remote: not supported.
        (Location::Remote(_), Location::Remote(_)) => {
            anyhow::bail!("Remote-to-remote sync is not supported")
        }
    }
}

/// Resolve the target password from `--password` or `--password-file`
/// (first line, trailing newline stripped). Mutually exclusive; the file
/// channel keeps the secret off the command line (only the path rides argv).
/// The flag's own copy is consumed by the move (no leftover), and the file
/// buffer is scrubbed after the first line is extracted.
fn resolve_password(cli: &mut Cli) -> anyhow::Result<Option<String>> {
    match (cli.password.take(), cli.password_file.as_deref()) {
        (Some(p), Some(_)) => {
            let mut p = p;
            p.zeroize();
            anyhow::bail!("--password and --password-file are mutually exclusive");
        }
        (Some(p), None) => Ok(Some(p)),
        (None, Some(file)) => {
            let mut content = std::fs::read_to_string(file).map_err(|e| {
                anyhow::anyhow!("failed to read --password-file {}: {e}", file.display())
            })?;
            let password = content.lines().next().map(str::to_string);
            content.zeroize();
            Ok(password)
        }
        (None, None) => Ok(None),
    }
}

/// Parse a `--files-from` list: one path per line, delimited strictly by
/// newlines — both Unix (`\n`) and Windows (`\r\n`) line endings work
/// (`str::lines` strips the trailing `\r`). Blank lines are skipped; a path
/// may contain spaces, commas, or anything else.
fn parse_file_list(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// The filesystem root of an absolute path: `/` on Unix, the drive root
/// (`C:\`) on Windows.
fn path_root(path: &Path) -> PathBuf {
    path.ancestors()
        .last()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

/// Whether `s` contains a glob metacharacter (`*`, `?`, or `[`).
fn has_glob_metachars(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// The sync root for a glob pattern: the text before the first metachar,
/// up to (and including) the last path separator — i.e. the directory that
/// holds the matched entries. `./*` and `*.rs` both root at the working
/// directory (returned empty — matches are named relative to it); `src/*`
/// and `src/**/x.rs` root at `src`; `/*.rs` roots at `/`.
fn static_prefix_dir(pattern: &str) -> PathBuf {
    let prefix = &pattern[..pattern.find(['*', '?', '[']).unwrap_or(pattern.len())];
    let base = match prefix.rfind(['/', '\\']) {
        Some(idx) => &prefix[..idx],
        None => "",
    };
    if base.is_empty() {
        // `/*.rs` roots at the filesystem root; a bare or `./`-relative
        // pattern roots at the working directory (empty base).
        if prefix.starts_with('/') {
            PathBuf::from("/")
        } else {
            PathBuf::new()
        }
    } else if base == "." {
        PathBuf::new()
    } else {
        PathBuf::from(base)
    }
}

/// Expand a quoted glob source pattern into `(base, matches)`.
///
/// Returns `None` when the pattern has no metacharacters or names a path that
/// literally exists (literal paths always win — the escape hatch for
/// filenames that contain `*`/`?`/`[`). Returns an error when the pattern is
/// malformed or matches nothing.
fn expand_source(pattern: &str) -> anyhow::Result<Option<(PathBuf, Vec<PathBuf>)>> {
    if !has_glob_metachars(pattern) || Path::new(pattern).exists() {
        return Ok(None);
    }
    let mut matches: Vec<PathBuf> = glob::glob(pattern)
        .map_err(|e| anyhow::anyhow!("invalid glob pattern '{pattern}': {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("glob error for '{pattern}': {e}"))?;
    if matches.is_empty() {
        anyhow::bail!("no files match source pattern '{pattern}'");
    }
    matches.sort();
    let base = static_prefix_dir(pattern);
    Ok(Some((base, matches)))
}

/// Extra flags forwarded to the remote `cp2 --server` invocation.
///
/// Each flag is returned as its own token (never a path), so the callers
/// that need a single command string can `join(" ")` without losing
/// anything. These tune the remote receiver (push) or remote sender (pull):
/// an explicit `-j`, the `--max-delete`/`--backup`/`--fsync` receiver
/// behaviors, and `--checksum` (the receiver must hash its own tree so the
/// sender's planner can compare hashes on push) and `--storage`
/// (hash-worker tuning follows the user's override). Flags that affect the
/// *sender's* planning (`--existing`, `--ignore-times`) travel in the
/// `PullRequest` frame instead. Forwarded flags are always understood by the
/// remote because auto-deploy keeps the binaries in lockstep.
pub(crate) fn server_args(options: &ExecutorOptions) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(n) = options.jobs {
        // Flag and value are separate elements: the local `cp2 --server`
        // child passes them straight into argv, and the ssh path `join(" ")`
        // rebuilds `--jobs N` for the remote shell to re-split.
        args.push("--jobs".to_string());
        args.push(n.max(1).to_string());
    }
    if let Some(n) = options.max_delete {
        args.push("--max-delete".to_string());
        args.push(n.to_string());
    }
    // The server is the receiver on push; a `--delete` push must scan the
    // full destination so the client's planner can name the extras to
    // remove. On pull the server is the sender and ignores receiver-side
    // `--delete` in its argv (the PullRequest frame carries the flag).
    if options.delete {
        args.push("--delete".to_string());
    }
    if options.backup {
        args.push("--backup".to_string());
    }
    if options.fsync {
        args.push("--fsync".to_string());
    }
    if options.checksum {
        args.push("--checksum".to_string());
    }
    // The server is the receiver on push but the *sender* on pull, so each
    // flag must be forwarded only when the user actually set it: on a pull
    // the server's `--remove-source-files` deletes the remote source tree.
    // Coupling it to `--verify` would silently delete every verified file on
    // a verified-only pull — the two flags are independent.
    if options.remove_source_files {
        args.push("--remove-source-files".to_string());
    }
    if options.verify {
        args.push("--verify".to_string());
    }
    if options.rollsum {
        // Both peers must chunk the same way: the server is the receiver on
        // push and the sender on pull, so the flag rides the argv both ways.
        args.push("--rollsum".to_string());
    }
    if options.quiet {
        // The server's own summary line ("Synced ...") is silenced too.
        args.push("--quiet".to_string());
    }
    if options.archive {
        // The receiver (push) applies owner/group and creates specials; on
        // pull the server is the sender and needs the flag to send them. `-a`
        // also implies `--literal-links`, which the server derives the same
        // way from its own `--archive` — no need to forward it twice.
        args.push("--archive".to_string());
    } else if options.literal_links {
        // `--literal-links` alone (no `-a`): the server must scan its links
        // literally too — on a pull it is the *sender* whose literal link
        // targets the client quick-checks against.
        args.push("--literal-links".to_string());
    }
    // The `rlpt` opt-outs: the server must scan consistently (links,
    // recursion) and apply metadata consistently (perms, times), in both
    // directions.
    if !options.recursive {
        args.push("--no-recursive".to_string());
    }
    if !options.preserve_links {
        args.push("--skip-links".to_string());
    }
    // The link-policy flags travel to the server in both directions: on a
    // push the server receiver must classify its destination scan
    // consistently with the client's source scan (rewritten targets must
    // match for the quick check), and on a pull the server is the *sender*
    // and needs them to build its transfer list.
    if options.follow_links {
        args.push("--follow-links".to_string());
    }
    if options.literal_internal_links {
        args.push("--literal-internal-links".to_string());
    }
    if options.literal_external_file_links {
        args.push("--literal-external-file-links".to_string());
    }
    if options.literal_external_dir_links {
        args.push("--literal-external-dir-links".to_string());
    }
    if !options.preserve_perms {
        args.push("--no-perms".to_string());
    }
    if !options.preserve_times {
        args.push("--no-times".to_string());
    }
    // `-S`/`-X`/`-U`: receiver-side application (sparse writes, xattrs,
    // atime) and sender-side collection (xattrs) both live on the server
    // side of a push and on the client side of a pull — forward whenever
    // set so both roles agree.
    if options.sparse {
        args.push("--sparse".to_string());
    }
    if options.xattrs {
        args.push("--xattrs".to_string());
    }
    if options.atimes {
        args.push("--atimes".to_string());
    }
    if options.storage != crate::platform::storage::StoragePreference::Auto {
        args.push("--storage".to_string());
        args.push(options.storage.to_string());
    }
    args
}

/// Ensure the server has a matching `cp2` binary (unless disabled), then
/// push `src_path` to the remote over the chosen transport.
#[expect(clippy::too_many_arguments)]
async fn push_via_ssh(
    remote: &RemoteTarget,
    src_path: &Path,
    options: &ExecutorOptions,
    user_remote_path: Option<&str>,
    binaries_dir: Option<&Path>,
    auto_install: bool,
    quiet: bool,
    client: &mut RemoteClient,
    jump: Option<&JumpHost>,
) -> anyhow::Result<SyncStats> {
    let server_args = server_args(options).join(" ");
    match ensure_and_open(
        remote,
        user_remote_path,
        binaries_dir,
        auto_install,
        quiet,
        &server_args,
        client,
        jump,
    )
    .await?
    {
        // The merged single-session flow: run the sync on the preamble
        // session; a stale/missing server binary triggers a deploy + retry.
        Ensured::Merged {
            os,
            arch,
            remote_path,
            session,
        } => {
            let mut options = options.clone();
            options.target_os = TargetOs::from_os_name(&os);
            run_session_with_deploy(
                session,
                &server_args,
                auto_install,
                quiet,
                remote,
                &os,
                &arch,
                &remote_path,
                binaries_dir,
                client,
                jump,
                |send, recv| {
                    let src_path = src_path.to_path_buf();
                    let options = options.clone();
                    async move {
                        let mut executor = Executor::new(send, recv);
                        let result = executor
                            .push(&src_path, &options)
                            .await
                            .map_err(anyhow::Error::new);
                        // Close the channel so the remote sees EOF.
                        drop(executor);
                        result
                    }
                },
            )
            .await
        }
        Ensured::Classic { os, remote_path } => {
            push_over_ssh(remote, src_path, options, &os, &remote_path, client, jump).await
        }
    }
}

/// Push over the chosen transport to an already-resolved remote (platform
/// probed, binary ensured). Shared by one-shot pushes and the `--watch` loop,
/// which reuses the resolution and only re-spawns the session per sync.
pub(crate) async fn push_over_ssh(
    remote: &RemoteTarget,
    src_path: &Path,
    options: &ExecutorOptions,
    os: &str,
    remote_path: &str,
    client: &mut RemoteClient,
    jump: Option<&JumpHost>,
) -> anyhow::Result<SyncStats> {
    let session = client.open_session(remote, remote_path, os, &server_args(options).join(" "), jump).await?;
    let (send, recv, mut handle) = session.into_parts();
    let mut executor = Executor::new(send, recv);

    // The probed remote OS is the *target* for the permission/link matrices
    // (spec §2.2 / §3.2) — the sender decides at scan time.
    let mut options = options.clone();
    options.target_os = TargetOs::from_os_name(os);
    let result = executor
        .push(src_path, &options)
        .await
        .map_err(anyhow::Error::msg);
    // Close the channel so the remote sees EOF and can finish cleanly.
    drop(executor);
    handle.finish(result).await
}

/// Ensure the server has a matching `cp2` binary (unless disabled), then
/// push the glob-expanded `roots` to the remote over the chosen transport.
#[expect(clippy::too_many_arguments)]
async fn push_multi_via_ssh(
    remote: &RemoteTarget,
    base: &Path,
    roots: &[PathBuf],
    options: &ExecutorOptions,
    user_remote_path: Option<&str>,
    binaries_dir: Option<&Path>,
    auto_install: bool,
    quiet: bool,
    client: &mut RemoteClient,
    jump: Option<&JumpHost>,
) -> anyhow::Result<SyncStats> {
    let server_args = server_args(options).join(" ");
    match ensure_and_open(
        remote,
        user_remote_path,
        binaries_dir,
        auto_install,
        quiet,
        &server_args,
        client,
        jump,
    )
    .await?
    {
        Ensured::Merged {
            os,
            arch,
            remote_path,
            session,
        } => {
            let mut options = options.clone();
            options.target_os = TargetOs::from_os_name(&os);
            run_session_with_deploy(
                session,
                &server_args,
                auto_install,
                quiet,
                remote,
                &os,
                &arch,
                &remote_path,
                binaries_dir,
                client,
                jump,
                |send, recv| {
                    let base = base.to_path_buf();
                    let roots = roots.to_vec();
                    let options = options.clone();
                    async move {
                        let mut executor = Executor::new(send, recv);
                        let result = executor
                            .push_multi(&base, &roots, &options)
                            .await
                            .map_err(anyhow::Error::new);
                        drop(executor);
                        result
                    }
                },
            )
            .await
        }
        Ensured::Classic { os, remote_path } => {
            push_multi_over_ssh(remote, base, roots, options, &os, &remote_path, client, jump)
                .await
        }
    }
}

/// Push glob-expanded roots over the chosen transport to an already-resolved
/// remote.
#[expect(clippy::too_many_arguments)]
pub(crate) async fn push_multi_over_ssh(
    remote: &RemoteTarget,
    base: &Path,
    roots: &[PathBuf],
    options: &ExecutorOptions,
    os: &str,
    remote_path: &str,
    client: &mut RemoteClient,
    jump: Option<&JumpHost>,
) -> anyhow::Result<SyncStats> {
    let session = client.open_session(remote, remote_path, os, &server_args(options).join(" "), jump).await?;
    let (send, recv, mut handle) = session.into_parts();
    let mut executor = Executor::new(send, recv);

    // The probed remote OS is the *target* for the permission/link matrices.
    let mut options = options.clone();
    options.target_os = TargetOs::from_os_name(os);
    let result = executor
        .push_multi(base, roots, &options)
        .await
        .map_err(anyhow::Error::msg);
    drop(executor);
    handle.finish(result).await
}

/// Ensure the server has a matching `cp2` binary (unless disabled), then
/// pull a remote tree into `dst_path` over the chosen transport.
#[expect(clippy::too_many_arguments)]
async fn pull_via_ssh(
    remote: &RemoteTarget,
    dst_path: &Path,
    options: &ExecutorOptions,
    user_remote_path: Option<&str>,
    binaries_dir: Option<&Path>,
    auto_install: bool,
    quiet: bool,
    client: &mut RemoteClient,
    jump: Option<&JumpHost>,
) -> anyhow::Result<SyncStats> {
    let server_args = server_args(options).join(" ");
    match ensure_and_open(
        remote,
        user_remote_path,
        binaries_dir,
        auto_install,
        quiet,
        &server_args,
        client,
        jump,
    )
    .await?
    {
        Ensured::Merged {
            os,
            arch,
            remote_path,
            session,
        } => {
            run_session_with_deploy(
                session,
                &server_args,
                auto_install,
                quiet,
                remote,
                &os,
                &arch,
                &remote_path,
                binaries_dir,
                client,
                jump,
                |send, recv| {
                    let dst_path = dst_path.to_path_buf();
                    let options = options.clone();
                    async move {
                        let mut executor = Executor::new(send, recv);
                        let result = executor
                            .pull(&dst_path, &options)
                            .await
                            .map_err(anyhow::Error::new);
                        drop(executor);
                        result
                    }
                },
            )
            .await
        }
        Ensured::Classic { os, remote_path } => {
            pull_over_ssh(remote, dst_path, options, &os, &remote_path, client, jump).await
        }
    }
}

/// Pull over the chosen transport to an already-resolved remote (platform
/// probed, binary ensured). Shared by one-shot pulls and the `--watch` poll
/// loop.
pub(crate) async fn pull_over_ssh(
    remote: &RemoteTarget,
    dst_path: &Path,
    options: &ExecutorOptions,
    os: &str,
    remote_path: &str,
    client: &mut RemoteClient,
    jump: Option<&JumpHost>,
) -> anyhow::Result<SyncStats> {
    let session = client.open_session(remote, remote_path, os, &server_args(options).join(" "), jump).await?;
    let (send, recv, mut handle) = session.into_parts();
    let mut executor = Executor::new(send, recv);

    let result = executor
        .pull(dst_path, options)
        .await
        .map_err(anyhow::Error::msg);
    drop(executor);
    handle.finish(result).await
}

/// Detect the remote platform and the remote binary's version in a single
/// ssh session when the transport supports the combined probe (saving one
/// session per run — a ControlMaster-multiplexed session on WSL2 still
/// costs ~0.37 s), deploy a matching binary when needed, and return the
/// resolved `(os, arch, remote_path)`.
///
/// A failed probe means the remote is unreachable or unauthorized —
/// surfaced now instead of guessing a platform and failing later with a
/// confusing deploy error. Only a successful probe that could not
/// determine the platform falls back to the local one.
pub(crate) async fn resolve_and_ensure(
    remote: &RemoteTarget,
    user_path: Option<&str>,
    binaries_dir: Option<&Path>,
    auto_install: bool,
    quiet: bool,
    client: &mut RemoteClient,
    jump: Option<&JumpHost>,
) -> anyhow::Result<(String, String)> {
    // The combined probe needs a candidate path; the user's explicit path
    // or the Unix default (the platform is unknown until the probe lands).
    let candidate = user_path
        .map_or_else(|| "~/.cargo/bin/cp2".to_string(), str::to_string);
    let (platform, mut version) = match client.probe_and_version(remote, &candidate, jump).await {
        Err(e) => return Err(e.into()),
        Ok(v) => v,
    };
    let (os, arch) = platform.unwrap_or_else(local_platform);
    let remote_path = match user_path {
        Some(p) => p.to_string(),
        None => default_remote_path(&os),
    };
    if !auto_install {
        return Ok((os, remote_path));
    }
    // A Windows remote probed with the Unix-default candidate path would
    // report a false "missing": re-probe with the resolved path first.
    if os == "windows" && version.is_none() && user_path.is_none() {
        version = client.check_version(remote, &remote_path, &os, jump).await?;
    }
    // The build fingerprint must match: it is a hash of every source file
    // (see `protocol::BUILD_FINGERPRINT`), so any code change — wire format,
    // behavior, or performance — makes the remote stale and forces a
    // redeploy. There is no released v1 to lock a protocol version to.
    if let Some((_, Some(fp))) = &version
        && fp == BUILD_FINGERPRINT
    {
        return Ok((os, remote_path));
    }
    // Deploy below: the remote binary is missing or a different build. No
    // deploy source is resolved before this point, so a missing
    // cross-platform sidecar is not an error for an already-matching remote.
    deploy_remote_binary(remote, &os, &arch, &remote_path, binaries_dir, quiet, client, jump).await?;
    Ok((os, remote_path))
}

/// Deploy a matching `cp2` binary to the remote and verify it runs — the
/// remote binary is missing or a different build. Shared by the classic
/// resolution and the merged single-session flow's stale/missing-server
/// retry.
/// Choose the local binary to deploy to a remote platform: a prebuilt
/// sidecar (`--binaries-dir` first, then next to this binary) — a Linux
/// sidecar is a statically linked musl build, so it runs on any remote
/// glibc, while the running binary needs the local glibc (a remote with an
/// older one fails at load time, `GLIBC_2.xx` not found). Same-platform
/// remotes without a sidecar deploy the running binary; a different-platform
/// remote without a sidecar is an error. No automatic download — the user
/// places the binary manually.
fn deploy_source(
    os: &str,
    arch: &str,
    binaries_dir: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let (local_os, local_arch) = local_platform();
    let candidates = sidecar_candidates(os, arch);
    if let Some(path) = find_sidecar(&candidates, binaries_dir) {
        return Ok(path);
    }
    if os == local_os && arch == local_arch {
        return std::env::current_exe().map_err(anyhow::Error::msg);
    }
    let names = candidates
        .iter()
        .map(|t| {
            if t.contains("windows") {
                format!("`cp2-{t}.exe`")
            } else {
                format!("`cp2-{t}`")
            }
        })
        .collect::<Vec<_>>()
        .join(" or ");
    anyhow::bail!(
        "remote is {os}/{arch}: download {names} for cp2 v{} from                 the GitHub releases page and place it next to this binary or in                 --binaries-dir, or pass --no-auto-install",
        env!("CARGO_PKG_VERSION")
    )
}

#[expect(clippy::too_many_arguments, reason = "the resolution needs every resolved remote fact")]
async fn deploy_remote_binary(
    remote: &RemoteTarget,
    os: &str,
    arch: &str,
    remote_path: &str,
    binaries_dir: Option<&Path>,
    quiet: bool,
    client: &mut RemoteClient,
    jump: Option<&JumpHost>,
) -> anyhow::Result<()> {
    let deploy_source = deploy_source(os, arch, binaries_dir)?;
    if !quiet {
        println!("Deploying cp2 v{} to {remote} ({remote_path})...", env!("CARGO_PKG_VERSION"));
    }
    client
        .deploy(remote, remote_path, &deploy_source, os, jump)
        .await?;
    // Verify the pushed binary actually runs and matches. A truncated or
    // otherwise corrupt push would otherwise surface only as a confusing
    // handshake failure on the sync itself.
    match client.check_version(remote, remote_path, os, jump).await {
        Ok(Some((_, Some(fp)))) if fp == BUILD_FINGERPRINT => Ok(()),
        Ok(None) => Err(anyhow::anyhow!(
            "deployed cp2 to {remote} at {remote_path}, but it does not report a version"
        )),
        Ok(Some((v, fp))) => Err(anyhow::anyhow!(
            "deployed cp2 to {remote} at {remote_path}, but it reports              v{v} (build {fp:?}); expected build {BUILD_FINGERPRINT}"
        )),
        Err(e) => Err(anyhow::anyhow!(
            "failed to verify the deployed cp2 on {remote}: {e}"
        )),
    }
}
/// The outcome of opening the sync session: either the merged single-session
/// flow (the platform was read from the in-band preamble, the session is
/// ready for the executor) or the classic two-session resolution (probe,
/// then open the session as usual).
#[expect(clippy::large_enum_variant, reason = "Merged carries the opened Session by design")]
pub(crate) enum Ensured {
    /// One ssh session carrying the platform preamble and the sync; a
    /// stale/missing server binary is recovered by a deploy and one retry.
    Merged {
        os: String,
        arch: String,
        remote_path: String,
        session: Session,
    },
    /// The classic probe+sync flow: open the session with the resolved
    /// platform and path.
    Classic { os: String, remote_path: String },
}

/// Resolve the remote platform/binary and open the sync session. The
/// common Unix case rides the merged single-session flow (rsync-style — one
/// ssh session carries the platform preamble and the sync); the fallbacks
/// (a password in play, `--remote-sudo`, a remote that does not speak the
/// preamble — Windows sshd etc.) keep the classic probe+sync resolution.
#[expect(clippy::too_many_arguments, reason = "the resolution needs every resolved remote fact")]
pub(crate) async fn ensure_and_open(
    remote: &RemoteTarget,
    user_path: Option<&str>,
    binaries_dir: Option<&Path>,
    auto_install: bool,
    quiet: bool,
    server_args: &str,
    client: &mut RemoteClient,
    jump: Option<&JumpHost>,
) -> anyhow::Result<Ensured> {
    // The merged command needs a candidate path; the user's explicit path or
    // the Unix default (the platform is unknown until the preamble lands).
    let candidate = user_path
        .map_or_else(|| "~/.cargo/bin/cp2".to_string(), str::to_string);
    match client
        .open_preamble_session(remote, &candidate, server_args, jump)
        .await
    {
        Ok(Some(p)) => {
            return Ok(Ensured::Merged {
                os: p.os,
                arch: p.arch,
                remote_path: candidate,
                session: p.session,
            })
        }
        Ok(None) => {}
        Err(e) => return Err(e.into()),
    }
    let (os, remote_path) =
        resolve_and_ensure(remote, user_path, binaries_dir, auto_install, quiet, client, jump)
            .await?;
    Ok(Ensured::Classic { os, remote_path })
}

/// Run the sync over an opened session: the executor consumes the halves,
/// the channel is closed, and the transport handle finishes. The executor
/// error keeps its type (downcastable) so the caller can decide a retry.
/// Whether the transfer itself failed (before the transport finish) is
/// returned alongside the finished result — the caller needs the
/// distinction to know whether the protocol completed before the ssh
/// child exited.
async fn run_session_once<F, Fut>(
    session: Session,
    run: &F,
) -> (bool, anyhow::Result<SyncStats>, SessionHandle)
where
    F: Fn(
            Box<dyn AsyncWrite + Unpin + Send>,
            Box<dyn AsyncRead + Unpin + Send>,
        ) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<SyncStats>>,
{
    let (send, recv, mut handle) = session.into_parts();
    let executor_result = run(send, recv).await;
    let transfer_failed = executor_result.is_err();
    let finished = handle.finish(executor_result).await;
    (transfer_failed, finished, handle)
}

/// Run the sync on the merged single-session flow; when the remote binary is
/// missing or stale — the handshake rejected it, or the session died before
/// the transfer (a non-zero exit — the binary never started) — deploy a
/// matching binary and retry once on a fresh session. The Hello fingerprint
/// is the authority the classic probe used to pre-check; here the deploy
/// happens on the failure instead of before the sync.
#[expect(clippy::too_many_arguments)]
pub(crate) async fn run_session_with_deploy<F, Fut>(
    session: Session,
    server_args: &str,
    auto_install: bool,
    quiet: bool,
    remote: &RemoteTarget,
    os: &str,
    arch: &str,
    remote_path: &str,
    binaries_dir: Option<&Path>,
    client: &mut RemoteClient,
    jump: Option<&JumpHost>,
    run: F,
) -> anyhow::Result<SyncStats>
where
    F: Fn(
            Box<dyn AsyncWrite + Unpin + Send>,
            Box<dyn AsyncRead + Unpin + Send>,
        ) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<SyncStats>>,
{
    let (transfer_failed, result, mut handle) = run_session_once(session, &run).await;
    let rejected = result.as_ref().is_err_and(|e| {
        e.downcast_ref::<crate::Error>()
            .is_some_and(|ce| matches!(ce, crate::Error::HandshakeRejected { .. }))
    });
    // The stream error arrived before the child was reaped (finish propagates
    // peer errors without waiting); a dead child with a non-zero exit means
    // the server binary never started — a deploy is the right recovery. An
    // auth failure cannot reach here: the preamble marker never appears, so
    // the merged open falls back before the executor runs. The dead-child
    // case is deploy-worthy only when the transfer itself failed: a server
    // that exits non-zero *after* a completed transfer (its teardown write
    // hit the closed channel) must not trigger a redeploy + re-run — with
    // `--remove-source-files` the source is already gone.
    let child_died = handle.child_exited_nonzero().await;
    if result.is_err() && auto_install && (rejected || (child_died && transfer_failed)) {
        // The merged deploy-and-serve: the deploy session *is* the sync
        // session (the binary is streamed, `exec`'d, and the Hello verifies
        // it) — the classic two-phase deploy's separate verification session
        // is gone, so the stale/missing case drops from four ssh sessions to
        // two. The retry runs once; a second failure (a corrupt deploy) is
        // the final error, exactly as before.
        let source = deploy_source(os, arch, binaries_dir)?;
        if !quiet {
            println!(
                "Deploying cp2 v{} to {remote} ({remote_path})...",
                env!("CARGO_PKG_VERSION")
            );
        }
        let session = client
            .deploy_and_open_session(remote, remote_path, server_args, &source, jump)
            .await?;
        let (_, result, _) = run_session_once(session, &run).await;
        return result;
    }
    result
}

/// Locate a prebuilt sidecar `cp2-<triple>` for the remote platform, in
/// `--binaries-dir` first, then next to this binary. Windows triples also
/// accept the `.exe`-suffixed name — what the release tarball actually ships
/// (`cp2-x86_64-pc-windows-gnu.exe`) — so a Unix client can deploy to a
/// Windows remote straight from the archive.
fn find_sidecar(candidates: &[&str], binaries_dir: Option<&Path>) -> Option<PathBuf> {
    candidates.iter().find_map(|triple| {
        let names: Vec<String> = if triple.contains("windows") {
            vec![triple.to_string(), format!("{triple}.exe")]
        } else {
            vec![triple.to_string()]
        };
        names.into_iter().find_map(|name| {
            let near = sidecar_path(&name);
            if near.is_file() {
                return Some(near);
            }
            binaries_dir
                .is_some_and(|dir| dir.join(format!("cp2-{name}")).is_file())
                .then(|| binaries_dir.unwrap().join(format!("cp2-{name}")))
        })
    })
}

/// Install the default per-file progress reporter (rsync `-aP`-style):
/// every transferred file gets a listing line on stdout, and on a terminal
/// an in-place percentage is shown while the file is in flight.
///
/// The in-place redraw is throttled to ~10/s: on a fast link the per-chunk
/// reports arrive thousands of times per second, and a redraw (write +
/// flush) per report would make the transfer itself terminal-bound. The
/// per-file completion line is never throttled.
fn install_progress(options: &mut ExecutorOptions) {
    use std::io::IsTerminal;
    let interactive = std::io::stdout().is_terminal();
    let state = std::sync::Arc::new(std::sync::Mutex::new(ProgressState::default()));
    options.progress = Some(Arc::new(move |path: &str, done: u64, total: u64, files_total: u64| {
        use std::fmt::Write as _;
        use std::io::Write;
        let mut out = std::io::stdout();
        let mut st = state.lock().unwrap();
        // The speed's byte counter: the per-file done deltas (the reported
        // bytes are monotonic per file).
        let prev = st.last_done.insert(path.to_string(), done).unwrap_or(0);
        st.bytes = st.bytes.saturating_add(done.saturating_sub(prev));
        if done >= total {
            // A file can report completion twice (the wrapper's final
            // in-progress write plus the explicit completion call) — count
            // each path once so `[index/total]` and the remaining count
            // stay honest (the byte counter is already delta-based).
            if st.completed_paths.insert(path.to_string()) {
                st.completed += 1;
            }
            // Completion line — the rsync -v per-file listing, with the
            // file's ordinal and the run's total (`[12/3456]`). Lines are
            // batched: on a 100 K-file transfer the per-line write syscall
            // alone costs seconds, and the batch path reports whole batches
            // at once anyway.
            let completed = st.completed;
            let _ = write!(st.pending, "[{completed}/{files_total}] {path}\n");
            let now = std::time::Instant::now();
            if st.pending.len() >= PROGRESS_BATCH_BYTES
                || now.duration_since(st.last_flush) >= PROGRESS_BATCH_AGE
                || st.completed == files_total
            {
                let _ = write!(out, "{}", st.pending);
                st.pending.clear();
                st.last_flush = now;
            }
        } else if interactive {
            // The redraw overwrites the current line — any pending
            // completion lines must land first so the display stays clean.
            if !st.pending.is_empty() {
                let _ = write!(out, "{}", st.pending);
                st.pending.clear();
                st.last_flush = std::time::Instant::now();
            }
            let now = std::time::Instant::now();
            if st.last_redraw.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(100))
            {
                return;
            }
            st.last_redraw = Some(now);
            // Integer percent; `checked` arithmetic degrades to 100% on
            // overflow or a zero total (byte counts stay far below
            // u64::MAX / 100 anyway).
            let pct = done
                .checked_mul(100)
                .and_then(|d| d.checked_div(total))
                .unwrap_or(100);
            // Display-only speed estimate: a byte count beyond f64's exact
            // range (2^53) can only round, not overflow.
            #[expect(clippy::cast_precision_loss)]
            let speed = st.bytes as f64 / st.start.elapsed().as_secs_f64();
            let remaining = files_total.saturating_sub(st.completed);
            let _ = write!(
                out,
                "\r[{}/{}] {path} {pct}% {} / {}  {}  ({} left)",
                st.completed + 1,
                files_total,
                human_bytes(done),
                human_bytes(total),
                human_speed(speed),
                remaining
            );
            let _ = out.flush();
        }
    }));
}

/// Display state for the progress reporter: the completed-file count (the
/// `[index/total]` numerator), the transferred bytes for the speed, and the
/// redraw throttle.
struct ProgressState {
    start: std::time::Instant,
    completed: u64,
    bytes: u64,
    last_done: std::collections::HashMap<String, u64>,
    /// Paths already counted as completed (a file's completion can be
    /// reported twice — the wrapper's final write plus the explicit call).
    completed_paths: std::collections::HashSet<String>,
    last_redraw: Option<std::time::Instant>,
    /// Pending per-file completion lines. On a large transfer the per-line
    /// write syscalls alone cost seconds per 100 K files, so lines are
    /// batched and flushed on a size or time budget instead.
    pending: String,
    last_flush: std::time::Instant,
}

/// Flush the completion-line buffer when it is large enough or old enough.
const PROGRESS_BATCH_BYTES: usize = 32 * 1024;
const PROGRESS_BATCH_AGE: std::time::Duration = std::time::Duration::from_millis(250);

impl Default for ProgressState {
    fn default() -> Self {
        let now = std::time::Instant::now();
        Self {
            start: now,
            completed: 0,
            bytes: 0,
            last_done: std::collections::HashMap::new(),
            completed_paths: std::collections::HashSet::new(),
            last_redraw: None,
            pending: String::new(),
            last_flush: now,
        }
    }
}

/// Format a transfer rate for progress display ("12.3 MiB/s").
fn human_speed(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes_per_sec;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}/s", UNITS[unit])
}

/// Format a byte count for progress display (1024-based K/M/G/T).
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if n < 1024 {
        return format!("{n}B");
    }
    // Display-only: f64 cannot represent byte counts above 2^53 exactly, but
    // the error is < 1 part in 10^15 — irrelevant for a human-readable size.
    #[expect(clippy::cast_precision_loss)]
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn server_args_flags_and_values_are_separate_tokens() {
        let options = ExecutorOptions {
            jobs: Some(4),
            max_delete: Some(10),
            storage: crate::platform::storage::StoragePreference::Ssd,
            delete: true,
            ..ExecutorOptions::default()
        };
        let args = server_args(&options);
        // Flag and value are separate elements: the local `cp2 --server`
        // child passes them straight into argv, and the ssh path joins them.
        assert!(args.windows(2).any(|w| w == ["--jobs", "4"]), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["--max-delete", "10"]), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["--storage", "ssd"]), "{args:?}");
        // `--delete` is forwarded so the server receiver scans the full
        // destination for the client's planner to name extras.
        assert!(args.iter().any(|a| a == "--delete"), "{args:?}");
        // No element ever embeds a space (a single argv token must stay one).
        assert!(args.iter().all(|a| !a.contains(' ')), "{args:?}");
    }

    #[test]
    fn find_sidecar_accepts_release_names() {
        let dir = tempfile::tempdir().unwrap();
        // The release tarball ships the Windows sidecar with an `.exe`
        // suffix; a Unix client must find it to deploy to a Windows remote.
        let exe = dir.path().join("cp2-x86_64-pc-windows-gnu.exe");
        fs::write(&exe, b"PE").unwrap();
        let found = find_sidecar(&["x86_64-pc-windows-msvc", "x86_64-pc-windows-gnu"], Some(dir.path()))
            .expect("the .exe sidecar must be found");
        assert_eq!(found, exe);

        // Unix triples use the plain name.
        let plain = dir.path().join("cp2-aarch64-unknown-linux-musl");
        fs::write(&plain, b"ELF").unwrap();
        let found = find_sidecar(&["aarch64-unknown-linux-musl"], Some(dir.path())).unwrap();
        assert_eq!(found, plain);

        // Nothing present → None (the caller reports the download hint).
        let empty = tempfile::tempdir().unwrap();
        assert!(find_sidecar(&["x86_64-pc-windows-gnu"], Some(empty.path())).is_none());
    }

    #[test]
    fn static_prefix_dir_cases() {
        assert_eq!(static_prefix_dir("./*"), PathBuf::new());
        assert_eq!(static_prefix_dir("*.rs"), PathBuf::new());
        assert_eq!(static_prefix_dir("src/*"), PathBuf::from("src"));
        assert_eq!(static_prefix_dir("src/**/x.rs"), PathBuf::from("src"));
        assert_eq!(static_prefix_dir("sub/dir/*.rs"), PathBuf::from("sub/dir"));
        assert_eq!(static_prefix_dir("src/foo*"), PathBuf::from("src"));
        assert_eq!(static_prefix_dir("src/foo[ab].rs"), PathBuf::from("src"));
        assert_eq!(static_prefix_dir("/abs/path/x?"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn expand_source_literal_path_wins() {
        let dir = tempfile::tempdir().unwrap();
        // A path that literally exists is never treated as a pattern, even
        // with metacharacters in the name.
        fs::write(dir.path().join("a*b"), b"x").unwrap();
        let literal = format!("{}/a*b", dir.path().display());
        assert!(expand_source(&literal).unwrap().is_none());

        // No metacharacters: not a pattern, even when the path does not exist.
        assert!(expand_source("/nonexistent/plain/path").unwrap().is_none());
    }

    #[test]
    fn expand_source_no_matches_errors() {
        let dir = tempfile::tempdir().unwrap();
        let pattern = format!("{}/no*.rs", dir.path().display());
        let err = expand_source(&pattern).unwrap_err().to_string();
        assert!(err.contains("no files match"), "{err}");
        assert!(err.contains(&pattern), "{err}");
    }

    #[test]
    fn expand_source_returns_base_and_sorted_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.rs"), b"y").unwrap();
        fs::write(dir.path().join("a.rs"), b"x").unwrap();
        fs::write(dir.path().join("c.txt"), b"z").unwrap();

        let pattern = format!("{}/*.rs", dir.path().display());
        let (base, roots) = expand_source(&pattern).unwrap().unwrap();
        assert_eq!(base, dir.path());
        let names: Vec<&str> = roots
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn parse_file_list_is_strictly_newline_delimited() {
        // One path per line; blank lines are skipped.
        let list = "a.txt\n\nsub/b.txt\n";
        assert_eq!(parse_file_list(list), vec!["a.txt", "sub/b.txt"]);
        // Windows (CRLF) line endings work too.
        let crlf = "a.txt\r\nb.txt\r\n";
        assert_eq!(parse_file_list(crlf), vec!["a.txt", "b.txt"]);
        // Paths may contain commas or leading/trailing spaces (trimmed).
        assert_eq!(
            parse_file_list("a, b.txt\n  c.txt  \n"),
            vec!["a, b.txt", "c.txt"]
        );
        // Empty input yields nothing.
        assert!(parse_file_list("\n\n").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn path_root_is_the_filesystem_root_on_unix() {
        assert_eq!(path_root(Path::new("/data/a.txt")), Path::new("/"));
        assert_eq!(path_root(Path::new("/")), Path::new("/"));
        assert_eq!(path_root(Path::new("/data")), Path::new("/"));
    }

    #[cfg(windows)]
    #[test]
    fn path_root_is_the_drive_root_on_windows() {
        // The drive letter is the root component: it is stripped when the
        // entry mirrors under the destination (`D:\data\a.txt` →
        // `DST/data/a.txt`).
        assert_eq!(path_root(Path::new(r"D:\data\a.txt")), Path::new(r"D:\"));
        assert_eq!(path_root(Path::new(r"D:\data")), Path::new(r"D:\"));
        assert_eq!(path_root(Path::new(r"C:\")), Path::new(r"C:\"));
    }

    #[test]
    fn expand_source_matches_dotfiles() {
        // Shell `*` skips dotfiles; cp2's wildcard matches them (rsync
        // semantics, consistent with the scanner syncing dotfiles).
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".hidden"), b"h").unwrap();
        let pattern = format!("{}/*", dir.path().display());
        let (_base, roots) = expand_source(&pattern).unwrap().unwrap();
        assert!(
            roots
                .iter()
                .any(|p| p.file_name().unwrap().to_str() == Some(".hidden"))
        );
    }
}
