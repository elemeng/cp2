mod common;
use common::*;

#[tokio::test]
async fn whole_file_push_over_server_child() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("hello.txt"), b"hello world")
        .await
        .unwrap();
    tokio::fs::create_dir(src.path().join("sub")).await.unwrap();
    tokio::fs::write(src.path().join("sub/nested.txt"), b"nested content")
        .await
        .unwrap();

    let stats = push_tree(src.path(), dst.path(), &default_options()).await;

    assert_eq!(stats.files_sent, 3);
    assert_eq!(
        std::fs::read(dst.path().join("hello.txt")).unwrap(),
        b"hello world"
    );
    assert_eq!(
        std::fs::read(dst.path().join("sub/nested.txt")).unwrap(),
        b"nested content"
    );
    assert!(dst.path().join("sub").is_dir());
}
#[tokio::test]
async fn pull_over_server_child() {
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();

    tokio::fs::write(serve.path().join("a.txt"), b"aaa")
        .await
        .unwrap();
    tokio::fs::write(serve.path().join("b.txt"), b"bbb")
        .await
        .unwrap();

    let stats = pull_tree(serve.path(), restore.path(), &default_options()).await;

    assert_eq!(stats.files_received, 2);
    assert_eq!(std::fs::read(restore.path().join("a.txt")).unwrap(), b"aaa");
    assert_eq!(std::fs::read(restore.path().join("b.txt")).unwrap(), b"bbb");
}
#[tokio::test]
async fn delta_update_over_server_child() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    // A mid-size file (above the small-file batch threshold, below the
    // delta tier) so updates take the direct whole-file path; the 20MB test
    // below exercises the signature/delta path.
    let data: Vec<u8> = pseudo_random(2 * 1024 * 1024);
    tokio::fs::write(src.path().join("big.bin"), &data)
        .await
        .unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;
    assert_eq!(std::fs::read(dst.path().join("big.bin")).unwrap(), data);

    // One-byte edit in the middle → the changed file is re-sent. Give the
    // file a distinct mtime: the planner matches size+mtime, and mtime is
    // second-granular, so two writes in the same second would otherwise look
    // unchanged.
    let mut v2 = data.clone();
    v2[1_000_000] ^= 0xFF;
    tokio::fs::write(src.path().join("big.bin"), &v2)
        .await
        .unwrap();
    set_future_mtime(&src.path().join("big.bin"));

    let stats = push_tree(src.path(), dst.path(), &default_options()).await;
    assert_eq!(stats.files_sent, 1);
    assert_eq!(std::fs::read(dst.path().join("big.bin")).unwrap(), v2);
}
#[tokio::test]
async fn large_file_push_and_delta_update() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    let data: Vec<u8> = pseudo_random(20 * 1024 * 1024);
    tokio::fs::write(src.path().join("large.bin"), &data)
        .await
        .unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;
    assert_eq!(std::fs::read(dst.path().join("large.bin")).unwrap(), data);

    // Insert 1 KB in the middle: the delta engine re-sends only what changed.
    let mut v2 = Vec::with_capacity(data.len() + 1024);
    v2.extend_from_slice(&data[..10 * 1024 * 1024]);
    v2.extend(std::iter::repeat_n(0xAB, 1024));
    v2.extend_from_slice(&data[10 * 1024 * 1024..]);
    tokio::fs::write(src.path().join("large.bin"), &v2)
        .await
        .unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;
    let received = std::fs::read(dst.path().join("large.bin")).unwrap();
    assert_eq!(received.len(), v2.len());
    assert_eq!(received, v2);
}
#[tokio::test]
async fn multi_delta_window_over_server_child() {
    // The second-sync scenario: several large files with small edits, all
    // delta-transferred. `-j 2` forces a compute window of 2, so the sliding
    // window drains mid-plan (5 jobs > 2) and every job is joined in order.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    // 12 MiB each: above DELTA_MIN_SIZE, so the update takes the
    // signature/delta path.
    let mut files = Vec::new();
    for i in 0..5 {
        let data: Vec<u8> = pseudo_random(12 * 1024 * 1024 + i);
        tokio::fs::write(src.path().join(format!("f{i}.bin")), &data)
            .await
            .unwrap();
        files.push(data);
    }
    push_tree(src.path(), dst.path(), &default_options()).await;

    // Edit each file in place (append a byte: size change defeats the quick
    // check) and push again — all five must delta-transfer through the
    // window and land byte-identical.
    let mut options = default_options();
    options.jobs = Some(2);
    let mut edited = Vec::new();
    for (i, data) in files.iter().enumerate() {
        let mut v2 = data.clone();
        v2.push(u8::try_from(i).unwrap());
        tokio::fs::write(src.path().join(format!("f{i}.bin")), &v2)
            .await
            .unwrap();
        edited.push(v2);
    }
    let stats = push_tree(src.path(), dst.path(), &options).await;
    assert_eq!(stats.files_sent, 5, "all five edited files must transfer");
    for (i, v2) in edited.iter().enumerate() {
        let received = std::fs::read(dst.path().join(format!("f{i}.bin"))).unwrap();
        assert_eq!(received.len(), v2.len());
        assert_eq!(received, *v2);
    }
}
#[tokio::test]
async fn checksum_push_skips_unchanged_files() {
    // Regression: `-c` on push used to transfer everything on every run,
    // because the receiver never hashed its own tree (the `--checksum` flag
    // was not forwarded to it), so the planner always saw a missing
    // destination hash and treated every file as an update.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("a.txt"), b"content a")
        .await
        .unwrap();
    tokio::fs::write(src.path().join("b.txt"), b"content b")
        .await
        .unwrap();

    let mut options = default_options();
    options.checksum = true;

    // The server (receiver) must hash its destination too — exactly what
    // `server_args` forwards over ssh in real deployments.
    let first = push_tree_with_server_args(src.path(), dst.path(), &options, &["--checksum"]).await;
    assert_eq!(first.files_sent, 2);

    // Second run: identical hashes on both sides → nothing to send.
    let second = push_tree_with_server_args(src.path(), dst.path(), &options, &["--checksum"]).await;
    assert_eq!(second.files_sent, 0, "checksum mode must skip identical files");
}
#[tokio::test]
async fn large_update_over_empty_basis_streams_chunks() {
    // Regression: a large update whose destination file is empty (an
    // interrupted first transfer, or a file that was truncated) has no useful
    // delta basis — it must fall back to the bounded chunked stream instead of
    // one whole-file in-memory `DeltaRecipe` frame (which would blow memory
    // and exceed the frame size limit for files over ~1 GiB).
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    // Destination already holds an empty file at the same path.
    tokio::fs::write(dst.path().join("big.bin"), b"").await.unwrap();

    // Above the delta tier (>10MB) so the update would normally delta.
    let data: Vec<u8> = pseudo_random(12 * 1024 * 1024);
    tokio::fs::write(src.path().join("big.bin"), &data)
        .await
        .unwrap();
    set_future_mtime(&src.path().join("big.bin"));

    let stats = push_tree(src.path(), dst.path(), &default_options()).await;
    assert_eq!(stats.files_sent, 1);
    assert_eq!(std::fs::read(dst.path().join("big.bin")).unwrap(), data);

    // And the destination is now a valid basis: a subsequent delta update
    // against it still works.
    let mut v2 = data.clone();
    v2.insert(6 * 1024 * 1024, 0xEE);
    tokio::fs::write(src.path().join("big.bin"), &v2)
        .await
        .unwrap();
    set_future_mtime(&src.path().join("big.bin"));
    push_tree(src.path(), dst.path(), &default_options()).await;
    assert_eq!(std::fs::read(dst.path().join("big.bin")).unwrap(), v2);
}
#[tokio::test]
async fn empty_dir_and_file_dir_replacement() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::create_dir(src.path().join("empty"))
        .await
        .unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;
    assert!(dst.path().join("empty").is_dir());

    // Replace a file with a directory of the same name.
    tokio::fs::write(src.path().join("swap"), b"i am a file")
        .await
        .unwrap();
    push_tree(src.path(), dst.path(), &default_options()).await;
    assert!(dst.path().join("swap").is_file());

    tokio::fs::remove_file(src.path().join("swap"))
        .await
        .unwrap();
    tokio::fs::create_dir(src.path().join("swap"))
        .await
        .unwrap();
    tokio::fs::write(src.path().join("swap/inner.txt"), b"now a dir")
        .await
        .unwrap();
    push_tree(src.path(), dst.path(), &default_options()).await;
    assert!(dst.path().join("swap").is_dir());
    assert_eq!(
        std::fs::read(dst.path().join("swap/inner.txt")).unwrap(),
        b"now a dir"
    );
}
#[tokio::test]
async fn delete_flag_removes_stale_files() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("keep.txt"), b"keep")
        .await
        .unwrap();
    push_tree(src.path(), dst.path(), &default_options()).await;

    // Destination has a file the source no longer has.
    tokio::fs::write(dst.path().join("stale.txt"), b"stale")
        .await
        .unwrap();

    let mut options = default_options();
    options.delete = true;
    push_tree_with_server_args(src.path(), dst.path(), &options, &["--delete"]).await;

    assert!(dst.path().join("keep.txt").exists());
    assert!(!dst.path().join("stale.txt").exists());
}
#[cfg(unix)]
#[tokio::test]
async fn unreadable_source_skipped_not_fatal() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("good.txt"), b"fine")
        .await
        .unwrap();
    tokio::fs::write(src.path().join("secret.bin"), vec![0u8; 64])
        .await
        .unwrap();
    // Make it unreadable (no-op for root; skipped by non-root runners).
    let _ = std::fs::set_permissions(
        src.path().join("secret.bin"),
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    );

    let stats = push_tree(src.path(), dst.path(), &default_options()).await;
    assert!(dst.path().join("good.txt").exists());
    // The unreadable file is either transferred (root) or skipped; either
    // way the sync completes without aborting.
    assert!(stats.files_sent >= 1);
}
#[tokio::test]
async fn long_path_skipped_not_fatal() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    let long_name = "y".repeat(240);
    tokio::fs::write(src.path().join(&long_name), vec![0xCD; 64])
        .await
        .unwrap();
    tokio::fs::write(src.path().join("ok.txt"), b"fine")
        .await
        .unwrap();

    let stats = push_tree(src.path(), dst.path(), &default_options()).await;
    assert!(dst.path().join("ok.txt").exists());
    // The over-long path is skipped, not fatal.
    assert_eq!(stats.skipped.len(), 1);
}
#[tokio::test]
async fn remote_path_scopes_the_transfer() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("a.txt"), b"scoped")
        .await
        .unwrap();

    let mut options = default_options();
    // Relative remote path: scoped under the serve root.
    options.remote_path = "backup".to_string();

    // Push targets root/backup, not the serve root itself.
    push_tree(src.path(), dst.path(), &options).await;
    assert_eq!(
        std::fs::read(dst.path().join("backup/a.txt")).unwrap(),
        b"scoped"
    );
    assert!(!dst.path().join("a.txt").exists());

    // Pull reads from root/backup. A no-slash remote path recreates the
    // directory under the destination (rsync `host:backup` semantics).
    pull_tree(dst.path(), restore.path(), &options).await;
    assert_eq!(
        std::fs::read(restore.path().join("backup/a.txt")).unwrap(),
        b"scoped"
    );
    assert!(!restore.path().join("a.txt").exists());
}
#[tokio::test]
async fn compressed_transfer_roundtrip() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    // Highly compressible content exercises the lz4 frame path.
    let data = vec![0x42u8; 512 * 1024];
    tokio::fs::write(src.path().join("zeros.bin"), &data)
        .await
        .unwrap();

    let mut options = default_options();
    options.compress = true;
    push_tree(src.path(), dst.path(), &options).await;
    assert_eq!(std::fs::read(dst.path().join("zeros.bin")).unwrap(), data);
}
#[cfg(unix)]
#[tokio::test]
async fn ignore_existing_preserves_dest_file_over_link() {
    use std::os::unix::fs::symlink;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("target.txt"), b"data")
        .await
        .unwrap();
    symlink("target.txt", src.path().join("l")).unwrap();
    // The destination already holds a *file* at the link's path.
    tokio::fs::write(dst.path().join("l"), b"precious user file")
        .await
        .unwrap();

    let mut options = default_options();
    options.ignore_existing = true;
    push_tree(src.path(), dst.path(), &options).await;

    // rsync `--ignore-existing`: the destination entry exists (even as a type
    // change, "file" vs "symlink") and must be left untouched.
    let meta = std::fs::symlink_metadata(dst.path().join("l")).unwrap();
    assert!(
        meta.file_type().is_file(),
        "the dest file must survive the push"
    );
    assert_eq!(
        std::fs::read(dst.path().join("l")).unwrap(),
        b"precious user file"
    );
}
#[tokio::test]
async fn push_multi_over_server_child() {
    // Glob-expanded sources: every match syncs as a top-level entry of one
    // run, including empty matched directories.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("a.txt"), b"aaa")
        .await
        .unwrap();
    tokio::fs::write(src.path().join("b.txt"), b"bbbb")
        .await
        .unwrap();
    tokio::fs::create_dir(src.path().join("empty")).await.unwrap();
    tokio::fs::create_dir(src.path().join("sub")).await.unwrap();
    tokio::fs::write(src.path().join("sub/c.txt"), b"c")
        .await
        .unwrap();

    let roots: Vec<std::path::PathBuf> = vec![
        src.path().join("a.txt"),
        src.path().join("b.txt"),
        src.path().join("empty"),
        src.path().join("sub"),
    ];
    let (mut child, send, recv) = spawn_server(dst.path());
    let mut executor = Executor::new(send, recv);
    let stats = executor
        .push_multi(src.path(), &roots, &ExecutorOptions::default())
        .await
        .expect("push_multi failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");

    // a.txt, b.txt, empty (dir), sub (dir), sub/c.txt.
    assert_eq!(stats.files_sent, 5);
    assert_eq!(std::fs::read(dst.path().join("a.txt")).unwrap(), b"aaa");
    assert_eq!(std::fs::read(dst.path().join("b.txt")).unwrap(), b"bbbb");
    assert!(dst.path().join("empty").is_dir());
    assert_eq!(std::fs::read(dst.path().join("sub/c.txt")).unwrap(), b"c");
}
#[tokio::test]
async fn push_multi_delete_removes_entries_outside_matches() {
    // One merged plan: `--delete` removes destination entries outside the
    // matched set (the behavior that rules out per-match sequential syncs).
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tokio::fs::write(dst.path().join("stale.txt"), b"s")
        .await
        .unwrap();
    tokio::fs::create_dir(dst.path().join("old")).await.unwrap();
    tokio::fs::write(dst.path().join("old/x.txt"), b"x")
        .await
        .unwrap();

    tokio::fs::write(src.path().join("a.txt"), b"aaa")
        .await
        .unwrap();
    let roots = vec![src.path().join("a.txt")];
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &["--delete"]);
    let mut executor = Executor::new(send, recv);
    let options = ExecutorOptions {
        delete: true,
        ..ExecutorOptions::default()
    };
    executor
        .push_multi(src.path(), &roots, &options)
        .await
        .expect("push_multi failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");

    assert_eq!(std::fs::read(dst.path().join("a.txt")).unwrap(), b"aaa");
    assert!(!dst.path().join("stale.txt").exists());
    assert!(!dst.path().join("old").exists());
}
#[tokio::test]
async fn absolute_remote_path_used_as_is() {
    // A leading `/` in the wire path is an absolute server path (rsync
    // semantics), not serve-root-relative — the server writes there directly.
    let src = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("a.txt"), b"abs")
        .await
        .unwrap();

    let mut options = default_options();
    options.remote_path = target.path().to_string_lossy().into_owned();

    // The serve root (cwd) is irrelevant for an absolute target.
    let (mut child, send, recv) = spawn_server(src.path());
    let mut executor = Executor::new(send, recv);
    executor
        .push(src.path(), &options)
        .await
        .expect("push failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");

    assert_eq!(std::fs::read(target.path().join("a.txt")).unwrap(), b"abs");
}
#[tokio::test]
async fn push_skips_and_updates_against_decoys() {
    // Without `--delete` the receiver probes the destination only for the
    // paths the source names; unrelated destination content must be left
    // exactly as it is, and the quick check must still work through the
    // targeted destination manifest (skip a same-size+mtime file, update a
    // changed one).
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("same.txt"), b"same content")
        .await
        .unwrap();
    tokio::fs::write(src.path().join("changed.txt"), b"changed source")
        .await
        .unwrap();
    let same_mtime = tokio::fs::metadata(src.path().join("same.txt"))
        .await
        .unwrap()
        .modified()
        .unwrap();

    // The destination already holds both files plus unrelated decoys.
    tokio::fs::write(dst.path().join("same.txt"), b"same content")
        .await
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(dst.path().join("same.txt"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(same_mtime))
        .unwrap();
    tokio::fs::write(dst.path().join("changed.txt"), b"old content")
        .await
        .unwrap();
    tokio::fs::write(dst.path().join("decoy.txt"), b"decoy")
        .await
        .unwrap();
    tokio::fs::create_dir(dst.path().join("decoy_dir")).await.unwrap();
    tokio::fs::write(dst.path().join("decoy_dir/deep.txt"), b"deep")
        .await
        .unwrap();

    let stats = push_tree(src.path(), dst.path(), &default_options()).await;

    // `same.txt` quick-check-skips; only `changed.txt` transfers.
    assert_eq!(stats.files_sent, 1);
    assert_eq!(
        std::fs::read(dst.path().join("same.txt")).unwrap(),
        b"same content"
    );
    assert_eq!(
        std::fs::read(dst.path().join("changed.txt")).unwrap(),
        b"changed source"
    );
    // Decoys are untouched without `--delete`.
    assert_eq!(std::fs::read(dst.path().join("decoy.txt")).unwrap(), b"decoy");
    assert_eq!(
        std::fs::read(dst.path().join("decoy_dir/deep.txt")).unwrap(),
        b"deep"
    );
}
#[tokio::test]
async fn pull_ignores_local_decoys_without_delete() {
    // The pull-side receiver runs the same targeted destination scan: local
    // files the remote source never names are preserved.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    tokio::fs::write(src.path().join("a.txt"), b"from server")
        .await
        .unwrap();
    tokio::fs::create_dir(src.path().join("sub")).await.unwrap();
    tokio::fs::write(src.path().join("sub/b.txt"), b"nested")
        .await
        .unwrap();

    tokio::fs::write(dst.path().join("decoy.txt"), b"local decoy")
        .await
        .unwrap();
    tokio::fs::create_dir(dst.path().join("local_dir")).await.unwrap();
    tokio::fs::write(dst.path().join("local_dir/x.txt"), b"x")
        .await
        .unwrap();

    let stats = pull_tree(src.path(), dst.path(), &default_options()).await;

    assert_eq!(stats.files_received, 3);
    assert_eq!(
        std::fs::read(dst.path().join("a.txt")).unwrap(),
        b"from server"
    );
    assert_eq!(
        std::fs::read(dst.path().join("sub/b.txt")).unwrap(),
        b"nested"
    );
    assert_eq!(
        std::fs::read(dst.path().join("decoy.txt")).unwrap(),
        b"local decoy"
    );
    assert_eq!(
        std::fs::read(dst.path().join("local_dir/x.txt")).unwrap(),
        b"x"
    );
}


#[tokio::test]
async fn remote_list_only_returns_the_tree() {
    // `--list-only` on a remote source: a ListRequest round-trip returns the
    // scanned entries (paths, sizes, kinds) without transferring.
    let serve = tempfile::tempdir().unwrap();
    tokio::fs::write(serve.path().join("a.txt"), b"aaa")
        .await
        .unwrap();
    tokio::fs::create_dir_all(serve.path().join("sub"))
        .await
        .unwrap();
    tokio::fs::write(serve.path().join("sub/b.txt"), b"bbb")
        .await
        .unwrap();

    let stats = list_tree(serve.path(), "", &default_options()).await;
    let paths: Vec<&str> = stats.changes.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"a.txt"), "listing must include a.txt: {paths:?}");
    assert!(
        paths.contains(&"sub/b.txt"),
        "listing must include sub/b.txt: {paths:?}"
    );
    let a = stats.changes.iter().find(|e| e.path == "a.txt").unwrap();
    assert_eq!(a.kind, 'f');
    assert_eq!(a.size, 3);
    let b = stats.changes.iter().find(|e| e.path == "sub/b.txt").unwrap();
    assert_eq!(b.kind, 'f');
    assert_eq!(b.size, 3);
}

#[tokio::test]
async fn remote_glob_pull_expands_server_side() {
    // A remote pull whose path has glob metacharacters is expanded on the
    // server and merged under the pattern's static prefix.
    let serve = tempfile::tempdir().unwrap();
    tokio::fs::create_dir_all(serve.path().join("sub"))
        .await
        .unwrap();
    tokio::fs::write(serve.path().join("sub/a.rs"), b"a")
        .await
        .unwrap();
    tokio::fs::write(serve.path().join("sub/b.rs"), b"bb")
        .await
        .unwrap();
    tokio::fs::write(serve.path().join("sub/c.txt"), b"ccc")
        .await
        .unwrap();

    let restore = tempfile::tempdir().unwrap();
    let mut options = default_options();
    options.remote_path = "sub/*.rs".to_string();
    let stats = pull_tree(serve.path(), restore.path(), &options).await;
    assert_eq!(stats.files_received, 2);
    assert_eq!(std::fs::read(restore.path().join("a.rs")).unwrap(), b"a");
    assert_eq!(std::fs::read(restore.path().join("b.rs")).unwrap(), b"bb");
    assert!(!restore.path().join("c.txt").exists());
}

#[tokio::test]
async fn remote_files_from_pull_merges_absolute_paths() {
    // Remote `--files-from`: the client sends absolute server paths; the
    // server merges them under the filesystem root, so each file mirrors
    // into the destination at its full absolute path (local files-from
    // layout).
    let serve = tempfile::tempdir().unwrap();
    let d1 = serve.path().join("d1");
    let d2 = serve.path().join("d2");
    tokio::fs::create_dir_all(&d1).await.unwrap();
    tokio::fs::create_dir_all(&d2).await.unwrap();
    tokio::fs::write(d1.join("a.txt"), b"a").await.unwrap();
    tokio::fs::write(d2.join("b.txt"), b"bb").await.unwrap();

    let restore = tempfile::tempdir().unwrap();
    let mut options = default_options();
    options.remote_paths = vec![
        d1.join("a.txt").to_string_lossy().into_owned(),
        d2.join("b.txt").to_string_lossy().into_owned(),
    ];
    let stats = pull_tree(serve.path(), restore.path(), &options).await;
    assert_eq!(stats.files_received, 2);
    // Mirrored at the absolute path minus the leading `/`.
    let abs1 = d1.join("a.txt");
    let abs2 = d2.join("b.txt");
    let rel1 = abs1.strip_prefix("/").unwrap();
    let rel2 = abs2.strip_prefix("/").unwrap();
    assert_eq!(
        std::fs::read(restore.path().join(rel1)).unwrap(),
        b"a",
        "{} must land at {}",
        d1.join("a.txt").display(),
        restore.path().join(rel1).display()
    );
    assert_eq!(
        std::fs::read(restore.path().join(rel2)).unwrap(),
        b"bb"
    );
}

#[tokio::test]
async fn multi_batch_push_verify_acks_every_batch() {
    // The sender flushes a `Batch` frame at every 128 MiB budget crossing,
    // so >128 MiB of small (batchable, <=2 MiB) files arrive as several
    // back-to-back Batch frames. Each frame's apply task must be joined
    // before the next frame's handle overwrites the slot — a lost earlier
    // batch drops its verification hashes, and `--verify` then reports
    // every file of the earlier batches as "no verification hash from
    // receiver". Flat prefix-free file names avoid the cross-file-sibling
    // pairing (which pulls files out of the batch); 1.9 MiB each makes the
    // budget cross mid-plan with files to spare.
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();

    for i in 0..200 {
        let data = pseudo_random(1_900_000 + i);
        tokio::fs::write(src.path().join(format!("file_{:02}{:02}.bin", i / 26, i % 26)), &data)
            .await
            .unwrap();
    }

    let mut options = default_options();
    options.verify = true;
    let stats = push_tree(src.path(), dst.path(), &options).await;
    assert_eq!(stats.files_sent, 200);
    assert!(
        stats.skipped.is_empty(),
        "every batch's verification hashes must reach the sender: {:?}",
        stats.skipped
    );

    // The no-trailing-slash source recreates its name under the
    // destination: find every .bin at any depth and check it arrived whole.
    let mut landed = 0;
    let mut stack = vec![dst.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "bin") {
                assert!(std::fs::metadata(&path).unwrap().len() >= 1_900_000);
                landed += 1;
            }
        }
    }
    assert_eq!(landed, 200);
}
