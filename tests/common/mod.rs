//! Shared helpers for the end-to-end integration tests (each e2e_*.rs
#![allow(dead_code)]  // shared helpers are only used by a subset of the e2e_* crates
//! runs the full sync protocol between an in-process client executor and a
//! spawned `cp2 --server` child over piped stdio — no sshd needed).

pub use cp2::{Executor, ExecutorOptions, SyncStats};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};

pub fn server_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cp2")
}

pub fn spawn_server(
    serve_root: &Path,
) -> (
    Child,
    Box<dyn AsyncWrite + Unpin + Send>,
    Box<dyn AsyncRead + Unpin + Send>,
) {
    spawn_server_with_args(serve_root, &[])
}

pub fn spawn_server_with_args(
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

pub async fn push_tree_with_server_args(
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

pub async fn push_tree(src: &Path, dst: &Path, options: &ExecutorOptions) -> SyncStats {
    let (mut child, send, recv) = spawn_server(dst);
    let mut executor = Executor::new(send, recv);
    let stats = executor.push(src, options).await.expect("push failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");
    stats
}

pub async fn pull_tree(serve_root: &Path, dst: &Path, options: &ExecutorOptions) -> SyncStats {
    let (mut child, send, recv) = spawn_server(serve_root);
    let mut executor = Executor::new(send, recv);
    let stats = executor.pull(dst, options).await.expect("pull failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");
    stats
}

/// Run a remote `--list-only` session against a spawned server child rooted
/// at `serve_root`, listing `path` (serve-root-relative or absolute).
pub async fn list_tree(serve_root: &Path, path: &str, options: &ExecutorOptions) -> SyncStats {
    let (mut child, send, recv) = spawn_server(serve_root);
    let mut executor = Executor::new(send, recv);
    let stats = executor.list(path, options).await.expect("list failed");
    drop(executor);
    let exit_status = child.wait().await.expect("wait server");
    assert!(exit_status.success(), "server exited with {exit_status}");
    stats
}

pub fn default_options() -> ExecutorOptions {
    ExecutorOptions::default()
}

pub fn pseudo_random(len: usize) -> Vec<u8> {
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

pub fn set_future_mtime(path: &Path) {
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

pub fn set_link_mtime(path: &Path, secs: i64) {
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

pub async fn wait_until(mut cond: impl FnMut() -> bool, timeout: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("condition not met within {timeout:?}");
}

pub fn set_file_mtime_ns(path: &Path, sec: i64, nsec: i64) {
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
