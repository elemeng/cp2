//! Filesystem watcher plumbing shared by the `--watch` CLI loop
//! (`commands/watch.rs`) and the server-side pull-watch session
//! (`sync/executor.rs`). Thin notify wrapper: a recursive watcher that
//! forwards sync-worthy events as unit signals.

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Cap on the debounce window: a never-ending burst of events (e.g. a build
/// writing thousands of files) must not starve the sync forever.
pub(crate) const MAX_COALESCE: Duration = Duration::from_secs(10);

/// Backoff after a failed sync before retrying the same changes (watch push
/// loop, initial-sync retry, and server-side scan retry all share it).
pub(crate) const SYNC_ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// Why a debounced wait on the changes channel ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BurstWait {
    /// A quiet window elapsed, or the coalesce cap was hit: run a sync cycle.
    Ready,
    /// The changes channel closed (the watcher stopped).
    ChannelClosed,
    /// The abort condition fired (Ctrl-C on the client, a disconnect probe on
    /// the server): stop watching.
    Aborted,
}

/// Wait out a debounced change burst on `changes`: the first event opens a
/// quiet window (`delay`), further events restart it, but [`MAX_COALESCE`]
/// caps the total wait so a continuous stream still syncs. `abort` is raced
/// against the wait so callers can stop early (a signal, a disconnect probe);
/// it resolves `Err` to abort the session with an error.
pub(crate) async fn wait_debounce(
    changes: &mut UnboundedReceiver<()>,
    delay: Duration,
    abort: impl core::future::Future<Output = crate::Result<()>>,
) -> crate::Result<BurstWait> {
    let window_start = tokio::time::Instant::now();
    let mut abort = core::pin::pin!(abort);
    loop {
        tokio::select! {
            res = changes.recv() => {
                if res.is_none() {
                    return Ok(BurstWait::ChannelClosed);
                }
                // More changes: restart the quiet window (the cap is absolute).
            }
            () = tokio::time::sleep(delay) => return Ok(BurstWait::Ready),
            () = tokio::time::sleep_until(window_start + MAX_COALESCE) => return Ok(BurstWait::Ready),
            res = &mut abort => {
                res?;
                return Ok(BurstWait::Aborted);
            }
        }
    }
}

/// Start a recursive watcher on `root`, forwarding every sync-worthy event
/// as a unit signal on `tx`.
///
/// The watcher must be kept alive by the caller (dropping it stops
/// delivery).
pub(crate) fn start_watcher(
    root: &Path,
    tx: UnboundedSender<()>,
) -> notify::Result<RecommendedWatcher> {
    let mut watcher = recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res
            && event_is_syncworthy(&event)
        {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// Only content/name changes matter: `Access` events (pure reads) must not
/// trigger syncs, or unrelated readers (editors, backups) would cause a
/// sync per read.
fn event_is_syncworthy(event: &Event) -> bool {
    !event.kind.is_access()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_events_are_ignored() {
        use notify::EventKind;
        let access = Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![],
            attrs: notify::event::EventAttributes::default(),
        };
        let modify = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            paths: vec![],
            attrs: notify::event::EventAttributes::default(),
        };
        let create = Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![],
            attrs: notify::event::EventAttributes::default(),
        };
        assert!(!event_is_syncworthy(&access));
        assert!(event_is_syncworthy(&modify));
        assert!(event_is_syncworthy(&create));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watcher_fires_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let _watcher = start_watcher(dir.path(), tx).unwrap();

        tokio::fs::write(dir.path().join("f.txt"), b"x")
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(
            matches!(got, Ok(Some(()))),
            "write must produce a sync-worthy event, got {got:?}"
        );
    }
}
