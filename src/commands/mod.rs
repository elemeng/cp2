pub mod server;
pub mod sync;
pub mod watch;

use crate::cli::Cli;
use crate::protocol::TargetOs;
use crate::sync::ExecutorOptions;
use crate::sync::SyncStats;
use anyhow::Result;
use std::path::Path;

/// Print any skipped files after a sync summary line, so files that could
/// not be applied (locked by another process, path too long, reserved
/// name, ...) are visible at the end of the run instead of only in logs.
///
/// Returns `true` when at least one file was skipped, so the caller can
/// signal a partial transfer (rsync exit code 23).
pub(crate) fn print_skipped(stats: &SyncStats) -> bool {
    if stats.skipped.is_empty() {
        return false;
    }
    println!("Skipped {} file(s):", stats.skipped.len());
    for file in &stats.skipped {
        println!("  {}: {}", file.path, file.reason);
    }
    true
}

/// rsync's "partial transfer due to error" exit code.
const EXIT_PARTIAL: i32 = 23;

/// Exit with rsync's partial-transfer code when files were skipped.
pub(crate) fn exit_if_partial(any_skipped: bool) {
    if any_skipped {
        std::process::exit(EXIT_PARTIAL);
    }
}

/// Warn about link flags that a higher-precedence flag silently overrides
/// (the scanner resolves precedence, not the CLI — a combined invocation
/// must not surprise the user):
///
/// `--skip-links` > `--follow-links` > `--literal-links` (-a implied) >
/// the `--literal-*-links` granular switches > the default smart policy.
fn warn_on_link_flag_conflicts(cli: &Cli) {
    let literal_family = cli.literal_links || cli.archive;
    let granular = cli.literal_internal_links || cli.literal_external_file_links
        || cli.literal_external_dir_links;
    let any_link_flag = cli.follow_links || literal_family || granular;
    if cli.skip_links && any_link_flag {
        tracing::warn!(
            "--skip-links overrides the other link flags: no symlink or shortcut is synced"
        );
    }
    if cli.follow_links && (literal_family || granular) {
        tracing::warn!(
            "--follow-links overrides the literal link flags: every symlink is dereferenced"
        );
    }
}

/// Warn about decision flags that silently cancel, no-op, or override each
/// other (rsync-style intent warnings, not hard errors — the planner still
/// applies its documented precedence). Links are handled separately in
/// [`warn_on_link_flag_conflicts`].
fn warn_on_decision_flag_conflicts(cli: &Cli) {
    if cli.max_delete.is_some() && !cli.delete {
        tracing::warn!("--max-delete has no effect without --delete");
    }
    if cli.existing && cli.ignore_existing {
        tracing::warn!(
            "--existing and --ignore-existing contradict each other: \
             nothing would be updated or created"
        );
    }
    if cli.ignore_times && cli.checksum {
        tracing::warn!(
            "--ignore-times overrides --checksum: every file is transferred regardless of matches"
        );
    }
}

/// Build executor options from the CLI flags.
///
/// # Errors
///
/// Returns an error when a `--exclude-from` / `--include-from` list file
/// cannot be read.
pub(crate) fn options_from_cli(cli: &Cli) -> anyhow::Result<ExecutorOptions> {
    warn_on_link_flag_conflicts(cli);
    warn_on_decision_flag_conflicts(cli);
    let mut exclude = cli.exclude.clone();
    let mut include = cli.include.clone();
    if let Some(f) = &cli.exclude_from {
        exclude.extend(read_pattern_file(f, "--exclude-from")?);
    }
    if let Some(f) = &cli.include_from {
        include.extend(read_pattern_file(f, "--include-from")?);
    }
    Ok(ExecutorOptions {
        checksum: cli.checksum,
        delete: cli.delete,
        update_only: cli.update,
        ignore_existing: cli.ignore_existing,
        existing: cli.existing,
        ignore_times: cli.ignore_times,
        max_delete: cli.max_delete,
        backup: cli.backup,
        jobs: cli.jobs,
        storage: cli.storage,
        compress: cli.compress,
        bwlimit: cli.bwlimit.flatten(),
        exclude,
        include,
        itemize: cli.itemize_changes,
        fsync: cli.fsync,
        // Empty = the serve root; the CLI overrides this from `user@host:path`
        // when either side is remote.
        remote_path: String::new(),
        remote_paths: Vec::new(),
        partial: true,
        remove_source_files: cli.remove_source_files,
        verify: cli.verify,
        rollsum: cli.rollsum,
        quiet: cli.quiet,
        archive: cli.archive,
        // Literal link preservation: the explicit `--literal-links` flag, or
        // implied by `-a` (byte-identical link mode).
        literal_links: cli.literal_links || cli.archive,
        literal_internal_links: cli.literal_internal_links,
        literal_external_file_links: cli.literal_external_file_links,
        literal_external_dir_links: cli.literal_external_dir_links,
        recursive: !cli.no_recursive,
        preserve_links: !cli.skip_links,
        follow_links: cli.follow_links,
        sparse: cli.sparse,
        xattrs: cli.xattrs,
        atimes: cli.atimes,
        preserve_perms: !cli.no_perms,
        preserve_times: !cli.no_times,
        // Overridden per direction before any scan runs: the probed remote
        // OS on push, the local OS on pull/local copy (spec §2.2 / §3.2).
        target_os: TargetOs::Unix,
        progress: None,
        // Overridden per direction by `sync::execute` (trailing-slash
        // semantics for the local source) or by the pull frame (server side).
        include_root_component: false,
    })
}

/// Read a rsync-style pattern list file (`--exclude-from` / `--include-from`):
/// one pattern per line, with blank lines and `#`/`;` comments ignored and
/// leading whitespace stripped.
///
/// # Errors
///
/// Returns an error if `path` cannot be read.
fn read_pattern_file(path: &Path, flag: &str) -> anyhow::Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read {flag} list {}: {e}", path.display()))?;
    let patterns = content
        .lines()
        .map(str::trim_start)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with(';'))
        .map(str::to_string)
        .collect();
    Ok(patterns)
}

/// Dispatch to server mode or the sync CLI based on the parsed arguments.
///
/// # Errors
///
/// Returns an error if the sync, the server session, or the ssh child fails.
pub async fn dispatch(mut cli: Cli) -> Result<()> {
    if cli.server {
        let options = options_from_cli(&cli)?;
        server::execute(&options).await
    } else {
        Box::pin(sync::execute(&mut cli)).await
    }
}

#[cfg(test)]
mod tests {
    use super::read_pattern_file;
    use std::path::PathBuf;

    #[test]
    fn pattern_file_parses_rsync_style() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("exclude.txt");
        std::fs::write(
            &f,
            "# build artifacts\n*.tmp\n  node_modules\n\n; keep\n/target\nsrc/generated\n",
        )
        .unwrap();
        let patterns = read_pattern_file(&f, "--exclude-from").unwrap();
        assert_eq!(
            patterns,
            vec!["*.tmp", "node_modules", "/target", "src/generated"]
        );
    }

    #[test]
    fn pattern_file_missing_is_an_error() {
        assert!(
            read_pattern_file(&PathBuf::from("/nonexistent/exclude.txt"), "--exclude-from").is_err()
        );
    }
}
