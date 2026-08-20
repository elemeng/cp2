//! Local-pipe end-to-end profile: fresh push + mid-file-edit push over a
//! spawned `cp2 --server` child with piped stdio — no ssh, so the wall time
//! isolates the application layer (scan / signature / frames / apply) from
//! the ssh and sshd layers. Compare against the ssh-based e2e timing: the
//! difference is the transport layer's cost.
//!
//! Usage: `cargo run --release --bin profile_push [--mb N]`

use std::process::Stdio;
use std::time::Instant;

use cp2::{Executor, ExecutorOptions};
use tokio::io::{AsyncRead, AsyncWrite};

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x = seed;
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push((x & 0xFF) as u8);
    }
    v
}

    async fn one_push(
        root: &std::path::Path,
        src: &std::path::Path,
    ) -> (f64, u64) {
        let mut child = tokio::process::Command::new(
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .join("cp2"),
        )
        .arg("--server")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let stderr_task = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = String::new();
            stderr.read_to_string(&mut buf).await.ok();
            if !buf.is_empty() {
                eprintln!("[server stderr] {}", buf.trim_end());
            }
        });

        let mut exec = Executor::new(
            Box::new(stdin) as Box<dyn AsyncWrite + Unpin + Send>,
            Box::new(stdout) as Box<dyn AsyncRead + Unpin + Send>,
        );
        let opts = ExecutorOptions {
            rollsum: true,
            ..ExecutorOptions::default()
        };

        let t = Instant::now();
        let stats = exec.push(src, &opts).await.unwrap();
        let dt = t.elapsed().as_secs_f64();
        drop(exec);
        child.kill().await.ok();
        let _ = stderr_task.await;
        (dt, stats.bytes_transferred)
    }


#[tokio::main]
async fn main() {
    let mb: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.trim_start_matches("--mb=").parse().ok())
        .unwrap_or(512);
    let len = mb * 1024 * 1024;

    let tag = std::process::id();
    let root = std::env::temp_dir().join(format!("cp2-profile-{tag}"));
    let src = std::env::temp_dir().join(format!("cp2-profile-src-{tag}"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&src);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&src).unwrap();
    let file = src.join("data.bin");
    std::fs::write(&file, pseudo_random(len, 0xDEAD_BEEF)).unwrap();

    let (dt, sent) = one_push(&root, &src).await;
    println!("local first: {dt:>6.3}s  ({sent} bytes)");

    // 10 MiB overwrite in the middle.
    let mut data = std::fs::read(&file).unwrap();
    let mid = len / 2;
    for b in &mut data[mid..mid + 10 * 1024 * 1024] {
        *b ^= 0x5A;
    }
    std::fs::write(&file, data).unwrap();

    let (dt, sent) = one_push(&root, &src).await;
    println!("local edit:  {dt:>6.3}s  ({sent} bytes)");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&src);
}
