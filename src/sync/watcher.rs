//! Filesystem watcher plumbing shared by the `--watch` CLI loop
//! (`commands/watch.rs`) and the server-side pull-watch session
//! (`sync/executor.rs`). Thin notify wrapper: a recursive watcher that
//! forwards sync-worthy events as unit signals.

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Cap on the debounce window: a never-ending burst of events (e.g. a build
/// writing thousands of files) must not starve the sync forever.
pub(crate) const MAX_COALESCE: Duration = Duration::from_secs(10);

/// Backoff after a failed sync before retrying the same changes (watch push
/// loop, initial-sync retry, and server-side scan retry all share it).
pub(crate) const SYNC_ERROR_BACKOFF: Duration = Duration::from_secs(5);

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
