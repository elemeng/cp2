//! `-W/--watch`: continuous incremental sync of a local directory to a
//! remote, driven by filesystem events (notify) with a debounce window.
//!
//! Each change burst runs a full incremental push (scan + plan + delta), so
//! only the changed bytes travel — the same machinery as a one-shot sync.
//! Every sync opens a fresh ssh session; platform detection and binary
//! deployment happen once at watch start.

use crate::cli::Cli;
use crate::protocol::TargetOs;
use crate::sync::watcher::{SYNC_ERROR_BACKOFF, start_watcher};
use crate::sync::{Executor, ExecutorOptions, SyncStats};
use crate::target::RemoteTarget;
use crate::transport::{JumpHost, RemoteClient};
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

/// Watch `src_path` and push every change burst to `remote`.
///
/// Runs an initial sync (baseline), then watches `src_path` recursively and
/// syncs after each quiet window. Ctrl-C stops cleanly. Returns after the
/// watcher stops (channel closed or signal).
///
/// # Errors
///
/// Returns an error if the initial sync, platform probe, binary deploy, or
/// watcher setup fails.
pub(crate) async fn watch_push(
    cli: &Cli,
    remote: &RemoteTarget,
    src_path: &Path,
    options: &ExecutorOptions,
    client: RemoteClient,
    jump: Option<JumpHost>,
) -> Result<()> {
    // The run's transport client is shared across every sync cycle (the russh
    // transport authenticates once for the whole watch session); the Mutex is
    // uncontended — operations are sequential.
    let client = Arc::new(tokio::sync::Mutex::new(client));
    let server_args = super::sync::server_args(options).join(" ");
    let ensured = super::sync::ensure_and_open(
        remote,
        cli.remote_path.as_deref(),
        cli.binaries_dir.as_deref(),
        !cli.no_auto_install,
        cli.quiet,
        &server_args,
        &mut *client.lock().await,
        jump.as_ref(),
    )
    .await?;
    let (os, remote_path) = match &ensured {
        super::sync::Ensured::Merged {
            os,
            remote_path,
            ..
        }
        | super::sync::Ensured::Classic {
            os,
            remote_path,
            ..
        } => (os.clone(), remote_path.clone()),
    };
    let arch = match &ensured {
        super::sync::Ensured::Merged { arch, .. } => Some(arch.clone()),
        super::sync::Ensured::Classic { .. } => None,
    };
    // The merged flow's session feeds the first sync attempt; the backoff
    // retries (and every later cycle) open their own session.
    let first_session = match ensured {
        super::sync::Ensured::Merged { session, .. } => Some(session),
        super::sync::Ensured::Classic { .. } => None,
    };
    let first_session = Arc::new(tokio::sync::Mutex::new(first_session));
    let auto_install = !cli.no_auto_install;
    let binaries_dir = cli.binaries_dir.clone();
    let arch = Arc::new(arch);

    // Watch mode suppresses per-file progress lines: every sync would spam
    // the terminal; a summary per sync is enough.
    let mut options = options.clone();
    options.progress = None;

    // Everything from the watcher on is bounded by -W=DUR (if set):
    // the initial sync counts toward the cap, and a timeout cancels the
    // session and exits cleanly.
    let delay = Duration::from_millis(cli.watch_delay);
    // Session config is shared (never mutated after this point); each sync
    // cycle bumps the Arc instead of deep-cloning the whole set.
    let src_path = Arc::new(src_path.to_path_buf());
    let remote = Arc::new(remote.clone());
    let remote_path = Arc::new(remote_path.clone());
    let os = Arc::new(os.clone());
    let options = Arc::new(options);
    run_bounded(
        Box::pin(async move {
            // Start the watcher *before* the initial sync so any change that
            // lands while the initial manifest is being built or transferred
            // is captured: the event is buffered, and watch_loop's first
            // iteration re-syncs.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let _watcher = start_watcher(&src_path, tx)?;

            if !cli.quiet {
                println!("Initial sync...");
            }
            let client = Arc::clone(&client);
            let initial = initial_sync(|| {
                let client = Arc::clone(&client);
                let remote = Arc::clone(&remote);
                let src_path = Arc::clone(&src_path);
                let options = Arc::clone(&options);
                let os = Arc::clone(&os);
                let remote_path = Arc::clone(&remote_path);
                let arch = Arc::clone(&arch);
                let first_session = Arc::clone(&first_session);
                let server_args = server_args.clone();
                let binaries_dir = binaries_dir.clone();
                let quiet = cli.quiet;
                let jump = jump.clone();
                async move {
                    let mut options = (*options).clone();
                    options.target_os = TargetOs::from_os_name(&os);
                    // The merged flow's session feeds the first attempt;
                    // retries and later cycles open their own.
                    if let Some(session) = first_session.lock().await.take() {
                        let Some(arch) = arch.as_ref() else {
                            return Err(anyhow::anyhow!(
                                "merged session without a resolved architecture"
                            ));
                        };
                        return super::sync::run_session_with_deploy(
                            session,
                            &server_args,
                            auto_install,
                            quiet,
                            &remote,
                            &os,
                            arch,
                            &remote_path,
                            binaries_dir.as_deref(),
                            &mut *client.lock().await,
                            jump.as_ref(),
                            |send, recv| {
                                let src_path = (*src_path).clone();
                                let options = options.clone();
                                async move {
                                    let mut executor = Executor::new(send, recv);
                                    let result = executor
                                        .push(&src_path, &options)
                                        .await
                                        .map_err(anyhow::Error::new);
                                    drop(executor);
                                    result
                                }
                            },
                        )
                        .await;
                    }
                    super::sync::push_over_ssh(
                        &remote,
                        &src_path,
                        &options,
                        &os,
                        &remote_path,
                        &mut *client.lock().await,
                        jump.as_ref(),
                    )
                    .await
                }
            })
            .await?;
            // Ctrl-C during the initial-sync retry backoff ends the whole
            // session: no "Watching…" line, no watch loop.
            if matches!(initial, InitialSync::Interrupted) {
                return Ok(());
            }
            if !cli.quiet {
                println!(
                    "Watching {} → {remote} (debounce {}ms). Ctrl-C to stop.",
                    src_path.display(),
                    cli.watch_delay
                );
            }
            watch_loop(&mut rx, delay, move || {
                let os = Arc::clone(&os);
                let remote = Arc::clone(&remote);
                let remote_path = Arc::clone(&remote_path);
                let src_path = Arc::clone(&src_path);
                let options = Arc::clone(&options);
                let client = Arc::clone(&client);
                let jump = jump.clone();
                async move {
                    let start = Instant::now();
                    let stats = super::sync::push_over_ssh(
                        &remote,
                        &src_path,
                        &options,
                        &os,
                        &remote_path,
                        &mut *client.lock().await,
                        jump.as_ref(),
                    )
                    .await?;
                    tracing::info!(
                        "watch sync: {} files, {} bytes in {:?}",
                        stats.files_sent,
                        stats.bytes_transferred,
                        start.elapsed()
                    );
                    Ok(stats)
                }
            })
            .await
        }),
        cli.watch,
        cli.quiet,
    )
    .await
}

/// The outcome of the initial sync.
enum InitialSync {
    /// The sync completed (possibly after retries).
    Completed,
    /// Ctrl-C ended the retry backoff: the whole watch session must stop —
    /// no "Watching…" line, no watch loop.
    Interrupted,
}

/// Run the initial sync, retrying failures with backoff until it succeeds or
/// Ctrl-C ends the session. Mirrors the watch loop's retry behavior, so a
/// transient failure at startup (a flaky network, a locked tree) does not
/// kill the watcher the way a bare `?` would. Ctrl-C during a backoff is a
/// distinct outcome: the caller stops the session instead of starting the
/// watch loop.
async fn initial_sync<F, Fut>(mut sync: F) -> Result<InitialSync>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<SyncStats>>,
{
    loop {
        match sync().await {
            Ok(_) => return Ok(InitialSync::Completed),
            Err(e) => {
                tracing::warn!(
                    "initial sync failed: {e}; retrying in {:?}",
                    SYNC_ERROR_BACKOFF
                );
                tokio::select! {
                    () = tokio::time::sleep(SYNC_ERROR_BACKOFF) => {}
                    _ = tokio::signal::ctrl_c() => return Ok(InitialSync::Interrupted),
                }
            }
        }
    }
}

/// Run the watch session `run`, bounding it to `max` when set
/// (`-W=DUR`). A timeout cancels the session after the cap and
/// exits cleanly; the in-flight ssh child is dropped, its stdin closes, and
/// the remote server exits on EOF.
async fn run_bounded<F>(run: F, max: Option<Duration>, quiet: bool) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    match max {
        Some(cap) => match tokio::time::timeout(cap, run).await {
            Ok(result) => result,
            Err(_elapsed) => {
                if !quiet {
                    println!("Watch duration reached; stopping.");
                }
                Ok(())
            }
        },
        None => run.await,
    }
}

/// Run `sync` whenever `changes` fires, coalescing bursts into single runs.
///
/// - The first event opens a debounce window: further events restart it, but
///   [`MAX_COALESCE`] caps the wait so a continuous stream still syncs.
/// - Changes that arrive *while a sync runs* mark the tree dirty; the next
///   sync starts immediately after the current one (no debounce).
/// - A failed sync is logged and the same changes are retried after
///   [`SYNC_ERROR_BACKOFF`].
/// - Ctrl-C ends the loop cleanly; closing `changes` also ends it (used by
///   tests and by the parent dropping the sender).
async fn watch_loop<F, Fut>(
    changes: &mut UnboundedReceiver<()>,
    delay: Duration,
    mut sync: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<SyncStats>>,
{
    loop {
        // Wait for the first change after the previous sync (or start).
        // Ctrl-C must work here too: the idle wait has no other way to end,
        // and an unhandled signal would hang the session between bursts.
        tokio::select! {
            res = changes.recv() => {
                if res.is_none() {
                    return Ok(()); // channel closed: stop watching
                }
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }

        // Debounce: wait for a quiet window (or the coalesce cap), stopping
        // cleanly on Ctrl-C. A closed channel falls through to sync what we
        // have, then the post-sync drain ends the loop.
        match crate::sync::watcher::wait_debounce(changes, delay, async {
            tokio::signal::ctrl_c().await.unwrap_or(());
            Ok(())
        })
        .await?
        {
            crate::sync::watcher::BurstWait::Aborted => return Ok(()),
            crate::sync::watcher::BurstWait::Ready
            | crate::sync::watcher::BurstWait::ChannelClosed => {}
        }

        // Sync, re-running immediately if changes arrived during the run;
        // failed syncs retry the same changes after a backoff.
        loop {
            match sync().await {
                Ok(_) => {
                    // Drain every pending signal: one resync covers the whole
                    // burst. Resyncing once per queued event would cascade on
                    // a busy tree (each resync itself generates more events).
                    let mut dirty = false;
                    loop {
                        match changes.try_recv() {
                            Ok(()) => dirty = true,
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                return Ok(())
                            }
                        }
                    }
                    if !dirty {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "watch sync failed: {e}; retrying in {:?}",
                        SYNC_ERROR_BACKOFF
                    );
                    tokio::select! {
                        () = tokio::time::sleep(SYNC_ERROR_BACKOFF) => {}
                        _ = tokio::signal::ctrl_c() => return Ok(()),
                    }
                }
            }
        }
    }
}

/// Watch a local destination driven by the **server**: the remote watches
/// its own source tree and runs incremental pull cycles over one persistent
/// session (the client re-scans its local destination each cycle — cheap
/// metadata, no network polling). Ctrl-C ends the session cleanly.
pub(crate) async fn watch_pull(
    cli: &Cli,
    remote: &RemoteTarget,
    dst_path: &Path,
    options: &ExecutorOptions,
    mut client: RemoteClient,
    jump: Option<JumpHost>,
) -> Result<()> {
    let server_args = super::sync::server_args(options).join(" ");
    let ensured = super::sync::ensure_and_open(
        remote,
        cli.remote_path.as_deref(),
        cli.binaries_dir.as_deref(),
        !cli.no_auto_install,
        cli.quiet,
        &server_args,
        &mut client,
        jump.as_ref(),
    )
    .await?;
    let mut options = options.clone();
    options.progress = None;

    if !cli.quiet {
        println!(
            "Watching {remote} → {} (server-driven, debounce {}ms). Ctrl-C to stop.",
            dst_path.display(),
            cli.watch_delay
        );
    }

    let delay = Duration::from_millis(cli.watch_delay);
    let remote = remote.clone();
    let dst_path = dst_path.to_path_buf();
    let auto_install = !cli.no_auto_install;
    let binaries_dir = cli.binaries_dir.clone();
    match ensured {
        // The merged single-session flow: the persistent watch session is
        // the preamble session; a stale/missing server binary deploys and
        // re-opens once.
        super::sync::Ensured::Merged {
            os,
            arch,
            remote_path,
            session,
        } => run_bounded(
            Box::pin(async move {
                super::sync::run_session_with_deploy(
                    session,
                    &server_args,
                    auto_install,
                    cli.quiet,
                    &remote,
                    &os,
                    &arch,
                    &remote_path,
                    binaries_dir.as_deref(),
                    &mut client,
                    jump.as_ref(),
                    |send, recv| {
                        let dst_path = dst_path.clone();
                        let options = options.clone();
                        async move {
                            let mut executor = Executor::new(send, recv);
                            // The first cycle runs immediately on connect; the
                            // session then stays open until Ctrl-C, -W=DUR,
                            // or a drop.
                            let result = executor
                                .pull_watch(&dst_path, &options, delay)
                                .await
                                .map_err(anyhow::Error::new);
                            drop(executor);
                            result
                        }
                    },
                )
                .await
                .map(|_| ())
            }),
            cli.watch,
            cli.quiet,
        )
        .await,
        super::sync::Ensured::Classic { os, remote_path } => {
            let remote_path = remote_path.clone();
            let os = os.clone();
            run_bounded(
                Box::pin(async move {
                    let session = client
                        .open_session(&remote, &remote_path, &os, &server_args, jump.as_ref())
                        .await?;
                    let (send, recv, mut handle) = session.into_parts();
                    let mut executor = Executor::new(send, recv);

                    // The first cycle runs immediately on connect; the
                    // session then stays open until Ctrl-C, -W=DUR, or a
                    // drop.
                    let result = executor
                        .pull_watch(&dst_path, &options, delay)
                        .await
                        .map_err(anyhow::Error::new);
                    drop(executor);
                    handle.finish(result.map(|_| ())).await
                }),
                cli.watch,
                cli.quiet,
            )
            .await
        }
    }
}

/// Watch a local source and incrementally copy changes to a local
/// destination, event-driven like the push case but over a spawned
/// `cp2 --server` child (pipes) instead of ssh — the full delta engine runs,
/// so only changed bytes travel.
pub(crate) async fn watch_local(
    cli: &Cli,
    src_path: &Path,
    dst_path: &Path,
    options: &ExecutorOptions,
) -> Result<()> {
    tokio::fs::create_dir_all(dst_path)
        .await
        .map_err(anyhow::Error::new)?;
    let mut options = options.clone();
    options.progress = None;

    // The watcher + initial sync + loop are bounded by -W=DUR.
    let delay = Duration::from_millis(cli.watch_delay);
    // Shared session config; each sync cycle bumps the Arc instead of
    // deep-cloning the paths and options.
    let src_path = Arc::new(src_path.to_path_buf());
    let dst_path = Arc::new(dst_path.to_path_buf());
    let options = Arc::new(options);
    run_bounded(
        async move {
            // Start the watcher before the initial sync (see watch_push):
            // changes during it are captured and re-synced on the first loop
            // iteration.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let _watcher = start_watcher(&src_path, tx)?;

            if !cli.quiet {
                println!("Initial sync...");
            }
            let initial = initial_sync(|| push_local_over_server(&src_path, &dst_path, &options)).await?;
            if matches!(initial, InitialSync::Interrupted) {
                return Ok(());
            }
            if !cli.quiet {
                println!(
                    "Watching {} → {} (debounce {}ms). Ctrl-C to stop.",
                    src_path.display(),
                    dst_path.display(),
                    cli.watch_delay
                );
            }
            watch_loop(&mut rx, delay, move || {
                let src_path = Arc::clone(&src_path);
                let dst_path = Arc::clone(&dst_path);
                let options = Arc::clone(&options);
                async move { push_local_over_server(&src_path, &dst_path, &options).await }
            })
            .await
        },
        cli.watch,
        cli.quiet,
    )
    .await
}

/// Run an incremental push over a spawned `cp2 --server` child rooted at
/// `dst` (the same transport the e2e suite uses; no ssh involved).
///
/// Also backs the one-shot local copy (`cp2 SRC DST`): the full protocol
/// pipeline runs, so symlinks, hard links, metadata, and `--delete` behave
/// exactly as they do over ssh. Receiver-side flags (`--jobs`, `--backup`,
/// `--max-delete`, `--fsync`, `--checksum`, `--storage`) reach the child
/// through `server_args`, matching what `spawn_ssh` forwards remotely.
pub(crate) async fn push_local_over_server(
    src: &Path,
    dst: &Path,
    options: &ExecutorOptions,
) -> Result<SyncStats> {
    let (mut child, send, recv) = spawn_server_child(dst, options)?;
    let mut executor = Executor::new(send, recv);
    let result = executor.push(src, options).await.map_err(anyhow::Error::new);
    drop(executor);
    finish_server_child(&mut child, result).await
}

/// Like [`push_local_over_server`], for a glob-expanded source: every root in
/// `roots` syncs as a top-level entry under `base` in one run.
pub(crate) async fn push_multi_local_over_server(
    base: &Path,
    roots: &[std::path::PathBuf],
    dst: &Path,
    options: &ExecutorOptions,
) -> Result<SyncStats> {
    let (mut child, send, recv) = spawn_server_child(dst, options)?;
    let mut executor = Executor::new(send, recv);
    let result = executor
        .push_multi(base, roots, options)
        .await
        .map_err(anyhow::Error::new);
    drop(executor);
    finish_server_child(&mut child, result).await
}

/// A spawned `cp2 --server` child plus the boxed stdio halves the client
/// executor talks over.
type ServerChild = (
    tokio::process::Child,
    Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    Box<dyn tokio::io::AsyncRead + Unpin + Send>,
);

/// Spawn `cp2 --server` rooted at `dst` (created if missing) and return the
/// child plus the boxed stream halves the client executor talks over.
fn spawn_server_child(dst: &Path, options: &ExecutorOptions) -> Result<ServerChild> {
    // The server child is rooted at `dst`, so it must exist before spawn.
    let dst = dst.to_path_buf();
    std::fs::create_dir_all(&dst).map_err(anyhow::Error::new)?;
    let bin = std::env::current_exe().map_err(anyhow::Error::new)?;
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("--server");
    // `server_args` returns each forwarded flag as its own token; they are
    // passed as separate argv elements, so no value is ever re-split.
    cmd.args(super::sync::server_args(options));
    let mut child = cmd
        .current_dir(&dst)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(anyhow::Error::new)?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("server child stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("server child stdout unavailable"))?;
    // The sync data flows over these pipes; a 64 KiB default capacity would
    // add a wakeup round trip every 64 KiB (see `platform::fs::enlarge_pipe`).
    crate::platform::fs::enlarge_pipe(&stdin);
    crate::platform::fs::enlarge_pipe(&stdout);
    Ok((child, Box::new(stdin), Box::new(stdout)))
}

/// Wait for the server child and surface its exit status, then the transfer
/// result.
async fn finish_server_child<T>(
    child: &mut tokio::process::Child,
    result: anyhow::Result<T>,
) -> anyhow::Result<T> {
    let status = child.wait().await.map_err(anyhow::Error::new)?;
    if !status.success() {
        anyhow::bail!("cp2 --server child exited with {status}");
    }
    result
}

/// Serial number for watch-loop test syncs.
#[cfg(test)]
fn test_stats() -> SyncStats {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    SyncStats {
        files_sent: 0,
        files_received: 0,
        bytes_transferred: N.fetch_add(1, Ordering::SeqCst),
        duration: Duration::ZERO,
        skipped: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn test_delay() -> Duration {
        Duration::from_millis(50)
    }

    #[tokio::test]
    async fn burst_coalesces_to_one_sync() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let count = Arc::new(AtomicUsize::new(0));
        let sync_count = count.clone();
        let task = tokio::spawn(async move {
            watch_loop(&mut rx, test_delay(), move || {
                let sync_count = sync_count.clone();
                async move {
                    sync_count.fetch_add(1, AtomicOrdering::SeqCst);
                    Ok(test_stats())
                }
            })
            .await
        });

        // A burst of changes within the debounce window → one sync.
        for _ in 0..5 {
            tx.send(()).unwrap();
        }
        drop(tx); // closing the channel ends the loop after the sync
        task.await.unwrap().unwrap();
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn changes_during_sync_trigger_immediate_resync() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let count = Arc::new(AtomicUsize::new(0));
        let sync_count = count.clone();
        let burst_tx = tx.clone();
        let task = tokio::spawn(async move {
            watch_loop(&mut rx, test_delay(), move || {
                let sync_count = sync_count.clone();
                let burst_tx = burst_tx.clone();
                async move {
                    let n = sync_count.fetch_add(1, AtomicOrdering::SeqCst);
                    // A change arrives during the first sync: the loop must
                    // resync immediately (no debounce) and reach 2 syncs.
                    if n == 0 {
                        let _ = burst_tx.send(());
                    }
                    Ok(test_stats())
                }
            })
            .await
        });

        tx.send(()).unwrap();
        // The loop cannot exit cleanly (the closure keeps a sender alive), so
        // cancel it once the resync has happened.
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert_eq!(count.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn burst_during_sync_coalesces_to_one_resync() {
        // Regression: a burst of events queued during a sync must drain into a
        // single resync — one full sync per queued event would cascade.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let count = Arc::new(AtomicUsize::new(0));
        let sync_count = count.clone();
        let burst_tx = tx.clone();
        let task = tokio::spawn(async move {
            watch_loop(&mut rx, test_delay(), move || {
                let sync_count = sync_count.clone();
                let burst_tx = burst_tx.clone();
                async move {
                    let n = sync_count.fetch_add(1, AtomicOrdering::SeqCst);
                    if n == 0 {
                        // Five events land during the first sync.
                        for _ in 0..5 {
                            let _ = burst_tx.send(());
                        }
                    }
                    Ok(test_stats())
                }
            })
            .await
        });

        tx.send(()).unwrap();
        // The loop cannot exit cleanly (the closure keeps a sender alive), so
        // cancel it once the resync has happened.
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert_eq!(
            count.load(AtomicOrdering::SeqCst),
            2,
            "a 5-event burst must drain into one resync"
        );
    }

    #[tokio::test]
    async fn failed_sync_retries_same_changes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let count = Arc::new(AtomicUsize::new(0));
        let sync_count = count.clone();
        let task = tokio::spawn(async move {
            watch_loop(&mut rx, test_delay(), move || {
                let sync_count = sync_count.clone();
                async move {
                    let n = sync_count.fetch_add(1, AtomicOrdering::SeqCst);
                    if n == 0 {
                        return Err(anyhow::anyhow!("transient failure"));
                    }
                    Ok(test_stats())
                }
            })
            .await
        });

        tx.send(()).unwrap();
        drop(tx);
        task.await.unwrap().unwrap();
        // First attempt fails, retried after backoff → succeeds on the second.
        assert_eq!(count.load(AtomicOrdering::SeqCst), 2);
    }
}
