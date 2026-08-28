//! Pure sync planner: decides what to do for each file.
//!
//! Adapted from the sy strategy.rs: decision logic is pure — it takes
//! manifests in, returns a [`SyncPlan`] out, and performs no I/O.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use crate::protocol::{FileKind, LinkKind};
use crate::sync::scanner::{FileEntry, Manifest};
use crate::sync::stats::{ItemizeAction, ItemizeEntry};
use crate::sync::wire::wire_rel;

/// What to do with a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    /// File unchanged — do nothing.
    Skip,
    /// New file or directory.
    Create,
    /// File exists but differs.
    Update,
    /// File exists in destination but not source.
    Delete,
    /// Content is in sync but the metadata (mode, mtime) drifted — re-apply
    /// the source attributes without transferring anything.
    MetaOnly,
}

/// A single planned operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTask {
    /// Path relative to the sync root.
    pub relative_path: PathBuf,
    /// The action to perform.
    pub action: SyncAction,
    /// Size of the source file (0 for deletes and directories).
    pub source_size: u64,
    /// Whether this task concerns a directory (create/delete an empty dir).
    pub is_dir: bool,
    /// Link target; `Some` makes this task a contentless link (transferred
    /// via `CreateLinks`) — the rewritten, DEST-relative target decided at
    /// scan time (spec §3.2).
    pub link_target: Option<String>,
    /// How a link task is materialized (symlink vs `.lnk`).
    pub link_kind: LinkKind,
    /// Hard-link target (root-relative path); `Some` makes this task a hard
    /// link to an already-transferred representative of the same inode group.
    pub link_to: Option<String>,
    /// Special file (fifo/socket/device) kind + device number; `Some` makes
    /// this task a contentless special recreated with `--archive` (Unix only).
    pub special: Option<(FileKind, Option<u64>)>,
    /// On-disk source of the content when it lives outside the sync root
    /// (`--follow-links` recursion); `None` resolves under the scan root.
    pub source_path: Option<PathBuf>,
    /// Content origin outside the sync root: never deleted by
    /// `--remove-source-files`.
    pub dereferenced: bool,
}

/// The result of planning a sync.
#[derive(Debug, Clone, Default)]
pub struct SyncPlan {
    /// Files/dirs to create.
    pub creates: Vec<SyncTask>,
    /// Files to update.
    pub updates: Vec<SyncTask>,
    /// Files to delete.
    pub deletes: Vec<SyncTask>,
    /// Files that are already in sync.
    pub skips: Vec<SyncTask>,
    /// Content in sync but metadata drifted (mode/mtime): the sender re-applies
    /// the source attributes without transferring content.
    pub meta: Vec<SyncTask>,
}

impl SyncPlan {
    /// Every task that transfers bytes (creates + updates, in transfer
    /// order) — the shared request/send/apply passes; deletes are handled
    /// separately by the receiver.
    pub fn tasks(&self) -> impl Iterator<Item = &SyncTask> {
        self.creates.iter().chain(self.updates.iter())
    }

    /// Per-file change entries for `--itemize-changes` (`-i`): the creates,
    /// updates, metadata-only re-applies, and in-sync skips. Deletes are
    /// appended by the caller once its `--delete` policy filter has decided
    /// which paths actually go (the plan's deletes include ones the sender
    /// drops as policy-skipped).
    #[must_use]
    pub fn itemize(&self) -> Vec<ItemizeEntry> {
        let mut v = Vec::with_capacity(
            self.creates.len() + self.updates.len() + self.meta.len() + self.skips.len(),
        );
        v.extend(self.creates.iter().map(|t| itemize_entry(ItemizeAction::Create, t)));
        v.extend(self.updates.iter().map(|t| itemize_entry(ItemizeAction::Update, t)));
        // A metadata-only re-apply is an update in the itemize sense.
        v.extend(self.meta.iter().map(|t| itemize_entry(ItemizeAction::Update, t)));
        v.extend(self.skips.iter().map(|t| itemize_entry(ItemizeAction::Skip, t)));
        v
    }
}

/// Itemize entry for one planned task: its rsync file-type letter plus the
/// relative wire path.
fn itemize_entry(action: ItemizeAction, task: &SyncTask) -> ItemizeEntry {
    ItemizeEntry::new(action, wire_rel(&task.relative_path), task_kind(task))
}

/// rsync-file-type letter for an itemize line: `d` directory, `L` symlink,
/// `S` special file, `f` ordinary file.
fn task_kind(task: &SyncTask) -> char {
    if task.is_dir {
        'd'
    } else if task.link_target.is_some() {
        'L'
    } else if task.special.is_some() {
        'S'
    } else {
        'f'
    }
}

/// Planner options (mirrors rsync decision flags).
#[derive(Debug, Clone, Default)]
// Each boolean is an independent rsync decision flag; the planner consults
// them as a flat set (see `compare`).
#[expect(clippy::struct_excessive_bools)]
pub struct PlannerConfig {
    /// Compare BLAKE3 hashes instead of size+mtime.
    pub checksum: bool,
    /// Compare only sizes, skip mtime.
    pub size_only: bool,
    /// Skip files where destination is newer (rsync `--update`).
    pub update_only: bool,
    /// Skip files that already exist on the receiver (rsync `--ignore-existing`).
    /// A transfer rule, not a delete rule: any existing *file or symlink* at
    /// the name is left untouched — including a type change (file ↔ link) or
    /// a drifted link target — while directories are still replaced (rsync
    /// "does not ignore existing directories").
    pub ignore_existing: bool,
    /// Only update files present on the receiver, do not create new ones
    /// (rsync `--existing`; directories are still created).
    pub existing: bool,
    /// Transfer everything, ignoring the size+mtime quick check
    /// (rsync `--ignore-times`).
    pub ignore_times: bool,
    /// Remove destination files not present in source.
    pub delete: bool,
    /// Bound the delete set to destination paths under these wire-relative
    /// roots (the `--files-from` entries; `None` = the whole destination
    /// may be trimmed).
    pub delete_scope: Option<Vec<String>>,
    /// Re-apply permission bits to already-in-sync files whose mode drifted
    /// (the `rlpt` core is on by default; off with `--no-perms`).
    pub preserve_perms: bool,
    /// Re-apply mtimes to already-in-sync files whose time drifted (off with
    /// `--no-times`, which also flips `size_only`).
    pub preserve_times: bool,
}

/// Pure planner: source manifest × destination manifest → [`SyncPlan`].
#[derive(Debug, Clone, Default)]
pub struct Planner {
    config: PlannerConfig,
}

impl Planner {
    /// Create a planner with the given configuration.
    #[must_use]
    pub fn new(config: PlannerConfig) -> Self {
        Self { config }
    }

    /// Plan a sync between two manifests. Pure function — no I/O.
    #[must_use]
    pub fn plan(&self, source: &Manifest, dest: &Manifest) -> SyncPlan {
        let mut plan = SyncPlan::default();

        let src_map: HashMap<&str, &FileEntry> = source
            .files
            .iter()
            .map(|f| (f.relative_path.as_str(), f))
            .collect();
        let dst_map: HashMap<&str, &FileEntry> = dest
            .files
            .iter()
            .map(|f| (f.relative_path.as_str(), f))
            .collect();

        // Source inode → representative path among the tasks that will leave
        // the representative's content on the destination this run: transferred
        // tasks (created or updated) plus *content-matched* skips (the dest
        // already holds identical bytes, so a later member of the same inode
        // group can link to it without transferring). Members 2..n of a
        // hard-link group become hard links to the first.
        let mut link_reps: HashMap<u64, String> = HashMap::new();

        // Pre-pass: in-sync entries are valid hard-link targets — their
        // destination content already equals the source bytes (same inode ⇒
        // same content), so a member of the same inode group can link to them
        // without transferring (restoring a broken destination relationship
        // instead of degrading the member to a standalone copy). Registration
        // must not depend on sorted order, so every *content-matched* skip is
        // registered before any task is formed. Only content-matched skips
        // qualify: a flag-induced skip (`--ignore-existing`, `--update`) can
        // sit on divergent destination content, which linking a member to it
        // would silently corrupt.
        for src in &source.files {
            if src.kind == FileKind::File
                && let Some(ino) = src.inode
                && let Some(dst) = dst_map.get(src.relative_path.as_str())
                && dst.link_target.is_none()
                && self.compare(src, dst) == SyncAction::Skip
                && self.content_matches(src, dst)
            {
                link_reps.entry(ino).or_insert_with(|| src.relative_path.clone());
            }
        }

        for src in &source.files {
            match dst_map.get(src.relative_path.as_str()) {
                None => {
                    // `--existing`: never create new files (directories are
                    // still created to hold updated files).
                    if self.config.existing && !src.is_dir {
                        plan.skips.push(Self::task_for(src, SyncAction::Skip));
                        continue;
                    }
                    let task = Self::task_for(src, SyncAction::Create);
                    plan.creates
                        .push(Self::assign_hardlink(src, task, &mut link_reps));
                }
                Some(dst_entry) => {
                    // Directories are existence-only: present on both sides
                    // means in sync (a directory is never "updated" — a file
                    // replacing a directory is handled by the file's task).
                    if src.is_dir && dst_entry.is_dir {
                        plan.skips.push(Self::task_for(src, SyncAction::Skip));
                        continue;
                    }
                    let action = self.compare(src, dst_entry);
                    match action {
                        SyncAction::Skip => {
                            // Content in sync: a drift in the preserved
                            // attributes still needs a metadata-only pass
                            // (rsync's attr-only transfer — a `chmod` or
                            // `--checksum`-matched time drift on an otherwise
                            // identical file). Regular content files only:
                            // dirs are existence-only, links' mtime is part of
                            // their compare, specials are contentless.
                            if !src.is_dir
                                && src.link_target.is_none()
                                && !Self::is_special(src.kind)
                                && !self.flag_forced_skip(src, dst_entry)
                                && self.attrs_differ(src, dst_entry)
                            {
                                plan.meta.push(Self::task_for(src, SyncAction::MetaOnly));
                            } else {
                                plan.skips.push(Self::task_for(src, SyncAction::Skip));
                            }
                        }
                        SyncAction::Update => {
                            let task = Self::task_for(src, SyncAction::Update);
                            plan.updates
                                .push(Self::assign_hardlink(src, task, &mut link_reps));
                        }
                        _ => {}
                    }
                }
            }
        }

        if self.config.delete {
            for dst in &dest.files {
                if !src_map.contains_key(dst.relative_path.as_str())
                    && self.delete_in_scope(&dst.relative_path)
                {
                    plan.deletes.push(SyncTask {
                        relative_path: PathBuf::from(&dst.relative_path),
                        action: SyncAction::Delete,
                        source_size: 0,
                        is_dir: dst.is_dir,
                        link_target: None,
                        link_kind: LinkKind::Symlink,
                        link_to: None,
                        special: None,
                        source_path: None,
                        dereferenced: false,
                    });
                }
            }
        }

        plan
    }

    /// Whether `kind` is a contentless special (fifo/socket/device).
    const fn is_special(kind: FileKind) -> bool {
        matches!(
            kind,
            FileKind::Fifo | FileKind::Socket | FileKind::BlockDevice | FileKind::CharDevice
        )
    }

    /// Whether a destination path may be deleted: unrestricted, or under one
    /// of the `--files-from` roots that bound the delete set (rsync scopes
    /// deletes to the listed paths — `--files-from` with one file entry must
    /// not trim the files sitting next to it). The comparison is on wire
    /// paths, so `\` is normalized on Windows sources.
    fn delete_in_scope(&self, rel: &str) -> bool {
        let Some(scope) = &self.config.delete_scope else {
            return true;
        };
        let rel = rel.replace('\\', "/");
        scope.iter().any(|root| {
            let root = root.replace('\\', "/");
            rel == root || rel.starts_with(&format!("{root}/"))
        })
    }

    /// Build a task for a source entry, mirroring the entry's link nature.
    fn task_for(src: &FileEntry, action: SyncAction) -> SyncTask {
        SyncTask {
            relative_path: PathBuf::from(&src.relative_path),
            action,
            source_size: src.size,
            is_dir: src.is_dir,
            link_target: src.link_target.clone(),
            link_kind: src.link_kind,
            link_to: None,
            special: Self::is_special(src.kind).then_some((src.kind, src.rdev)),
            source_path: src.source_path.clone(),
            dereferenced: src.dereferenced,
        }
    }

    /// Mark `task` as a hard link when its source inode already has a
    /// representative — a transferred task, or a content-matched in-sync entry
    /// that keeps its bytes on the destination; otherwise register it as the
    /// representative for that inode.
    fn assign_hardlink(
        src: &FileEntry,
        mut task: SyncTask,
        reps: &mut HashMap<u64, String>,
    ) -> SyncTask {
        // Only regular files form hard-link groups (dirs and specials can
        // never share an inode across paths).
        if let Some(ino) = src.inode
            && src.kind == FileKind::File
        {
            match reps.entry(ino) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(src.relative_path.clone());
                }
                std::collections::hash_map::Entry::Occupied(e) => {
                    task.link_to = Some(e.get().clone());
                }
            }
        }
        task
    }

    /// Whether the source and destination mtimes agree, including the
    /// nanosecond remainder — the receiver restores both at apply time, so a
    /// drift means the file changed.
    fn times_match(src: &FileEntry, dst: &FileEntry) -> bool {
        src.mtime_sec == dst.mtime_sec && src.mtime_nsec == dst.mtime_nsec
    }

    /// Whether a content-matched file still needs a metadata-only update: the
    /// permission bits drift (under `--perms`) or the mtime drifts (under
    /// `--times` — reachable when the quick check used `--checksum`).
    fn attrs_differ(&self, src: &FileEntry, dst: &FileEntry) -> bool {
        // Permission bits only: the destination scan records the raw `st_mode`
        // (file-type bits included), the source scan the permission bits.
        const PERM: u32 = 0o7777;
        let perms = self.config.preserve_perms && (src.mode & PERM) != (dst.mode & PERM);
        let times = self.config.preserve_times && !Self::times_match(src, dst);
        perms || times
    }

    /// Whether a `Skip` was forced by a decision flag (`--update` on a newer
    /// destination, `--ignore-existing` on any existing file) rather than by
    /// the content quick check. Such files are deliberately outside the
    /// run's scope, so the metadata-only pass must not touch them either:
    /// rsync leaves `-u`/`--ignore-existing` skips alone, while an attr pass
    /// would rewind a newer file's mtime to the source's (the mirror of
    /// `compare`'s own flag checks — keep both in sync).
    fn flag_forced_skip(&self, src: &FileEntry, dst: &FileEntry) -> bool {
        (self.config.update_only && dst.mtime_sec > src.mtime_sec)
            || (self.config.ignore_existing && !src.is_dir && !dst.is_dir)
    }

    /// Whether the destination entry already holds the source's content — the
    /// precondition for using it as a hard-link target without transferring
    /// the member. This is the *default* quick check (size+mtime, or BLAKE3
    /// under `--checksum`), independent of the decision flags: a `Skip`
    /// induced by `--ignore-existing` or `--update` can sit on divergent
    /// destination content, which linking a member to it would silently
    /// corrupt.
    fn content_matches(&self, src: &FileEntry, dst: &FileEntry) -> bool {
        if self.config.checksum {
            return match (src.file_hash, dst.file_hash) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
        }
        if self.config.size_only {
            return src.size == dst.size;
        }
        src.size == dst.size && Self::times_match(src, dst)
    }

    /// Decide the action for a file that exists on both sides.
    fn compare(&self, src: &FileEntry, dst: &FileEntry) -> SyncAction {
        // rsync `--ignore-existing`: a transfer rule — any existing *file or
        // symlink* at this name is left untouched, regardless of a type
        // change (file ↔ link), a content difference, or a drifted link
        // target. Directories are exempt ("rsync does not ignore existing
        // directories, or nothing would get done"): a directory blocking a
        // path is still replaced below.
        if self.config.ignore_existing && !src.is_dir && !dst.is_dir {
            return SyncAction::Skip;
        }
        // A file/directory mismatch at the same path is always an update
        // (the file replaces the directory, or vice versa).
        if src.is_dir != dst.is_dir {
            return SyncAction::Update;
        }
        // Symlinks: both must be links to the same target to be in sync.
        if src.link_target.is_some() || dst.link_target.is_some() {
            if src.link_target.is_some() && src.link_target == dst.link_target {
                // Same target string. With times preserved (rsync `-t`) the
                // link's own mtime must agree too — the receiver restores it
                // at creation, so a drift means the source link changed.
                // `--checksum` and `--no-times` fall back to the target
                // string alone (a link has no content hash — the target *is*
                // its content — mirroring the file hash / size-only checks).
                if self.config.checksum || self.config.size_only || Self::times_match(src, dst) {
                    return SyncAction::Skip;
                }
                return SyncAction::Update;
            }
            return SyncAction::Update;
        }
        // Specials (fifo/socket/device): contentless; in sync iff the same
        // kind, device number, and mtime. A special replacing a regular file
        // (or vice versa) is an update.
        if Self::is_special(src.kind) || Self::is_special(dst.kind) {
            return if src.kind == dst.kind && src.rdev == dst.rdev && src.mtime_sec == dst.mtime_sec
            {
                SyncAction::Skip
            } else {
                SyncAction::Update
            };
        }
        if self.config.update_only && dst.mtime_sec > src.mtime_sec {
            return SyncAction::Skip;
        }
        if self.config.ignore_times {
            return SyncAction::Update;
        }
        if self.config.checksum {
            return match (src.file_hash, dst.file_hash) {
                (Some(a), Some(b)) if a == b => SyncAction::Skip,
                _ => SyncAction::Update,
            };
        }
        if self.config.size_only {
            return if src.size == dst.size {
                SyncAction::Skip
            } else {
                SyncAction::Update
            };
        }
        if src.size != dst.size || !Self::times_match(src, dst) {
            SyncAction::Update
        } else {
            SyncAction::Skip
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(rel: &str, size: u64, mtime: i64, hash: Option<[u8; 32]>) -> FileEntry {
        FileEntry {
            relative_path: rel.to_string(),
            size,
            mode: 0o644,
            mtime_sec: mtime,
            file_hash: hash,
            is_dir: false,
            link_target: None,
            link_kind: LinkKind::Symlink,
            inode: None,
            kind: FileKind::File,
            rdev: None,
            uid: None,
            gid: None,
            mtime_nsec: 0,
            atime_sec: 0,
            atime_nsec: 0,
            xattrs: None,
            source_path: None,
            dereferenced: false,
        }
    }

    fn manifest(files: Vec<FileEntry>) -> Manifest {
        Manifest {
            root: PathBuf::from("."),
            total_bytes: files.iter().map(|f| f.size).sum(),
            files,
            skipped: Vec::new(),
        }
    }

    #[test]
    fn plan_new_files() {
        let src = manifest(vec![
            entry("a.txt", 10, 100, None),
            entry("b.txt", 20, 100, None),
        ]);
        let dst = manifest(vec![]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.creates.len(), 2);
        assert!(plan.deletes.is_empty());
    }

    #[test]
    fn plan_unchanged_skip() {
        let src = manifest(vec![entry("a.txt", 10, 100, None)]);
        let dst = manifest(vec![entry("a.txt", 10, 100, None)]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
        assert!(plan.creates.is_empty() && plan.updates.is_empty());
    }

    #[test]
    fn plan_size_change_update() {
        let src = manifest(vec![entry("a.txt", 12, 100, None)]);
        let dst = manifest(vec![entry("a.txt", 10, 100, None)]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.updates.len(), 1);
    }

    #[test]
    fn plan_mtime_change_update() {
        let src = manifest(vec![entry("a.txt", 10, 200, None)]);
        let dst = manifest(vec![entry("a.txt", 10, 100, None)]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.updates.len(), 1);
    }

    #[test]
    fn meta_only_on_mode_drift() {
        // Content in sync (size+mtime match) but the destination's perms
        // drifted — the planner must emit a metadata-only task, not a skip.
        let mut dst = entry("a.txt", 10, 100, None);
        dst.mode = 0o600 | 0o100_000; // raw st_mode with file-type bits
        let src = manifest(vec![entry("a.txt", 10, 100, None)]);
        let dst_manifest = manifest(vec![dst]);
        let plan = Planner::new(PlannerConfig {
            delete: true,
            preserve_perms: true,
            preserve_times: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst_manifest);
        assert_eq!(plan.meta.len(), 1, "mode drift must be a meta task: {plan:?}");
        assert_eq!(plan.skips.len(), 0);
        assert_eq!(plan.meta[0].action, SyncAction::MetaOnly);
        // File-type bits in the raw st_mode must not count as a drift.
        let mut same = dst_manifest.clone();
        same.files[0].mode = 0o644 | 0o100_000;
        let plan2 = Planner::new(PlannerConfig {
            preserve_perms: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &same);
        assert_eq!(plan2.meta.len(), 0, "type bits are not a perm drift");
        assert_eq!(plan2.skips.len(), 1);
    }

    #[test]
    fn update_and_ignore_existing_skips_never_meta_update() {
        // rsync -u skips newer destination files *completely*: a metadata-
        // only pass would rewind the newer file's mtime to the source's.
        // Same for --ignore-existing ("do nothing to existing files"). The
        // flag-forced skips must stay plain skips even when attrs drift.
        let mut dst = entry("a.txt", 10, 100, None);
        dst.mode = 0o600;
        dst.mtime_sec = 200; // newer than the source (100)
        let src = manifest(vec![entry("a.txt", 10, 100, None)]);
        let dst_manifest = manifest(vec![dst]);
        for update in [false, true] {
            let plan = Planner::new(PlannerConfig {
                update_only: update,
                ignore_existing: !update,
                ..PlannerConfig::default()
            })
            .plan(&src, &dst_manifest);
            assert!(
                plan.meta.is_empty(),
                "flag-forced skip must not become a meta update (update_only={update}): {plan:?}"
            );
            assert_eq!(plan.skips.len(), 1);
        }
    }

    #[test]
    fn no_meta_without_preserve_perms() {
        // `--no-perms`: a mode drift is deliberately ignored.
        let mut dst = entry("a.txt", 10, 100, None);
        dst.mode = 0o600;
        let src = manifest(vec![entry("a.txt", 10, 100, None)]);
        let dst_manifest = manifest(vec![dst]);
        let plan = Planner::default().plan(&src, &dst_manifest);
        assert!(plan.meta.is_empty());
        assert_eq!(plan.skips.len(), 1);
    }

    #[test]
    fn meta_only_on_checksum_time_drift() {
        // `--checksum` with equal hashes but a drifted mtime: content matches,
        // times preserved — a metadata-only task (rsync re-applies the time).
        let h = Some([7u8; 32]);
        let mut dst = entry("a.txt", 10, 200, h);
        dst.mtime_nsec = 5;
        let src = manifest(vec![entry("a.txt", 10, 100, h)]);
        let dst_manifest = manifest(vec![dst]);
        let plan = Planner::new(PlannerConfig {
            checksum: true,
            preserve_times: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst_manifest);
        assert_eq!(plan.meta.len(), 1);
        assert!(plan.updates.is_empty(), "content matches — no transfer");
    }

    #[test]
    fn plan_delete() {
        let src = manifest(vec![]);
        let dst = manifest(vec![entry("old.txt", 10, 100, None)]);
        let plan = Planner::new(PlannerConfig {
            delete: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.deletes.len(), 1);
    }

    #[test]
    fn plan_delete_respects_files_from_scope() {
        // `--files-from` + `--delete`: only destination paths under the
        // listed roots (or the roots themselves) are deleted; a sibling
        // sharing a prefix (`data.txt` vs `data/…`) and everything outside
        // the listed paths survive.
        let src = manifest(vec![]);
        let dst = manifest(vec![
            entry("data/old.txt", 10, 100, None),
            entry("data/sub/deep.txt", 10, 100, None),
            entry("unrelated.txt", 10, 100, None),
            entry("data.txt", 10, 100, None),
        ]);
        let plan = Planner::new(PlannerConfig {
            delete: true,
            delete_scope: Some(vec![
                "data/old.txt".to_string(),
                "data/sub".to_string(),
            ]),
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        let deleted: Vec<&str> = plan
            .deletes
            .iter()
            .map(|t| t.relative_path.to_str().unwrap())
            .collect();
        assert_eq!(deleted, vec!["data/old.txt", "data/sub/deep.txt"]);
        // No scope = the whole destination may be trimmed.
        let plan = Planner::new(PlannerConfig {
            delete: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.deletes.len(), 4);
    }

    #[test]
    fn plan_no_delete_by_default() {
        let src = manifest(vec![]);
        let dst = manifest(vec![entry("old.txt", 10, 100, None)]);
        let plan = Planner::default().plan(&src, &dst);
        assert!(plan.deletes.is_empty());
    }

    #[test]
    fn plan_checksum_mode() {
        let h1 = Some([1u8; 32]);
        let h2 = Some([2u8; 32]);
        let src = manifest(vec![entry("a.txt", 10, 100, h1)]);
        let dst = manifest(vec![entry("a.txt", 10, 100, h2)]);
        let plan = Planner::new(PlannerConfig {
            checksum: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.updates.len(), 1);
    }

    #[test]
    fn plan_checksum_match_skip() {
        let h = Some([7u8; 32]);
        let src = manifest(vec![entry("a.txt", 10, 100, h)]);
        let dst = manifest(vec![entry("a.txt", 10, 100, h)]);
        let plan = Planner::new(PlannerConfig {
            checksum: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
    }

    #[test]
    fn plan_update_only() {
        let src = manifest(vec![entry("a.txt", 10, 100, None)]);
        let dst = manifest(vec![entry("a.txt", 20, 200, None)]);
        let plan = Planner::new(PlannerConfig {
            update_only: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
    }

    #[test]
    fn plan_ignore_existing() {
        let src = manifest(vec![entry("a.txt", 50, 300, None)]);
        let dst = manifest(vec![entry("a.txt", 10, 100, None)]);
        let plan = Planner::new(PlannerConfig {
            ignore_existing: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
    }

    #[test]
    fn plan_ignore_existing_preserves_file_over_link() {
        // rsync semantics: a destination *file* at the link's path exists —
        // the type change is ignored and the file is preserved (E3).
        let src = manifest(vec![link_entry("l", "target.txt")]);
        let dst = manifest(vec![entry("l", 10, 100, None)]);
        let plan = Planner::new(PlannerConfig {
            ignore_existing: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
        assert!(plan.updates.is_empty(), "the dest file must not be replaced");
    }

    #[test]
    fn plan_ignore_existing_preserves_link_over_file() {
        // The reverse type change: a stale destination link survives too.
        let src = manifest(vec![entry("l", 10, 100, None)]);
        let dst = manifest(vec![link_entry("l", "target.txt")]);
        let plan = Planner::new(PlannerConfig {
            ignore_existing: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
        assert!(plan.updates.is_empty(), "the dest link must not be replaced");
    }

    #[test]
    fn plan_ignore_existing_skips_link_retarget() {
        // A drifted link target is still "existing": no re-creation.
        let src = manifest(vec![link_entry("l", "new.txt")]);
        let dst = manifest(vec![link_entry("l", "old.txt")]);
        let plan = Planner::new(PlannerConfig {
            ignore_existing: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
        assert!(plan.updates.is_empty());
    }

    #[test]
    fn plan_ignore_existing_still_replaces_directories() {
        // rsync "does not ignore existing directories": a directory blocking
        // a file (or a file blocking a directory) is still replaced.
        let src = manifest(vec![entry("x", 10, 100, None)]);
        let mut dst = manifest(vec![entry("x", 0, 100, None)]);
        dst.files[0].is_dir = true;
        let plan = Planner::new(PlannerConfig {
            ignore_existing: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.updates.len(), 1);
        assert!(!plan.updates[0].is_dir, "the file replaces the directory");

        let mut src = manifest(vec![entry("y", 0, 100, None)]);
        src.files[0].is_dir = true;
        let dst = manifest(vec![entry("y", 10, 100, None)]);
        let plan = Planner::new(PlannerConfig {
            ignore_existing: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.updates.len(), 1);
        assert!(plan.updates[0].is_dir, "the directory replaces the file");
    }

    #[test]
    fn plan_existing_suppresses_file_creates_only() {
        let mut file = entry("new.txt", 50, 300, None);
        let mut dir = entry("emptydir", 0, 300, None);
        dir.is_dir = true;
        file.is_dir = false;
        let src = manifest(vec![file, dir]);
        let dst = manifest(vec![]);
        let plan = Planner::new(PlannerConfig {
            existing: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        // The file create is suppressed; the empty directory is still created.
        assert_eq!(plan.creates.len(), 1);
        assert!(plan.creates[0].is_dir);
        assert_eq!(plan.skips.len(), 1);
        assert!(!plan.skips[0].is_dir);
    }

    #[test]
    fn plan_ignore_times_forces_update() {
        // Identical size+mtime would normally skip; --ignore-times transfers.
        let src = manifest(vec![entry("a.txt", 10, 100, None)]);
        let dst = manifest(vec![entry("a.txt", 10, 100, None)]);
        let plan = Planner::new(PlannerConfig {
            ignore_times: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.updates.len(), 1);
        assert!(plan.skips.is_empty());
    }

    #[test]
    fn plan_ignore_times_respects_update_only() {
        // --update still wins over --ignore-times (destination is newer).
        let src = manifest(vec![entry("a.txt", 10, 100, None)]);
        let dst = manifest(vec![entry("a.txt", 20, 200, None)]);
        let plan = Planner::new(PlannerConfig {
            ignore_times: true,
            update_only: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
    }

    fn link_entry(rel: &str, target: &str) -> FileEntry {
        FileEntry {
            relative_path: rel.to_string(),
            size: 0,
            mode: 0,
            mtime_sec: 100,
            file_hash: None,
            is_dir: false,
            link_target: Some(target.to_string()),
            link_kind: LinkKind::Symlink,
            inode: None,
            kind: FileKind::Symlink,
            rdev: None,
            uid: None,
            gid: None,
            mtime_nsec: 0,
            atime_sec: 0,
            atime_nsec: 0,
            xattrs: None,
            source_path: None,
            dereferenced: false,
        }
    }

    #[test]
    fn plan_symlink_same_target_skips() {
        let src = manifest(vec![link_entry("l", "target.txt")]);
        let dst = manifest(vec![link_entry("l", "target.txt")]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
        assert!(plan.updates.is_empty());
    }

    #[test]
    fn plan_symlink_different_target_updates() {
        let src = manifest(vec![link_entry("l", "new.txt")]);
        let dst = manifest(vec![link_entry("l", "old.txt")]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.updates.len(), 1);
        assert!(plan.updates[0].link_target.is_some());
    }

    #[test]
    fn plan_file_replacing_symlink_updates() {
        let mut file = entry("l", 10, 100, None);
        file.relative_path = "l".to_string();
        let src = manifest(vec![file]);
        let dst = manifest(vec![link_entry("l", "target.txt")]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.updates.len(), 1);
        assert!(plan.updates[0].link_target.is_none());
    }

    #[test]
    fn plan_symlink_create_is_link_task() {
        let src = manifest(vec![link_entry("l", "target.txt")]);
        let dst = manifest(vec![]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.creates.len(), 1);
        assert!(plan.creates[0].link_target.is_some());
        assert!(plan.creates[0].link_to.is_none());
    }

    #[test]
    fn plan_hardlink_group_marks_members() {
        let mut a = entry("a.txt", 10, 100, None);
        let mut b = entry("b.txt", 10, 100, None);
        a.inode = Some(7);
        b.inode = Some(7);
        let src = manifest(vec![a, b]);
        let dst = manifest(vec![]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.creates.len(), 2);
        // First (sorted) is the representative, transferred normally.
        assert!(plan.creates[0].link_to.is_none());
        // Second links to it.
        assert_eq!(plan.creates[1].link_to.as_deref(), Some("a.txt"));
    }

    #[test]
    fn plan_singleton_inode_is_not_a_link() {
        let mut a = entry("a.txt", 10, 100, None);
        a.inode = Some(7);
        let mut b = entry("b.txt", 10, 100, None);
        b.inode = Some(8);
        let src = manifest(vec![a, b]);
        let dst = manifest(vec![]);
        let plan = Planner::default().plan(&src, &dst);
        assert!(plan.creates.iter().all(|t| t.link_to.is_none()));
    }

    #[test]
    fn plan_no_inode_no_hardlink_grouping() {
        let src = manifest(vec![
            entry("a.txt", 10, 100, None),
            entry("b.txt", 10, 100, None),
        ]);
        let dst = manifest(vec![]);
        let plan = Planner::default().plan(&src, &dst);
        assert!(plan.creates.iter().all(|t| t.link_to.is_none()));
    }

    #[test]
    fn plan_hardlink_across_create_and_update() {
        let mut a = entry("a.txt", 10, 100, None);
        a.inode = Some(7);
        let mut b = entry("b.txt", 10, 200, None); // differs → update
        b.inode = Some(7);
        let src = manifest(vec![a, b]);
        let dst = manifest(vec![entry("b.txt", 10, 100, None)]);
        let plan = Planner::default().plan(&src, &dst);
        // a.txt: create (representative), b.txt: update linked to a.txt.
        assert_eq!(plan.creates.len(), 1);
        assert!(plan.creates[0].link_to.is_none());
        assert_eq!(plan.updates.len(), 1);
        assert_eq!(plan.updates[0].link_to.as_deref(), Some("a.txt"));
    }

    #[test]
    fn plan_hardlink_uses_in_sync_representative() {
        // Regression: a destination hard-link group where the representative
        // is already in sync but a member differs (e.g. the destination link
        // was broken externally) must re-link the member instead of degrading
        // it to a standalone file. The source members share an inode, so the
        // representative's in-sync bytes are exactly what the member needs.
        let mut a = entry("a.txt", 10, 100, None);
        let mut b = entry("b.txt", 10, 100, None);
        a.inode = Some(7);
        b.inode = Some(7);
        let src = manifest(vec![a, b]);
        // Dest: a.txt in sync; b.txt replaced with divergent content (newer
        // mtime, still present).
        let dst = manifest(vec![
            entry("a.txt", 10, 100, None),
            entry("b.txt", 10, 200, None),
        ]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
        assert_eq!(plan.skips[0].relative_path, *"a.txt");
        assert_eq!(plan.updates.len(), 1);
        assert_eq!(
            plan.updates[0].link_to.as_deref(),
            Some("a.txt"),
            "the member must re-link to the in-sync representative"
        );
    }

    #[test]
    fn plan_hardlink_in_sync_rep_after_member_sorted_first() {
        // The member can sort *before* the representative ("dup.txt" <
        // "orig.txt"); the in-sync representative must still win, regardless
        // of sorted order.
        let mut dup = entry("dup.txt", 10, 100, None);
        let mut orig = entry("orig.txt", 10, 100, None);
        dup.inode = Some(7);
        orig.inode = Some(7);
        let src = manifest(vec![dup, orig]);
        let dst = manifest(vec![
            entry("dup.txt", 10, 200, None),
            entry("orig.txt", 10, 100, None),
        ]);
        let plan = Planner::default().plan(&src, &dst);
        let update = plan
            .updates
            .iter()
            .find(|t| t.relative_path == *"dup.txt")
            .expect("the member must update");
        assert_eq!(
            update.link_to.as_deref(),
            Some("orig.txt"),
            "the member must link to the in-sync representative even when sorted first"
        );
    }

    #[test]
    fn plan_hardlink_create_links_to_in_sync_representative() {
        // A missing member links to an in-sync representative without
        // transferring content.
        let mut a = entry("a.txt", 10, 100, None);
        let mut b = entry("b.txt", 10, 100, None);
        a.inode = Some(7);
        b.inode = Some(7);
        let src = manifest(vec![a, b]);
        let dst = manifest(vec![entry("a.txt", 10, 100, None)]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.creates.len(), 1);
        assert_eq!(plan.creates[0].link_to.as_deref(), Some("a.txt"));
    }

    #[test]
    fn plan_hardlink_ignores_divergent_ignore_existing_skip() {
        // `--ignore-existing` skips an existing-but-divergent destination
        // entry; a member must NOT link to it — the destination content
        // differs, and linking would silently corrupt the member.
        let mut a = entry("a.txt", 10, 100, None);
        let mut b = entry("b.txt", 10, 100, None);
        a.inode = Some(7);
        b.inode = Some(7);
        let src = manifest(vec![a, b]);
        // Dest: a.txt present but holding different bytes (same path, wildly
        // different size+mtime).
        let dst = manifest(vec![entry("a.txt", 99, 999, None)]);
        let plan = Planner::new(PlannerConfig {
            ignore_existing: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        // a.txt: skip (--ignore-existing), not a valid representative;
        // b.txt: created as a standalone file.
        assert_eq!(plan.creates.len(), 1);
        assert!(plan.creates[0].link_to.is_none());
    }

    #[test]
    fn plan_link_mtime_change_updates() {
        // Regression (rsync -t): a link whose own mtime changed but whose
        // target is unchanged must be re-created so the destination time
        // converges.
        let src = manifest(vec![link_entry("l", "t")]);
        let mut dst = manifest(vec![link_entry("l", "t")]);
        dst.files[0].mtime_sec = 999;
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.updates.len(), 1);
        assert!(plan.updates[0].link_target.is_some());
    }

    #[test]
    fn plan_link_mtime_ignored_under_no_times() {
        // `--no-times` falls back to the target string alone.
        let src = manifest(vec![link_entry("l", "t")]);
        let mut dst = manifest(vec![link_entry("l", "t")]);
        dst.files[0].mtime_sec = 999;
        let plan = Planner::new(PlannerConfig {
            size_only: true,
            ..PlannerConfig::default()
        })
        .plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
        assert!(plan.updates.is_empty());
    }

    #[test]
    fn plan_link_same_target_and_mtime_skips() {
        let src = manifest(vec![link_entry("l", "t")]);
        let dst = manifest(vec![link_entry("l", "t")]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
    }

    #[test]
    fn plan_nsec_difference_updates() {
        // An nsec-only drift is an update — the quick check compares the
        // nanosecond remainder (the receiver restores it at apply time).
        let src = manifest(vec![entry("a.txt", 10, 100, None)]);
        let mut dst = manifest(vec![entry("a.txt", 10, 100, None)]);
        dst.files[0].mtime_nsec = 500_000_000;
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.updates.len(), 1);
    }

    #[test]
    fn plan_nsec_equal_skips() {
        let src = manifest(vec![entry("a.txt", 10, 100, None)]);
        let dst = manifest(vec![entry("a.txt", 10, 100, None)]);
        let plan = Planner::default().plan(&src, &dst);
        assert_eq!(plan.skips.len(), 1);
    }

    #[test]
    fn plan_special_create_update_skip() {
        let mut src = manifest(vec![entry("pipe", 0, 100, None)]);
        src.files[0].kind = FileKind::Fifo;

        // Missing on the destination → create, tagged as a special.
        let plan = Planner::new(PlannerConfig::default()).plan(&src, &Manifest::new(PathBuf::from(".")));
        assert_eq!(plan.creates.len(), 1);
        assert_eq!(plan.creates[0].special, Some((FileKind::Fifo, None)));

        // Same special on both sides (kind + mtime) → skip.
        let mut dst = manifest(vec![entry("pipe", 0, 100, None)]);
        dst.files[0].kind = FileKind::Fifo;
        let plan = Planner::new(PlannerConfig::default()).plan(&src, &dst);
        assert!(plan.skips.iter().any(|t| t.relative_path.as_os_str() == "pipe"));

        // A regular file at the same path → update (the special replaces it).
        let dst = manifest(vec![entry("pipe", 3, 100, None)]);
        let plan = Planner::new(PlannerConfig::default()).plan(&src, &dst);
        assert!(plan.updates.iter().any(|t| t.relative_path.as_os_str() == "pipe"));
        assert_eq!(plan.updates[0].special, Some((FileKind::Fifo, None)));
    }
}
