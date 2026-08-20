//! Minimal CLI smoke tests: help, version, argument validation, local copy.

use std::process::Command;
use tempfile::TempDir;

fn cp2_command() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cp2"));
    cmd.env("RUST_LOG", "error");
    cmd
}

#[test]
fn test_help_flag() {
    let output = cp2_command().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("rsync-style file synchronization over SSH"));
    assert!(stdout.contains("Usage:"));
}

#[test]
fn test_version_flag() {
    let output = cp2_command().arg("--version").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Auto-deploy compares the build fingerprint in the banner; it must be
    // present and 16 hex digits.
    let banner = stdout.trim_end();
    assert!(
        banner.contains("(build ") && banner.ends_with(')'),
        "version banner must carry the build fingerprint, got: {stdout}"
    );
    let fp = banner
        .split("(build ")
        .nth(1)
        .unwrap_or_default()
        .trim_end_matches(')');
    assert_eq!(fp.len(), 16, "fingerprint must be 16 hex digits, got: {stdout}");
    assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn test_sync_requires_arguments() {
    let output = cp2_command().output().unwrap();
    assert!(!output.status.success());
}

#[test]
fn test_local_copy() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
    std::fs::create_dir(src.path().join("sub")).unwrap();
    std::fs::write(src.path().join("sub/b.txt"), b"world").unwrap();

    // A trailing slash on the source copies its contents into the
    // destination (`cp2 src/ DST/out` → `out/a.txt`), matching rsync.
    let output = cp2_command()
        .arg(format!("{}/", src.path().display()))
        .arg(dst.path().join("out"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        std::fs::read(dst.path().join("out/a.txt")).unwrap(),
        b"hello"
    );
    assert_eq!(
        std::fs::read(dst.path().join("out/sub/b.txt")).unwrap(),
        b"world"
    );
}

#[test]
fn test_local_copy_includes_dir_without_slash() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    std::fs::write(src.path().join("a.txt"), b"hello").unwrap();

    // No trailing slash: rsync recreates the source dir under the
    // destination (`cp2 src DST/out` → `out/<src-name>/a.txt`).
    let name = src
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let output = cp2_command()
        .arg(src.path())
        .arg(dst.path().join("out"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        std::fs::read(dst.path().join("out").join(&name).join("a.txt")).unwrap(),
        b"hello"
    );
    // The contents-only location must not also exist.
    assert!(!dst.path().join("out/a.txt").exists());
}

#[test]
fn test_local_copy_dry_run() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    std::fs::write(src.path().join("a.txt"), b"hello").unwrap();

    let output = cp2_command()
        .arg("--dry-run")
        .arg(src.path())
        .arg(dst.path().join("out"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!dst.path().join("out/a.txt").exists());
}
