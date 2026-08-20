//! End-to-end tests: run the full sync protocol between an in-process client
//! executor and a spawned `cp2 --server` child over piped stdio (no sshd
//! needed). This exercises the protocol + delta stack end to end; the ssh
//! wrapper itself is a thin spawn and is covered manually against a real
//! sshd.

use cp2::{Executor, ExecutorOptions, SyncStats};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};

/// Path to the compiled `cp2` binary (set by cargo for integration tests).
fn server_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cp2")
}

/// Spawn `cp2 --server` rooted at `serve_root`, returning the child and the
/// boxed stream halves the client executor talks over.
fn spawn_server(
    serve_root: &Path,
) -> (
    Child,
    Box<dyn AsyncWrite + Unpin + Send>,
    Box<dyn AsyncRead + Unpin + Send>,
) {
    spawn_server_with_args(serve_root, &[])
}

/// Like [`spawn_server`] with extra server argv (e.g. `--backup`,
/// `--max-delete 0`) — receiver-side flags the client forwards over ssh in
/// real deployments.
fn spawn_server_with_args(
    serve_root: &Path,
    args: &[&str],
) -> (
    Child,
    Box<dyn AsyncWrite + Unpin + Send>,
    Box<dyn AsyncRead + Unpin + Send>,
) {
    let mut cmd = Command::new(server_bin());
    cmd.arg("--server").args(args).current_dir(serve_root);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn cp2 --server");
    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    (child, Box::new(stdin), Box::new(stdout))
}

/// Push `src` to a server rooted at `dst` with extra server args.
async fn push_tree_with_server_args(
    src: &Path,
    dst: &Path,
    options: &ExecutorOptions,
    server_args: &[&str],
) -> SyncStats {
    let (mut child, send, recv) = spawn_server_with_args(dst, server_args);
    let mut executor = Executor::new(send, recv);
    let stats = executor.push(src, options).await.expect("push failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");
    stats
}

/// Push `src` to a server rooted at `dst`; wait for the child to exit.
async fn push_tree(src: &Path, dst: &Path, options: &ExecutorOptions) -> SyncStats {
    let (mut child, send, recv) = spawn_server(dst);
    let mut executor = Executor::new(send, recv);
    let stats = executor.push(src, options).await.expect("push failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");
    stats
}

/// Pull from a server rooted at `serve_root` into `dst`.
async fn pull_tree(serve_root: &Path, dst: &Path, options: &ExecutorOptions) -> SyncStats {
    let (mut child, send, recv) = spawn_server(serve_root);
    let mut executor = Executor::new(send, recv);
    let stats = executor.pull(dst, options).await.expect("pull failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");
    stats
}

fn default_options() -> ExecutorOptions {
    ExecutorOptions::default()
}

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

    // Pull reads from root/backup.
    pull_tree(dst.path(), restore.path(), &options).await;
    assert_eq!(
        std::fs::read(restore.path().join("a.txt")).unwrap(),
        b"scoped"
    );
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

/// Deterministic pseudo-random bytes (LCG) — CDC-friendly, unlike highly
/// periodic content which degenerates to a single chunk.
fn pseudo_random(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut state = 0x1234_5678u64;
    for _ in 0..len {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // Keep the low byte of the LCG output's upper bits (intentional
        // truncation of pseudo-random data).
        #[expect(clippy::cast_possible_truncation)]
        let byte = (state >> 33) as u8;
        data.push(byte);
    }
    data
}

/// Set a file's mtime a few seconds in the future (avoids same-second
/// collision with the previously written version).
fn set_future_mtime(path: &Path) {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 5;
    let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t);
    let _ = std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_times(std::fs::FileTimes::new().set_modified(time)));
}

/// Set a symlink's *own* mtime to a whole-second value, without touching the
/// link target (`utimensat(AT_SYMLINK_NOFOLLOW)`).
#[cfg(unix)]
fn set_link_mtime(path: &Path, secs: i64) {
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL in path");
    let ts = libc::timespec {
        tv_sec: secs,
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
    assert_eq!(rc, 0, "set link mtime");
}

#[tokio::test]
async fn watch_pull_syncs_remote_changes() {
    use std::time::Duration;

    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    tokio::fs::write(serve.path().join("a.txt"), b"aaa")
        .await
        .unwrap();

    // Server-driven watch-pull: the server watches its own tree and drives
    // incremental cycles over the persistent session.
    let (mut child, send, recv) = spawn_server(serve.path());
    let mut executor = Executor::new(send, recv);
    let restore_path = restore.path().to_path_buf();
    let watch = tokio::spawn(async move {
        executor
            .pull_watch(
                &restore_path,
                &default_options(),
                Duration::from_millis(100),
            )
            .await
    });

    // The initial cycle runs immediately on connect.
    wait_until(
        || restore.path().join("a.txt").exists(),
        Duration::from_secs(10),
    )
    .await;

    // A change on the server's side is picked up by its watcher and pulled.
    tokio::fs::write(serve.path().join("b.txt"), b"bbb")
        .await
        .unwrap();
    wait_until(
        || restore.path().join("b.txt").exists(),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(std::fs::read(restore.path().join("b.txt")).unwrap(), b"bbb");

    // End the session: kill the server; the client sees EOF and errors out.
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = watch.await;
}

/// Poll `cond` until it holds or `timeout` elapses (watch tests are
/// event-driven end to end, so give notify + protocol slack).
async fn wait_until(mut cond: impl FnMut() -> bool, timeout: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("condition not met within {timeout:?}");
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

#[cfg(unix)]
#[tokio::test]
async fn no_times_leaves_transfer_time_and_uses_size_only_check() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let file = src.path().join("a.txt");
    tokio::fs::write(&file, b"payload").await.unwrap();
    // Pin the source mtime to a far-past value.
    let c = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
    let past = libc::timespec { tv_sec: 946_684_800, tv_nsec: 0 }; // 2000-01-01
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [past, past].as_ptr(), 0) },
        0
    );

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
        dst_meta.mtime(), src_meta.mtime(),
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

/// Set a regular file's mtime to `sec` seconds plus `nsec` nanoseconds.
#[cfg(unix)]
fn set_file_mtime_ns(path: &Path, sec: i64, nsec: i64) {
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL in path");
    let ts = libc::timespec {
        tv_sec: sec,
        tv_nsec: nsec,
    };
    let times = [ts, ts];
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) };
    assert_eq!(rc, 0, "set file mtime");
}

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

#[tokio::test]
async fn single_file_source_syncs() {
    // A single file as the source syncs to the destination root (regression:
    // the sender used to resolve the entry against the file path itself,
    // producing `file/name` → ENOTDIR).
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let file = src.path().join("single.txt");
    tokio::fs::write(&file, b"hello world").await.unwrap();

    let (mut child, send, recv) = spawn_server(dst.path());
    let mut executor = Executor::new(send, recv);
    executor
        .push(&file, &default_options())
        .await
        .expect("push failed");
    drop(executor);
    assert!(child.wait().await.unwrap().success());

    assert_eq!(
        std::fs::read(dst.path().join("single.txt")).unwrap(),
        b"hello world"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sparse_push_keeps_holes() {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    // A VM-image-shaped file: head, a 1 MiB hole, tail (2 blocks allocated,
    // 1 MiB logical).
    let mut f = std::fs::File::create(src.path().join("img.bin")).unwrap();
    f.write_all(b"head").unwrap();
    f.seek(SeekFrom::Start(1_048_576 - 4)).unwrap();
    f.write_all(b"tail").unwrap();
    f.sync_all().unwrap();
    let source_meta = std::fs::metadata(src.path().join("img.bin")).unwrap();
    assert!(source_meta.blocks() * 512 < 1_048_576 / 4, "the source must be sparse");

    let mut options = default_options();
    options.sparse = true;
    push_tree_with_server_args(src.path(), dst.path(), &options, &["--sparse"]).await;

    let dest_path = dst.path().join("img.bin");
    let dest_meta = std::fs::metadata(&dest_path).unwrap();
    assert_eq!(
        dest_meta.len(),
        source_meta.len(),
        "the logical size must be exact"
    );
    assert!(
        dest_meta.blocks() * 512 < 1_048_576 / 4,
        "--sparse must keep the hole ({:?} blocks allocated)",
        dest_meta.blocks()
    );
    let content = std::fs::read(&dest_path).unwrap();
    assert_eq!(&content[..4], b"head");
    assert_eq!(&content[content.len() - 4..], b"tail");
}

#[cfg(unix)]
#[tokio::test]
async fn sparse_push_without_flag_allocates() {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let mut f = std::fs::File::create(src.path().join("img.bin")).unwrap();
    f.write_all(b"head").unwrap();
    f.seek(SeekFrom::Start(1_048_576 - 4)).unwrap();
    f.write_all(b"tail").unwrap();

    // Without `-S` the default path preallocates: the same file is fully
    // allocated on the destination.
    push_tree(src.path(), dst.path(), &default_options()).await;
    let dest_meta = std::fs::metadata(dst.path().join("img.bin")).unwrap();
    assert!(
        dest_meta.blocks() * 512 >= 1_048_576 / 2,
        "without --sparse the file must be fully allocated ({:?} blocks)",
        dest_meta.blocks()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn xattr_preserved_with_flag() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let file = src.path().join("a.txt");
    tokio::fs::write(&file, b"x").await.unwrap();
    let cpath = CString::new(file.as_os_str().as_bytes()).unwrap();
    let name = CString::new("user.cp2_test").unwrap();
    assert_eq!(
        unsafe {
            libc::setxattr(
                cpath.as_ptr(),
                name.as_ptr(),
                b"hello xattr".as_ptr().cast(),
                11,
                0,
            )
        },
        0,
        "set the source xattr"
    );

    // Without `-X` nothing travels.
    push_tree(src.path(), dst.path(), &default_options()).await;
    let dpath = CString::new(dst.path().join("a.txt").as_os_str().as_bytes()).unwrap();
    assert_eq!(
        unsafe { libc::getxattr(dpath.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) },
        -1,
        "xattrs are not copied without -X"
    );

    // With `-X` the value round-trips. Change the content first (different
    // size, so the quick check cannot skip — an in-sync file would skip,
    // xattrs included).
    tokio::fs::write(&file, b"yz").await.unwrap();
    let mut options = default_options();
    options.xattrs = true;
    push_tree_with_server_args(src.path(), dst.path(), &options, &["--xattrs"]).await;
    let mut buf = vec![0u8; 64];
    let n = unsafe {
        libc::getxattr(dpath.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
    };
    assert!(n > 0, "the xattr must be present on the destination");
    buf.truncate(usize::try_from(n).unwrap_or(0) as usize);
    assert_eq!(buf, b"hello xattr");
}

#[cfg(unix)]
#[tokio::test]
async fn atime_restored_with_flag() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let file = src.path().join("a.txt");
    tokio::fs::write(&file, b"x").await.unwrap();
    // A distinctive atime, with the current mtime untouched.
    let cpath = CString::new(file.as_os_str().as_bytes()).unwrap();
    let mtime = std::fs::metadata(&file).unwrap().mtime();
    let times = [
        libc::timespec {
            tv_sec: 1_500_000_000,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: mtime,
            tv_nsec: 0,
        },
    ];
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) },
        0
    );

    // Without `-U` the receiver keeps its own atime (UTIME_OMIT) — it is
    // "now", not the source's 1_500_000_000.
    push_tree(src.path(), dst.path(), &default_options()).await;
    let dest_atime = std::fs::metadata(dst.path().join("a.txt"))
        .unwrap()
        .atime();
    assert_ne!(
        dest_atime, 1_500_000_000,
        "without -U the destination atime must stay the receiver's own"
    );

    // With `-U` the captured source atime is restored. Change the content
    // (different size, so the quick check cannot skip) and re-set the
    // distinctive atime on the new content.
    tokio::fs::write(&file, b"yz").await.unwrap();
    let mtime = std::fs::metadata(&file).unwrap().mtime();
    let times = [
        libc::timespec {
            tv_sec: 1_500_000_000,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: mtime,
            tv_nsec: 0,
        },
    ];
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) },
        0
    );
    let mut options = default_options();
    options.atimes = true;
    push_tree_with_server_args(src.path(), dst.path(), &options, &["--atimes"]).await;
    let dest_atime = std::fs::metadata(dst.path().join("a.txt"))
        .unwrap()
        .atime();
    assert_eq!(
        dest_atime, 1_500_000_000,
        "-U must restore the source atime"
    );
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

/// Sibling files ("1.iso" / "1.1.iso") pair up: the reference crosses the
/// wire as full content, the dependent is reconstructed by a cross-file
/// delta against it — both must be byte-identical afterwards.
#[tokio::test]
async fn push_sibling_files_use_cross_file_delta() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let one = src.path().join("1.iso");
    let one_one = src.path().join("1.1.iso");
    // Deterministic 8 MiB content; the sibling differs in one 256 KiB
    // region (the "few small files inside the image" pattern).
    let base: Vec<u8> = (0..8 * 1024 * 1024u64)
        .map(|i| ((i.wrapping_mul(7) ^ 0x5A) & 0xFF) as u8)
        .collect();
    std::fs::write(&one, &base).unwrap();
    let mut sibling = base.clone();
    for b in &mut sibling[4 * 1024 * 1024..4 * 1024 * 1024 + 256 * 1024] {
        *b ^= 0xFF;
    }
    std::fs::write(&one_one, &sibling).unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;

    assert_eq!(
        std::fs::read(dst.path().join("1.iso")).unwrap(),
        base,
        "reference must be byte-identical"
    );
    assert_eq!(
        std::fs::read(dst.path().join("1.1.iso")).unwrap(),
        sibling,
        "dependent must be reconstructed byte-identically"
    );
}

/// A chain where the middle file is *both* a cross-file dependent (its own
/// basis is the first file) and a reference (the third file's basis): the
/// sender must feed the middle file's dependents with a spawned signature
/// job — dropping its channels would abort the whole sync with
/// "cross-file basis signature lost" (regression: the benchmark's f1/f100/
/// f1000 tree).
#[tokio::test]
async fn push_chained_cross_file_bases() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let a = src.path().join("1.iso");
    let b = src.path().join("1.1.iso");
    let c = src.path().join("1.1.1.iso");
    // Deterministic 8 MiB content; each level differs from the previous in
    // one 256 KiB region, so every cross-file delta matches most blocks.
    let base: Vec<u8> = (0..8 * 1024 * 1024u64)
        .map(|i| ((i.wrapping_mul(7) ^ 0x5A) & 0xFF) as u8)
        .collect();
    let mut mid = base.clone();
    for b_ in &mut mid[2 * 1024 * 1024..2 * 1024 * 1024 + 256 * 1024] {
        *b_ ^= 0xFF;
    }
    let mut end = mid.clone();
    for b_ in &mut end[5 * 1024 * 1024..5 * 1024 * 1024 + 256 * 1024] {
        *b_ ^= 0xFF;
    }
    std::fs::write(&a, &base).unwrap();
    std::fs::write(&b, &mid).unwrap();
    std::fs::write(&c, &end).unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;

    assert_eq!(std::fs::read(dst.path().join("1.iso")).unwrap(), base);
    assert_eq!(std::fs::read(dst.path().join("1.1.iso")).unwrap(), mid);
    assert_eq!(
        std::fs::read(dst.path().join("1.1.1.iso")).unwrap(),
        end,
        "the chained dependent must not abort the sync"
    );
}

/// A dissimilar pair (matching heuristic fires, but the delta is mostly
/// literal) must fall back to a whole-file transfer — still correct.
#[tokio::test]
async fn push_dissimilar_siblings_fall_back_to_whole_file() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let one = src.path().join("disk.img");
    let one_one = src.path().join("disk.1.img");
    let a: Vec<u8> = (0..2 * 1024 * 1024u64)
        .map(|i| (i.wrapping_mul(3) & 0xFF) as u8)
        .collect();
    let b: Vec<u8> = (0..2 * 1024 * 1024u64)
        .map(|i| ((i.wrapping_mul(11) ^ 0x3C) & 0xFF) as u8)
        .collect();
    std::fs::write(&one, &a).unwrap();
    std::fs::write(&one_one, &b).unwrap();

    push_tree(src.path(), dst.path(), &default_options()).await;

    assert_eq!(std::fs::read(dst.path().join("disk.img")).unwrap(), a);
    assert_eq!(std::fs::read(dst.path().join("disk.1.img")).unwrap(), b);
}

/// The rollsum engine end-to-end: a changed file with an insertion and an
/// overwrite is delta-transferred against the fixed-block basis and
/// reconstructed byte-exactly. Both peers run with the flag.
#[tokio::test]
async fn push_rollsum_engine_roundtrip() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let file = src.path().join("data.bin");
    let mut data: Vec<u8> = (0..4 * 1024 * 1024u64)
        .map(|i| ((i.wrapping_mul(7) ^ 0x5A) & 0xFF) as u8)
        .collect();
    std::fs::write(&file, &data).unwrap();
    push_tree(src.path(), dst.path(), &default_options()).await;

    // Edit: overwrite 64 KiB and insert 4 KiB.
    for b in &mut data[1024 * 1024..1024 * 1024 + 65536] {
        *b ^= 0xFF;
    }
    data.splice(2 * 1024 * 1024..2 * 1024 * 1024, vec![0xEE; 4096]);
    std::fs::write(&file, &data).unwrap();

    let mut options = default_options();
    options.rollsum = true;
    let server_args = vec!["--rollsum"];
    let (mut child, send, recv) = spawn_server_with_args(dst.path(), &server_args);
    let mut executor = Executor::new(send, recv);
    let _ = executor.push(src.path(), &options).await.expect("push failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");

    assert_eq!(
        std::fs::read(dst.path().join("data.bin")).unwrap(),
        data,
        "rollsum engine must reconstruct byte-exactly"
    );
}
