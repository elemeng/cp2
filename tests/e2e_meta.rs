mod common;
use common::*;

#[tokio::test]
async fn backup_keeps_previous_version() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("doc.txt"), b"v1")
        .await
        .unwrap();
    push_tree(src.path(), dst.path(), &default_options()).await;

    // The receiver (server child) runs with --backup.
    tokio::fs::write(src.path().join("doc.txt"), b"v2")
        .await
        .unwrap();
    set_future_mtime(&src.path().join("doc.txt"));
    push_tree_with_server_args(src.path(), dst.path(), &default_options(), &["--backup"]).await;

    assert_eq!(std::fs::read(dst.path().join("doc.txt")).unwrap(), b"v2");
    assert_eq!(
        std::fs::read(dst.path().join("doc.txt~")).unwrap(),
        b"v1",
        "--backup must keep the replaced file as doc.txt~"
    );
}
#[tokio::test]
async fn backup_covers_deleted_files() {
    // Regression: rsync's `--backup` also backs up files that are about to be
    // *deleted* (with `--delete`), not only files about to be replaced.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("keep.txt"), b"keep")
        .await
        .unwrap();
    push_tree(src.path(), dst.path(), &default_options()).await;

    // Destination gains a stale file the source no longer has.
    tokio::fs::write(dst.path().join("stale.txt"), b"stale")
        .await
        .unwrap();

    let mut options = default_options();
    options.delete = true;
    push_tree_with_server_args(src.path(), dst.path(), &options, &["--delete", "--backup"]).await;

    assert!(dst.path().join("keep.txt").exists());
    assert!(!dst.path().join("stale.txt").exists());
    assert_eq!(
        std::fs::read(dst.path().join("stale.txt~")).unwrap(),
        b"stale",
        "--backup must keep the deleted file as stale.txt~"
    );
}
#[tokio::test]
async fn max_delete_refuses_excess() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("keep.txt"), b"keep")
        .await
        .unwrap();
    push_tree(src.path(), dst.path(), &default_options()).await;
    // Destination gains two files the source lacks.
    tokio::fs::write(dst.path().join("stale1.txt"), b"s1")
        .await
        .unwrap();
    tokio::fs::write(dst.path().join("stale2.txt"), b"s2")
        .await
        .unwrap();

    let mut options = default_options();
    options.delete = true;

    // Server capped at 1 delete: the sync must fail before removing anything.
    let (mut child, send, recv) =
        spawn_server_with_args(dst.path(), &["--delete", "--max-delete", "1"]);
    let mut executor = Executor::new(send, recv);
    let err = executor.push(src.path(), &options).await.unwrap_err();
    drop(executor);
    let _ = child.wait().await;
    assert!(
        err.to_string().contains("max-delete"),
        "expected max-delete error, got: {err}"
    );
    // Nothing was deleted.
    assert!(dst.path().join("stale1.txt").exists());
    assert!(dst.path().join("stale2.txt").exists());
}
#[tokio::test]
async fn remove_source_files_deletes_sources_after_push() {
    // Move-off workflow: files are removed from the source only after the
    // receiver confirms them; directories and symlinks are never removed.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("a.fits"), b"AAAA")
        .await
        .unwrap();
    tokio::fs::write(src.path().join("b.fits"), b"BBBB")
        .await
        .unwrap();
    tokio::fs::create_dir(src.path().join("keep_dir")).await.unwrap();
    tokio::fs::write(src.path().join("keep_dir/c.fits"), b"CCCC")
        .await
        .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.fits", src.path().join("link.fits")).unwrap();

    let options = ExecutorOptions {
        remove_source_files: true,
        ..ExecutorOptions::default()
    };
    push_tree(src.path(), dst.path(), &options).await;

    // Files gone from the source; the directory (and symlink) stay.
    assert!(!src.path().join("a.fits").exists());
    assert!(!src.path().join("b.fits").exists());
    assert!(!src.path().join("keep_dir/c.fits").exists());
    assert!(src.path().join("keep_dir").is_dir());
    #[cfg(unix)]
    // `is_symlink` (not `exists`): the link survives as a dangling link now
    // that its target was moved off.
    assert!(src.path().join("link.fits").is_symlink());
    // Destination is complete.
    assert_eq!(std::fs::read(dst.path().join("a.fits")).unwrap(), b"AAAA");
    assert_eq!(
        std::fs::read(dst.path().join("keep_dir/c.fits")).unwrap(),
        b"CCCC"
    );
}
#[tokio::test]
async fn remove_source_files_on_pull_removes_server_sources() {
    // On pull the server is the sender, so `--remove-source-files` is
    // forwarded to it; the server deletes its own source tree after the
    // client (receiver) acks.
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    tokio::fs::write(serve.path().join("img.fits"), b"IMG")
        .await
        .unwrap();

    let mut options = default_options();
    options.remove_source_files = true;
    let (mut child, send, recv) =
        spawn_server_with_args(serve.path(), &["--remove-source-files"]);
    let mut executor = Executor::new(send, recv);
    executor
        .pull(restore.path(), &options)
        .await
        .expect("pull failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");

    assert_eq!(
        std::fs::read(restore.path().join("img.fits")).unwrap(),
        b"IMG"
    );
    assert!(!serve.path().join("img.fits").exists());
}
#[tokio::test]
async fn remove_source_files_keeps_receiver_skipped_sources() {
    // The receiver cannot apply the over-long name (its staged temp exceeds
    // NAME_MAX), so its skip must protect the source file from
    // `--remove-source-files` — while the successfully transferred file is
    // still removed.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let long_name = "y".repeat(240);
    tokio::fs::write(src.path().join(&long_name), vec![0xCD; 64])
        .await
        .unwrap();
    tokio::fs::write(src.path().join("ok.txt"), b"fine")
        .await
        .unwrap();

    let options = ExecutorOptions {
        remove_source_files: true,
        ..ExecutorOptions::default()
    };
    let stats = push_tree(src.path(), dst.path(), &options).await;

    assert_eq!(stats.skipped.len(), 1);
    // The transferred file was moved off the source...
    assert!(!src.path().join("ok.txt").exists());
    assert!(dst.path().join("ok.txt").exists());
    // ...but the receiver-skipped file's source is kept.
    assert!(src.path().join(&long_name).exists());
}
#[tokio::test]
async fn remove_source_files_verifies_chunked_large_files() {
    // Large new files stream as chunks; the sender hashes them on the fly
    // (no re-read) and `--remove-source-files` deletes the source only after
    // the receiver's written-bytes hash matches.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let payload = vec![0xABu8; 8 * 1024 * 1024]; // > MEDIUM_FILE_MAX (4MB) → chunked
    tokio::fs::write(src.path().join("big.fits"), &payload)
        .await
        .unwrap();

    let options = ExecutorOptions {
        remove_source_files: true,
        ..ExecutorOptions::default()
    };
    push_tree(src.path(), dst.path(), &options).await;

    assert!(!src.path().join("big.fits").exists());
    assert_eq!(
        std::fs::read(dst.path().join("big.fits")).unwrap(),
        payload
    );
}
#[tokio::test]
async fn verify_confirms_destination_without_deleting() {
    // `--verify` exercises the same on-the-fly hash guard as
    // `--remove-source-files`, but deletes nothing — a failed verification
    // would surface as a skipped file instead.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("a.txt"), b"alpha")
        .await
        .unwrap();
    // > MEDIUM_FILE_MAX (4MB) → the chunked path with on-the-fly hashing.
    tokio::fs::write(src.path().join("big.fits"), vec![0xCDu8; 5 * 1024 * 1024])
        .await
        .unwrap();

    let options = ExecutorOptions {
        verify: true,
        ..ExecutorOptions::default()
    };
    let stats = push_tree(src.path(), dst.path(), &options).await;

    assert_eq!(stats.skipped.len(), 0, "verification must pass cleanly");
    // Sources are untouched in verify-only mode.
    assert!(src.path().join("a.txt").exists());
    assert!(src.path().join("big.fits").exists());
    // Destinations are byte-correct.
    assert_eq!(std::fs::read(dst.path().join("a.txt")).unwrap(), b"alpha");
    assert_eq!(std::fs::read(dst.path().join("big.fits")).unwrap().len(), 5 * 1024 * 1024);
}
#[cfg(unix)]
#[tokio::test]
async fn archive_creates_special_files() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let pipe = src.path().join("pipe");
    let c = std::ffi::CString::new(pipe.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o640) }, 0);
    tokio::fs::write(src.path().join("a.txt"), b"x")
        .await
        .unwrap();

    // Without `-a`, specials are not recreated (and the sync still works).
    push_tree(src.path(), dst.path(), &default_options()).await;
    assert!(dst.path().join("a.txt").exists());
    assert!(!dst.path().join("pipe").exists());

    // With `-a` (receiver gets the flag via the server argv, like server_args
    // forwards it), the fifo is recreated with its mode.
    let mut options = default_options();
    options.archive = true;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--archive"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("archive push failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");

    let meta = std::fs::symlink_metadata(dst.path().join("pipe")).unwrap();
    assert_eq!(
        meta.mode() & libc::S_IFMT,
        libc::S_IFIFO,
        "dst pipe must be a fifo"
    );
    assert_eq!(meta.mode() & 0o7777, 0o640, "fifo mode preserved");
}
#[cfg(unix)]
#[tokio::test]
async fn archive_remove_source_files_keeps_specials() {
    use std::os::unix::ffi::OsStrExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let pipe = src.path().join("pipe");
    let c = std::ffi::CString::new(pipe.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o640) }, 0);
    tokio::fs::write(src.path().join("a.txt"), b"x")
        .await
        .unwrap();

    let mut options = default_options();
    options.archive = true;
    options.remove_source_files = true;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--archive"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    // The regular file was moved off; the special survives on the source.
    assert!(!src.path().join("a.txt").exists());
    assert!(src.path().join("pipe").exists());
    assert!(dst.path().join("pipe").exists());
}
#[tokio::test]
async fn no_times_leaves_transfer_time_and_uses_size_only_check() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let file = src.path().join("a.txt");
    tokio::fs::write(&file, b"payload").await.unwrap();
    // Pin the source mtime to a far-past value (2000-01-01).
    set_file_mtime_ns(&file, 946_684_800, 0);

    let mut options = default_options();
    options.preserve_times = false;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--no-times"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    // The destination got the transfer time, not the source's.
    let dst_meta = std::fs::metadata(dst.path().join("a.txt")).unwrap();
    let src_meta = std::fs::metadata(&file).unwrap();
    assert_ne!(
        dst_meta.modified().unwrap(),
        src_meta.modified().unwrap(),
        "without -t the destination must not carry the source mtime"
    );

    // Second run: size-only quick check sees the same size → nothing moves.
    let stats = push_tree_with_server_args(
        src.path(),
        dst.path(),
        &options,
        &["--no-times"],
    ).await;
    assert_eq!(stats.files_sent, 0, "size-only check must skip unchanged files");
}
#[cfg(unix)]
#[tokio::test]
async fn no_perms_sets_explicit_0644_0755() {
    use std::os::unix::fs::PermissionsExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let file = src.path().join("a.txt");
    tokio::fs::write(&file, b"x").await.unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    tokio::fs::create_dir(src.path().join("sub")).await.unwrap();

    let mut options = default_options();
    options.preserve_perms = false;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--no-perms"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    // `--no-perms` no longer means "leave the umask default": the sender
    // computes explicit defaults (spec §2.2) — files 0644, directories 0755 —
    // and the receiver applies them.
    let mode = std::fs::metadata(dst.path().join("a.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(mode, 0o644, "--no-perms must set the explicit file default");
    let dir_mode = std::fs::metadata(dst.path().join("sub"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(dir_mode, 0o755, "--no-perms must set the explicit dir default");
}
#[tokio::test]
async fn no_recursive_skips_subdirectories() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("a.txt"), b"top")
        .await
        .unwrap();
    tokio::fs::create_dir(src.path().join("sub")).await.unwrap();
    tokio::fs::write(src.path().join("sub/b.txt"), b"deep")
        .await
        .unwrap();

    let mut options = default_options();
    options.recursive = false;
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--no-recursive"]);
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    assert!(dst.path().join("a.txt").exists());
    assert!(!dst.path().join("sub/b.txt").exists());
    assert!(!dst.path().join("sub").exists());
}
#[cfg(unix)]
#[tokio::test]
async fn mode_clears_setuid_setgid_sticky() {
    use std::os::unix::fs::PermissionsExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let suid = src.path().join("suid.txt");
    tokio::fs::write(&suid, b"x").await.unwrap();
    std::fs::set_permissions(&suid, std::fs::Permissions::from_mode(0o4755)).unwrap();
    let sticky = src.path().join("sticky.txt");
    tokio::fs::write(&sticky, b"y").await.unwrap();
    std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;

    // Spec §2.2: SUID/SGID/Sticky are force-cleared on the wire (0-Root
    // never lets a setuid bit reach the destination).
    let suid_mode = std::fs::metadata(dst.path().join("suid.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(suid_mode, 0o755, "setuid bit must be cleared");
    let sticky_mode = std::fs::metadata(dst.path().join("sticky.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(sticky_mode, 0o777, "sticky bit must be cleared (rwxrwxrwx only)");
}
// No Windows counterpart: FILETIME carries 100 ns granularity, so the exact
// 123_456_789 ns remainder cannot be represented (nor asserted) there.
#[cfg(unix)]
#[tokio::test]
async fn nanosecond_mtime_preserved_by_default_and_archive() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let file = src.path().join("a.txt");
    tokio::fs::write(&file, b"x").await.unwrap();
    set_file_mtime_ns(&file, 1_600_000_000, 123_456_789);

    // The default restores the nanosecond remainder too (the quick check
    // compares it unconditionally, so whole-second application would
    // re-trigger on every run).
    push_tree(src.path(), dst.path(), &default_options()).await;
    let d = std::fs::metadata(dst.path().join("a.txt"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    assert_eq!(d.as_secs(), 1_600_000_000);
    assert_eq!(
        d.subsec_nanos(), 123_456_789,
        "the default must restore the nanosecond remainder"
    );

    // `-a` agrees (the server receiver runs with `--archive`, as the real
    // CLI forwards; owner/group and SUID bits are the archive-only parts).
    let dst2 = tempfile::tempdir().unwrap();
    let mut options = default_options();
    options.archive = true;
    push_tree_with_server_args(src.path(), dst2.path(), &options, &["--archive"]).await;
    let d2 = std::fs::metadata(dst2.path().join("a.txt"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    assert_eq!(
        d2.subsec_nanos(), 123_456_789,
        "-a must restore the nanosecond remainder too"
    );

    // And a second run quick-checks to a skip (no nsec mismatch loop).
    let stats = push_tree(src.path(), dst.path(), &default_options()).await;
    assert_eq!(stats.files_sent, 0, "a second default run must skip the file");
}


#[tokio::test]
async fn metadata_only_update_applies_drifted_mode() {
    // rsync's attr-only transfer: an in-sync file whose perms were chmod'ed
    // gets its mode re-applied without any content bytes moving, and an
    // unchanged third run is a clean skip (no churn).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        tokio::fs::write(src.path().join("doc.txt"), b"v1")
            .await
            .unwrap();
        push_tree(src.path(), dst.path(), &default_options()).await;
        let before = std::fs::metadata(dst.path().join("doc.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;

        // Drift the source's perms; the re-sync must re-apply them.
        std::fs::set_permissions(
            src.path().join("doc.txt"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let stats = push_tree(src.path(), dst.path(), &default_options()).await;
        assert_eq!(stats.files_sent, 1, "chmod'ed in-sync file re-applied");
        assert_eq!(stats.bytes_transferred, 0, "no content bytes moved");
        let after = std::fs::metadata(dst.path().join("doc.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(after, 0o600, "mode must be re-applied from the source");
        assert_ne!(after, before);

        // No churn: an unchanged run transfers nothing.
        let stats = push_tree(src.path(), dst.path(), &default_options()).await;
        assert_eq!(stats.files_sent, 0, "unchanged run must not re-apply");
    }
}

#[tokio::test]
async fn backup_on_pull_keeps_previous_version() {
    // The pull receiver is the client: --backup must keep the replaced file
    // as <name>~ there too, not only on push.
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    tokio::fs::write(serve.path().join("keep.txt"), b"v2-longer")
        .await
        .unwrap();
    tokio::fs::write(restore.path().join("keep.txt"), b"v1")
        .await
        .unwrap();

    let mut options = default_options();
    options.backup = true;
    pull_tree(serve.path(), restore.path(), &options).await;
    assert_eq!(
        std::fs::read(restore.path().join("keep.txt")).unwrap(),
        b"v2-longer"
    );
    assert_eq!(
        std::fs::read(restore.path().join("keep.txt~")).unwrap(),
        b"v1",
        "--backup must keep the previous version on pull"
    );
}

#[tokio::test]
async fn verify_on_pull_hashes_the_received_bytes() {
    // On pull the *server* is the sender: --verify must ride its argv so the
    // IndexRequest asks the client receiver to hash every applied file
    // (remove-source-files relies on the same wiring).
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    let data = pseudo_random(3 * 1024 * 1024);
    tokio::fs::write(serve.path().join("big.bin"), &data)
        .await
        .unwrap();

    let mut options = default_options();
    options.verify = true;
    let (mut child, send, recv) = spawn_server_with_args(serve.path(), &["--verify"]);
    let mut executor = Executor::new(send, recv);
    let stats = executor
        .pull(restore.path(), &options)
        .await
        .expect("verified pull failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");
    assert_eq!(stats.files_received, 1);
    assert_eq!(
        std::fs::read(restore.path().join("big.bin")).unwrap(),
        data
    );
}

#[tokio::test]
async fn max_delete_on_pull_aborts_excess() {
    // The pull receiver is the client: --max-delete must be enforced by the
    // delete-executing side there too (on push that's the server receiver).
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    tokio::fs::write(serve.path().join("keep.txt"), b"keep")
        .await
        .unwrap();
    // Restore side carries five stale extras the pull would delete.
    for i in 0..5 {
        tokio::fs::write(restore.path().join(format!("stale{i}.txt")), b"s")
            .await
            .unwrap();
    }

    let mut options = default_options();
    options.delete = true;
    options.max_delete = Some(2);
    let (mut child, send, recv) = spawn_server(serve.path());
    let mut executor = Executor::new(send, recv);
    let err = executor.pull(restore.path(), &options).await.unwrap_err();
    drop(executor);
    let _ = child.wait().await;
    assert!(
        err.to_string().contains("max-delete"),
        "expected max-delete error, got: {err}"
    );
    // The abort came before any deletion.
    for i in 0..5 {
        assert!(restore.path().join(format!("stale{i}.txt")).exists());
    }
}

#[tokio::test]
async fn no_times_on_pull_leaves_transfer_time() {
    // rlpt opt-out on the pull: the restore mtime must be the transfer time,
    // not the source's ancient value (server sender scans with --no-times,
    // client receiver applies no mtime).
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    tokio::fs::write(serve.path().join("f.txt"), b"x")
        .await
        .unwrap();
    set_file_mtime_ns(&serve.path().join("f.txt"), 1_000_000, 0);

    let mut options = default_options();
    options.preserve_times = false;
    let (mut child, send, recv) = spawn_server_with_args(serve.path(), &["--no-times"]);
    let mut executor = Executor::new(send, recv);
    executor
        .pull(restore.path(), &options)
        .await
        .expect("pull failed");
    drop(executor);
    let _ = child.wait().await;
    let restored = std::fs::metadata(restore.path().join("f.txt")).unwrap();
    assert!(
        restored.modified().unwrap() > std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        "--no-times must leave the transfer time, not the source mtime"
    );
}

#[tokio::test]
async fn no_recursive_on_pull_skips_subdirectories() {
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    tokio::fs::create_dir(serve.path().join("sub")).await.unwrap();
    tokio::fs::write(serve.path().join("sub/inner.txt"), b"i")
        .await
        .unwrap();
    tokio::fs::write(serve.path().join("top.txt"), b"t")
        .await
        .unwrap();

    let mut options = default_options();
    options.recursive = false;
    let (mut child, send, recv) = spawn_server_with_args(serve.path(), &["--no-recursive"]);
    let mut executor = Executor::new(send, recv);
    executor
        .pull(restore.path(), &options)
        .await
        .expect("pull failed");
    drop(executor);
    let _ = child.wait().await;
    assert_eq!(
        std::fs::read(restore.path().join("top.txt")).unwrap(),
        b"t"
    );
    assert!(
        !restore.path().join("sub/inner.txt").exists(),
        "--no-recursive must skip the subdirectory on pull"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn archive_on_pull_creates_special_files() {
    use std::os::unix::fs::FileTypeExt;
    // The pull *sender* is the server: it must scan-and-send specials only
    // when its own argv carries --archive; the client receiver creates them.
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    std::fs::File::create(serve.path().join("fifo")).unwrap();
    std::fs::remove_file(serve.path().join("fifo")).unwrap();
    unsafe {
        libc::mkfifo(
            std::ffi::CString::new(serve.path().join("fifo").to_str().unwrap().as_bytes())
                .unwrap()
                .as_ptr(),
            0o600,
        );
    }

    let mut options = default_options();
    options.archive = true;
    let (mut child, send, recv) = spawn_server_with_args(serve.path(), &["--archive"]);
    let mut executor = Executor::new(send, recv);
    executor
        .pull(restore.path(), &options)
        .await
        .expect("pull failed");
    drop(executor);
    let _ = child.wait().await;
    let meta = std::fs::symlink_metadata(restore.path().join("fifo")).unwrap();
    assert!(
        meta.file_type().is_fifo(),
        "-a on a pull must recreate the fifo"
    );
}
