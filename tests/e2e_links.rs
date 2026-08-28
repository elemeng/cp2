mod common;
use common::*;

#[tokio::test]
async fn symlink_push_and_retarget() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("old.txt"), b"old")
        .await
        .unwrap();
    symlink("old.txt", src.path().join("link.txt")).unwrap();

    // Give the source link a distinctive lstat mtime: the scanner records it
    // and the receiver must restore it (a fresh link would carry the push
    // time instead).
    let src_link = src.path().join("link.txt");
    let cpath = std::ffi::CString::new(src_link.as_os_str().as_bytes()).unwrap();
    let ts = libc::timespec {
        tv_sec: 1_600_000_000,
        tv_nsec: 0,
    };
    let times = [ts, ts];
    let rc = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            cpath.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    assert_eq!(rc, 0, "set source link mtime");

    push_tree(src.path(), dst.path(), &default_options()).await;
    assert_eq!(
        std::fs::read_link(dst.path().join("link.txt")).unwrap(),
        std::path::PathBuf::from("old.txt")
    );
    // The destination link's own mtime must match the source link's.
    let dst_mtime = std::fs::symlink_metadata(dst.path().join("link.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        dst_mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        1_600_000_000,
        "symlink mtime must be restored"
    );

    // Retarget: the destination link must be replaced.
    tokio::fs::write(src.path().join("new.txt"), b"new")
        .await
        .unwrap();
    std::fs::remove_file(src.path().join("link.txt")).unwrap();
    symlink("new.txt", src.path().join("link.txt")).unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;
    assert_eq!(
        std::fs::read_link(dst.path().join("link.txt")).unwrap(),
        std::path::PathBuf::from("new.txt")
    );
    assert_eq!(std::fs::read(dst.path().join("old.txt")).unwrap(), b"old");
}
#[cfg(unix)]
#[tokio::test]
async fn hardlink_push_preserves_linkage() {
    use std::os::unix::fs::MetadataExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("orig.txt"), b"shared content")
        .await
        .unwrap();
    std::fs::hard_link(src.path().join("orig.txt"), src.path().join("dup.txt")).unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;

    assert_eq!(
        std::fs::read(dst.path().join("dup.txt")).unwrap(),
        b"shared content"
    );
    let orig = std::fs::metadata(dst.path().join("orig.txt")).unwrap();
    let dup = std::fs::metadata(dst.path().join("dup.txt")).unwrap();
    assert_eq!(orig.ino(), dup.ino(), "destination must hard-link the pair");
}
#[cfg(unix)]
#[tokio::test]
async fn hardlink_group_restored_when_member_changes() {
    use std::os::unix::fs::MetadataExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("orig.txt"), b"shared content")
        .await
        .unwrap();
    std::fs::hard_link(src.path().join("orig.txt"), src.path().join("dup.txt")).unwrap();
    push_tree(src.path(), dst.path(), &default_options()).await;
    let orig = std::fs::metadata(dst.path().join("orig.txt")).unwrap();
    let dup = std::fs::metadata(dst.path().join("dup.txt")).unwrap();
    assert_eq!(orig.ino(), dup.ino(), "run 1 must hard-link the pair");

    // Break the destination relationship: replace dup.txt with a standalone
    // file while the representative orig.txt stays in sync. The next run must
    // re-link the member to the in-sync representative instead of degrading
    // it to a standalone copy.
    std::fs::remove_file(dst.path().join("dup.txt")).unwrap();
    tokio::fs::write(dst.path().join("dup.txt"), b"tampered")
        .await
        .unwrap();

    let stats = push_tree(src.path(), dst.path(), &default_options()).await;
    // No content bytes travel — the member becomes a hard link to the
    // already-in-sync representative.
    assert_eq!(stats.files_sent, 0, "the member links, it does not re-transfer");
    let orig = std::fs::metadata(dst.path().join("orig.txt")).unwrap();
    let dup = std::fs::metadata(dst.path().join("dup.txt")).unwrap();
    assert_eq!(
        orig.ino(), dup.ino(),
        "the destination hard-link group must be restored"
    );
    assert_eq!(
        std::fs::read(dst.path().join("dup.txt")).unwrap(),
        b"shared content"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn link_mtime_change_retransfers_link() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("target.txt"), b"x")
        .await
        .unwrap();
    symlink("target.txt", src.path().join("l")).unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;

    // Bump the source link's *own* mtime without touching its target: the
    // next run must re-create the destination link so its time converges
    // (rsync -t semantics — target string alone is no longer enough).
    set_link_mtime(&src.path().join("l"), 1_600_000_000);
    push_tree(src.path(), dst.path(), &default_options()).await;

    let dst_mtime = std::fs::symlink_metadata(dst.path().join("l"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(
        dst_mtime, 1_600_000_000,
        "the dest link mtime must converge"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn ignore_existing_preserves_dest_link_over_file() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("l"), b"source file content")
        .await
        .unwrap();
    // The destination has a symlink at the file's path.
    tokio::fs::write(dst.path().join("target.txt"), b"t")
        .await
        .unwrap();
    symlink("target.txt", dst.path().join("l")).unwrap();

    let mut options = default_options();
    options.ignore_existing = true;
    push_tree(src.path(), dst.path(), &options).await;

    assert!(
        dst.path().join("l").is_symlink(),
        "the dest link must survive the push"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn archive_keeps_links_literal() {
    use std::os::unix::fs::symlink;
    // `-a` is the byte-identical mode: links keep their *literal* targets —
    // an absolute internal link is not rewritten to DEST-relative, and an
    // external directory link is kept as a link, not skipped.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let ext = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("real.txt"), b"x")
        .await
        .unwrap();
    tokio::fs::create_dir(src.path().join("links")).await.unwrap();
    let abs_target = src.path().join("real.txt");
    symlink(&abs_target, src.path().join("links/l_abs")).unwrap();
    symlink(ext.path(), src.path().join("links/l_ext_dir")).unwrap();

    let mut options = default_options();
    options.archive = true;
    // `-a` implies `--literal-links` (the CLI derives it; the server derives
    // it from its own `--archive`).
    options.literal_links = true;
    push_tree_with_server_args(src.path(), dst.path(), &options, &["--archive"]).await;
    assert_eq!(
        std::fs::read_link(dst.path().join("links/l_abs")).unwrap(),
        abs_target,
        "-a must keep the literal absolute target (no DEST-relative rewrite)"
    );
    assert!(
        dst.path().join("links/l_ext_dir").is_symlink(),
        "-a must keep an external directory link as a link, not skip it"
    );

    // Second run: the source (literal) and the destination probe (literal)
    // agree, so everything quick-checks to a skip — no re-creation loop.
    let stats2 = push_tree_with_server_args(src.path(), dst.path(), &options, &["--archive"]).await;
    assert_eq!(stats2.files_sent, 0, "a literal second run must skip the links");
}
#[cfg(unix)]
#[tokio::test]
async fn literal_links_flag_preserves_links_without_archive() {
    use std::os::unix::fs::symlink;
    // The standalone `--literal-links` (no `-a`): same literal preservation.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("real.txt"), b"x")
        .await
        .unwrap();
    symlink("real.txt", src.path().join("l")).unwrap();

    let mut options = default_options();
    options.literal_links = true;
    push_tree_with_server_args(src.path(), dst.path(), &options, &["--literal-links"]).await;
    assert!(dst.path().join("l").is_symlink());
    assert_eq!(
        std::fs::read_link(dst.path().join("l")).unwrap(),
        std::path::Path::new("real.txt"),
        "--literal-links keeps the literal target"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn skip_links_overrides_archive() {
    use std::os::unix::fs::symlink;
    // `--skip-links` stays the highest priority even under `-a`: the link is
    // not synced and not followed — the destination never receives it.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("real.txt"), b"payload")
        .await
        .unwrap();
    symlink("real.txt", src.path().join("l")).unwrap();

    let mut options = default_options();
    options.archive = true;
    options.literal_links = true;
    options.preserve_links = false;
    push_tree_with_server_args(src.path(), dst.path(), &options, &["--archive", "--skip-links"]).await;

    assert!(
        !dst.path().join("l").exists(),
        "--skip-links under -a leaves the link out of the sync entirely"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn follow_links_dereferences_file_symlinks() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("target.txt"), b"content")
        .await
        .unwrap();
    symlink("target.txt", src.path().join("link")).unwrap();
    symlink("target.txt", src.path().join("dirlink")).unwrap();
    // A directory-target link: `--follow-links` (rsync -L) recurses into the
    // referent instead of skipping it.
    tokio::fs::create_dir(src.path().join("sub")).await.unwrap();
    tokio::fs::write(src.path().join("sub/inner.txt"), b"deep")
        .await
        .unwrap();
    std::os::unix::fs::symlink("sub", src.path().join("sublink")).unwrap();

    let mut options = default_options();
    options.follow_links = true;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--follow-links"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    // File-target link → a regular file carrying the target's content.
    let meta = std::fs::symlink_metadata(dst.path().join("link")).unwrap();
    assert!(meta.file_type().is_file(), "dereferenced link must be a file");
    assert_eq!(std::fs::read(dst.path().join("link")).unwrap(), b"content");
    // Directory-target link → recursed: a real directory holding the
    // referent's contents at the link's path.
    assert!(dst.path().join("sublink").is_dir());
    assert_eq!(
        std::fs::read(dst.path().join("sublink/inner.txt")).unwrap(),
        b"deep",
        "the directory referent's contents are synced under the link path"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn skip_links_skips_all_symlinks() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("target.txt"), b"content")
        .await
        .unwrap();
    symlink("target.txt", src.path().join("link")).unwrap();
    tokio::fs::create_dir(src.path().join("sub")).await.unwrap();
    std::os::unix::fs::symlink("sub", src.path().join("sublink")).unwrap();

    let mut options = default_options();
    options.preserve_links = false;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--skip-links"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    // `--skip-links` (rsync --no-l): no link is synced and no referent is
    // followed — file-target and directory-target links are all absent.
    assert!(!dst.path().join("link").exists(), "file-target link is skipped");
    assert!(
        !dst.path().join("sublink").exists(),
        "directory-target link is skipped"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn internal_link_rewritten_to_dest_relative() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("target.txt"), b"data").await.unwrap();
    tokio::fs::create_dir(src.path().join("sub")).await.unwrap();
    // An internal link written as a relative path from a subdirectory.
    symlink("../target.txt", src.path().join("sub/link.txt")).unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;

    // The destination link carries the DEST-relative target (same string:
    // the source and destination mirror each other's structure), so the
    // destination is self-contained (spec §3.2).
    let dest_link = dst.path().join("sub/link.txt");
    assert_eq!(
        std::fs::read_link(&dest_link).unwrap(),
        std::path::Path::new("../target.txt")
    );
    assert_eq!(
        std::fs::read(dest_link).unwrap(),
        b"data",
        "the rewritten relative target must resolve on the destination"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn external_file_link_dereferenced_by_default() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("payload.bin");
    tokio::fs::write(&external, b"external content").await.unwrap();
    // An absolute link pointing outside SRC.
    symlink(&external, src.path().join("extlink")).unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;

    // Default: dereferenced — the destination gets a regular file carrying
    // the external target's content (DEST stays self-contained).
    let dest_link = dst.path().join("extlink");
    let meta = std::fs::symlink_metadata(&dest_link).unwrap();
    assert!(meta.file_type().is_file(), "external file link must be dereferenced");
    assert_eq!(std::fs::read(&dest_link).unwrap(), b"external content");
}
#[cfg(unix)]
#[tokio::test]
async fn external_dir_link_skipped_by_default() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    tokio::fs::write(outside.path().join("x.txt"), b"x").await.unwrap();
    symlink(outside.path(), src.path().join("extdir")).unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;

    // Default: skipped, nothing copied, and the run completes cleanly.
    assert!(!dst.path().join("extdir").exists());
}
#[cfg(unix)]
#[tokio::test]
async fn literal_external_file_links_preserves_absolute_target() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("payload.bin");
    tokio::fs::write(&external, b"external content").await.unwrap();
    symlink(&external, src.path().join("extlink")).unwrap();

    let mut options = default_options();
    options.literal_external_file_links = true;
    let (mut child, send, recv) =
        spawn_server_with_args(dst.path(), &["--literal-external-file-links"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    // Opt-in: the link keeps its literal absolute target (0 bytes copied).
    let dest_link = dst.path().join("extlink");
    assert!(dest_link.is_symlink(), "kept link must stay a symlink");
    assert_eq!(std::fs::read_link(&dest_link).unwrap(), external);
}
#[cfg(unix)]
#[tokio::test]
async fn follow_links_recurses_external_dir() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    tokio::fs::create_dir(outside.path().join("nested")).await.unwrap();
    tokio::fs::write(outside.path().join("a.txt"), b"alpha").await.unwrap();
    tokio::fs::write(outside.path().join("nested/b.txt"), b"beta").await.unwrap();
    symlink(outside.path(), src.path().join("extdir")).unwrap();

    let mut options = default_options();
    options.follow_links = true;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--follow-links"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    // The external directory becomes a real directory under the link's path.
    assert!(dst.path().join("extdir").is_dir(), "external dir must be recursed");
    assert_eq!(std::fs::read(dst.path().join("extdir/a.txt")).unwrap(), b"alpha");
    assert_eq!(std::fs::read(dst.path().join("extdir/nested/b.txt")).unwrap(), b"beta");
}
#[cfg(unix)]
#[tokio::test]
async fn follow_links_detects_directory_loops() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let ext_a = tempfile::tempdir().unwrap();
    let ext_b = tempfile::tempdir().unwrap();
    tokio::fs::write(ext_a.path().join("f.txt"), b"f").await.unwrap();
    // Two external directories pointing at each other — the visited set must
    // cut the cycle instead of recursing forever.
    symlink(ext_b.path(), ext_a.path().join("to_b")).unwrap();
    symlink(ext_a.path(), ext_b.path().join("to_a")).unwrap();
    symlink(ext_a.path(), src.path().join("enter")).unwrap();

    let mut options = default_options();
    options.follow_links = true;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--follow-links"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed (the loop must not hang)");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    // The recursed content landed once; the back-link is skipped as a loop.
    assert_eq!(std::fs::read(dst.path().join("enter/f.txt")).unwrap(), b"f");
    assert!(
        dst.path().join("enter/to_b").is_dir(),
        "the second external dir is recursed as a real directory"
    );
    assert!(
        !dst.path().join("enter/to_b/to_a").exists(),
        "the cycle back-link must be skipped"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn archive_keeps_setuid_setgid_sticky() {
    use std::os::unix::fs::PermissionsExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let suid = src.path().join("suid.txt");
    tokio::fs::write(&suid, b"x").await.unwrap();
    std::fs::set_permissions(&suid, std::fs::Permissions::from_mode(0o4755)).unwrap();
    let sticky = src.path().join("sticky.txt");
    tokio::fs::write(&sticky, b"y").await.unwrap();
    std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).unwrap();

    let mut options = default_options();
    options.archive = true;
    push_tree(src.path(), dst.path(), &options).await;

    // `-a` (byte-identical mode): SUID/SGID/Sticky survive the wire.
    let suid_mode = std::fs::metadata(dst.path().join("suid.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(suid_mode, 0o4755, "-a must keep the setuid bit");
    let sticky_mode = std::fs::metadata(dst.path().join("sticky.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(sticky_mode, 0o1777, "-a must keep the sticky bit");
}
#[cfg(unix)]
#[tokio::test]
async fn literal_external_file_links_then_default_converges_to_file() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external = outside.path().join("payload.bin");
    tokio::fs::write(&external, b"external content").await.unwrap();
    symlink(&external, src.path().join("extfile")).unwrap();

    // Run 1: `--literal-external-file-links` leaves a real symlink on the
    // destination.
    let mut keep = default_options();
    keep.literal_external_file_links = true;
    let (mut child, send, recv) =
        spawn_server_with_args(dst.path(), &["--literal-external-file-links"]);
    let mut executor = Executor::new(send, recv);
    executor.push(src.path(), &keep).await.expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());
    assert!(dst.path().join("extfile").is_symlink());

    // Run 2: default policy dereferences — the stale symlink must be
    // replaced by a real file (the destination converges to the source's
    // current representation).
    push_tree(src.path(), dst.path(), &default_options()).await;
    let dest_link = dst.path().join("extfile");
    assert!(
        dest_link.is_file() && !dest_link.is_symlink(),
        "a policy change must replace the stale destination link with a file"
    );
    assert_eq!(std::fs::read(&dest_link).unwrap(), b"external content");
}
#[cfg(unix)]
#[tokio::test]
async fn follow_links_then_default_delete_keeps_recursed_subtree() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    tokio::fs::write(outside.path().join("a.txt"), b"alpha")
        .await
        .unwrap();
    symlink(outside.path(), src.path().join("extdir")).unwrap();

    // Run 1: `--follow-links` recurses the external dir onto the destination.
    let mut follow = default_options();
    follow.follow_links = true;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--follow-links"]);
    let mut executor = Executor::new(send, recv);
    executor.push(src.path(), &follow).await.expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());
    assert!(dst.path().join("extdir/a.txt").exists());

    // Run 2: default policy + `--delete` — the external dir is skipped, and
    // the previously recursed subtree must survive deletion: the path still
    // exists in the source, it is simply not transferred under the current
    // policy, so neither it nor its contents are "extras".
    let mut del = default_options();
    del.delete = true;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--delete"]);
    let mut executor = Executor::new(send, recv);
    executor.push(src.path(), &del).await.expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());
    assert!(
        dst.path().join("extdir/a.txt").exists(),
        "a policy-skipped external dir must protect its recursed subtree from --delete"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn delete_protects_policy_skipped_links() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    tokio::fs::write(outside.path().join("x.txt"), b"x").await.unwrap();
    // The source still has the external-dir link — it is policy-skipped, not
    // removed — so a `--delete` run must not remove the destination's copy.
    symlink(outside.path(), src.path().join("extdir")).unwrap();
    symlink(outside.path(), dst.path().join("extdir")).unwrap();
    tokio::fs::write(dst.path().join("keep.txt"), b"k").await.unwrap();

    let mut options = default_options();
    options.delete = true;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--delete"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    assert!(
        dst.path().join("extdir").exists(),
        "a policy-skipped source link must survive --delete"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ignore_existing_pull_preserves_dest_file_over_link() {
    // Pull mirror of the push case: the server-side planner must honor
    // --ignore-existing (riding the PullRequest frame) and leave the
    // restore-side regular file alone even though the source is a symlink.
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    tokio::fs::write(serve.path().join("target.txt"), b"t")
        .await
        .unwrap();
    std::os::unix::fs::symlink("target.txt", serve.path().join("data.txt")).unwrap();
    tokio::fs::write(restore.path().join("data.txt"), b"orig-file")
        .await
        .unwrap();

    let mut options = default_options();
    options.ignore_existing = true;
    pull_tree(serve.path(), restore.path(), &options).await;
    let meta = std::fs::symlink_metadata(restore.path().join("data.txt")).unwrap();
    assert!(
        meta.file_type().is_file(),
        "--ignore-existing on pull must keep the existing file over the source link"
    );
    assert_eq!(
        std::fs::read(restore.path().join("data.txt")).unwrap(),
        b"orig-file"
    );
}

