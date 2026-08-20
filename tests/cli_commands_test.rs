use cp2::cli::Cli;

#[tokio::test]
async fn test_sync_command_dry_run() {
    let mut cli = Cli {
        rollsum: false,
        source: Some("./data".to_string()),
        destination: Some("user@127.0.0.1:backup".to_string()),
        server: false,
        port: None,
        jump_host: None,
        password: None,
        jump_password: None,
        sudo_password: None,
        remote_sudo: false,
        password_file: None,
        remote_path: None,
        binaries_dir: None,
        no_auto_install: true,
        verbose: 0,
        quiet: false,
        dry_run: true,
        delete: false,
        update: false,
        checksum: false,
        ignore_existing: false,
        existing: false,
        ignore_times: false,
        max_delete: None,
        backup: false,
        watch: None,
        watch_delay: 1000,
        jobs: Some(1),
        storage: cp2::platform::storage::StoragePreference::Auto,
        compress: false,
        bwlimit: None,
        exclude: vec![],
        include: vec![],
        fsync: false,
        remove_source_files: false,
        verify: false,
        archive: false,
        skip_links: false,
        follow_links: false,
        literal_links: false,
        literal_internal_links: false,
        literal_external_file_links: false,
        literal_external_dir_links: false,
        no_perms: false,
        sparse: false,
        xattrs: false,
        atimes: false,
        no_times: false,
        no_recursive: false,
        files_from: None,
    };
    let result = cp2::commands::sync::execute(&mut cli).await;
    assert!(result.is_ok());
}

#[test]
fn test_location_parse_local() {
    let loc = cp2::Location::parse("/tmp/foo");
    assert!(matches!(loc, cp2::Location::Local(_)));
}

#[test]
fn test_location_parse_remote() {
    let loc = cp2::Location::parse("user@127.0.0.1:backup");
    match loc {
        cp2::Location::Remote(r) => {
            assert_eq!(r.user, "user");
            assert_eq!(r.host, "127.0.0.1");
            assert_eq!(r.port, 22);
            assert_eq!(r.path, "backup");
        }
        cp2::Location::Local(_) => panic!("expected remote"),
    }
}

#[test]
fn test_location_parse_numeric_suffix_is_path() {
    // rsync semantics: ports come only from `--port`.
    let loc = cp2::Location::parse("user@127.0.0.1:4433");
    match loc {
        cp2::Location::Remote(r) => {
            assert_eq!(r.path, "4433");
            assert_eq!(r.port, 22);
        }
        cp2::Location::Local(_) => panic!("expected remote"),
    }
}

/// End-to-end glob source: run the real binary with a *quoted* pattern (the
/// shell would otherwise expand it into many positional args) and check the
/// destination layout. Local→local copy exercises expansion → `push_multi` →
/// server child → full protocol.
#[tokio::test]
async fn glob_source_expands_and_syncs_matches() {
    use std::process::Stdio;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("a.rs"), b"x").unwrap();
    std::fs::write(src.path().join("b.rs"), b"y").unwrap();
    std::fs::write(src.path().join("c.txt"), b"z").unwrap();

    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_cp2"))
        .arg("./*.rs")
        .arg(dst.path())
        .current_dir(src.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success(), "cp2 exited with {status}");

    assert!(dst.path().join("a.rs").is_file());
    assert!(dst.path().join("b.rs").is_file());
    assert!(!dst.path().join("c.txt").exists());
}

#[tokio::test]
async fn glob_source_matching_nothing_errors() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_cp2"))
        .arg("./*.nomatch")
        .arg(dst.path())
        .current_dir(src.path())
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no files match"), "{stderr}");
}

/// `--remove-source-files` on the real binary: local copy moves files off the
/// source, keeping directories.
#[tokio::test]
async fn remove_source_files_local_copy() {
    use std::process::Stdio;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("frame.fits"), b"data").unwrap();
    std::fs::create_dir(src.path().join("sub")).unwrap();
    std::fs::write(src.path().join("sub/other.fits"), b"more").unwrap();

    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_cp2"))
        .arg("--remove-source-files")
        .arg(src.path())
        .arg(dst.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success(), "cp2 exited with {status}");

    assert_eq!(std::fs::read(dst.path().join("frame.fits")).unwrap(), b"data");
    assert_eq!(
        std::fs::read(dst.path().join("sub/other.fits")).unwrap(),
        b"more"
    );
    assert!(!src.path().join("frame.fits").exists());
    assert!(!src.path().join("sub/other.fits").exists());
    assert!(src.path().join("sub").is_dir());
}

/// `--verify` on the real binary: local copy verifies byte-identity without
/// deleting the source.
#[tokio::test]
async fn verify_flag_local_copy() {
    use std::process::Stdio;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("frame.fits"), b"payload").unwrap();

    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_cp2"))
        .arg("--verify")
        .arg(src.path())
        .arg(dst.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success(), "cp2 exited with {status}");

    assert_eq!(
        std::fs::read(dst.path().join("frame.fits")).unwrap(),
        b"payload"
    );
    assert!(src.path().join("frame.fits").exists());
}

/// `--files-from` on the real binary: entries are absolute paths and each
/// syncs to the destination mirroring its root-relative structure.
#[tokio::test]
async fn files_from_flag_local_copy() {
    use std::process::Stdio;
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("a.txt"), b"a").unwrap();
    std::fs::create_dir(src.path().join("sub")).unwrap();
    std::fs::write(src.path().join("sub/b.txt"), b"b").unwrap();
    std::fs::write(src.path().join("skip.txt"), b"skip").unwrap();
    let list = src.path().join("list.txt");
    std::fs::write(
        &list,
        format!("{}\n{}\n", src.path().join("a.txt").display(), src.path().join("sub").display()),
    )
    .unwrap();

    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_cp2"))
        .arg("--files-from")
        .arg(&list)
        .arg(dst.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success(), "cp2 exited with {status}");

    // The root-relative structure is mirrored under the destination.
    let mirror = dst.path().join(src.path().strip_prefix("/").unwrap());
    assert_eq!(std::fs::read(mirror.join("a.txt")).unwrap(), b"a");
    assert_eq!(std::fs::read(mirror.join("sub/b.txt")).unwrap(), b"b");
    assert!(!mirror.join("skip.txt").exists());
}

/// A relative entry in the list is rejected, not silently reinterpreted.
#[tokio::test]
async fn files_from_rejects_relative_entries() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("a.txt"), b"a").unwrap();
    let list = src.path().join("list.txt");
    std::fs::write(&list, "books/science.md\n").unwrap();

    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_cp2"))
        .arg("--files-from")
        .arg(&list)
        .arg(dst.path())
        .current_dir(src.path())
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("must be absolute paths"), "{stderr}");
}

