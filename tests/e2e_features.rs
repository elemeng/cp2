mod common;
use common::*;

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

#[tokio::test]
async fn rollsum_pull_roundtrips() {
    // The pull sender is the server: it must chunk-roll only when its own
    // argv carries --rollsum (the flag is not in the PullRequest frame —
    // both peers derive it from the same CLI).
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    let data = pseudo_random(4 * 1024 * 1024);
    tokio::fs::write(serve.path().join("data.bin"), &data)
        .await
        .unwrap();

    let mut options = default_options();
    options.rollsum = true;
    let (mut child, send, recv) = spawn_server_with_args(serve.path(), &["--rollsum"]);
    let mut executor = Executor::new(send, recv);
    executor
        .pull(restore.path(), &options)
        .await
        .expect("pull failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");

    assert_eq!(
        std::fs::read(restore.path().join("data.bin")).unwrap(),
        data,
        "rollsum pull must reconstruct byte-exactly"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn xattrs_on_pull_roundtrip() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    // The pull *sender* is the server: -X must ride its argv so the xattrs
    // are collected and sent; the client receiver applies them.
    let serve = tempfile::tempdir().unwrap();
    let restore = tempfile::tempdir().unwrap();
    let file = serve.path().join("a.txt");
    tokio::fs::write(&file, b"x").await.unwrap();
    let cpath = CString::new(file.as_os_str().as_bytes()).unwrap();
    let name = CString::new("user.cp2_pull").unwrap();
    assert_eq!(
        unsafe {
            libc::setxattr(
                cpath.as_ptr(),
                name.as_ptr(),
                b"pulled value".as_ptr().cast(),
                12,
                0,
            )
        },
        0,
        "set the source xattr"
    );

    let mut options = default_options();
    options.xattrs = true;
    let (mut child, send, recv) = spawn_server_with_args(serve.path(), &["--xattrs"]);
    let mut executor = Executor::new(send, recv);
    executor
        .pull(restore.path(), &options)
        .await
        .expect("pull failed");
    drop(executor);
    let _ = child.wait().await;

    let dpath = CString::new(restore.path().join("a.txt").as_os_str().as_bytes()).unwrap();
    let mut buf = vec![0u8; 64];
    let n = unsafe {
        libc::getxattr(dpath.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
    };
    assert!(n > 0, "the xattr must be present after the pull");
    buf.truncate(usize::try_from(n).unwrap_or(0) as usize);
    assert_eq!(buf, b"pulled value");
}

