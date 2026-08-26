//! `cp2 --server`: the sshd-invoked server mode (rsync's `--server`).
//!
//! Reads the sync protocol from stdin and writes to stdout. The serve root
//! is the current working directory; pull paths are sanitized under it.
//! Authentication and permissions were already handled by sshd before this
//! process started.

use crate::protocol::Frame;
use crate::protocol::stream;
use crate::sync::{Executor, ExecutorOptions};
use anyhow::Result;
use std::path::PathBuf;

/// Serve one sync session over stdin/stdout.
///
/// # Errors
///
/// Returns an error if the protocol session fails; the peer is notified with
/// a `Frame::Error` before the error is returned.
pub async fn execute(options: &ExecutorOptions) -> Result<()> {
    // Human output must never hold the protocol hostage: sshd wired our
    // stderr to a pipe, and a stalled forward — a client that stops reading
    // it, a wedged ControlMaster mux — can fill that pipe and block a
    // stderr write mid-transfer. The serve loop then hangs even though the
    // data flowed (the observed ControlMaster deadlock: the server was
    // blocked writing its end-of-run summary while the client waited for
    // the session to finish). fd 2 is made non-blocking so diagnostics are
    // best-effort: dropped under backpressure instead of stalling the sync.
    #[cfg(unix)]
    make_stderr_non_blocking();

    // The protocol rides stdin/stdout, which sshd wired as pipes with the
    // default 64 KiB capacity. Enlarge them from this end (the pipe is shared,
    // so setting the size here covers both ends): a 64 KiB capacity would add
    // a wakeup round trip every 64 KiB (see `platform::fs::enlarge_pipe`).
    crate::platform::fs::enlarge_pipe(&std::io::stdin());
    crate::platform::fs::enlarge_pipe(&std::io::stdout());

    // Unix: raw-fd epoll adapters — the bulk protocol path reads and writes
    // the pipes directly into the caller's buffers, matching the client-side
    // transport model (`ChildStdout`). `tokio::io::stdin()/stdout()` route
    // every call through the `Blocking` wrapper's internal buffer (a
    // full-payload copy plus a blocking-pool dispatch per call), which the
    // chunked path pays per frame; on real Linux (cheap epoll wakeups) the
    // direct path measures faster. Elsewhere the tokio stdio handles stand
    // in.
    #[cfg(unix)]
    let (send, recv) = (
        match fdio::FdWrite::stdout() {
            Ok(w) => Box::new(w) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
            Err(_) => Box::new(tokio::io::stdout()),
        },
        match fdio::FdRead::stdin() {
            Ok(r) => Box::new(r) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
            Err(_) => Box::new(tokio::io::stdin()),
        },
    );
    #[cfg(not(unix))]
    let (send, recv) = (
        Box::new(tokio::io::stdout()) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
        Box::new(tokio::io::stdin()) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    );
    let root = PathBuf::from(".");

    let mut executor = Executor::new(send, recv);
    let stats = executor.serve(&root, options).await;
    // Release the stream handles before writing the error frame on a fresh
    // stdout handle.
    drop(executor);
    let stats = match stats {
        Ok(stats) => stats,
        Err(e) => {
            // Best-effort: tell the peer why we are aborting before the
            // stream closes — the peer's `from_peer` surfaces `Frame::Error`
            // as a real error instead of a bare EOF.
            let mut out = tokio::io::stdout();
            let _ = stream::send_frame(
                &mut out,
                &Frame::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return Err(e.into());
        }
    };

    // stdout is the protocol channel — human output goes to stderr. fd 2 is
    // non-blocking (see `make_stderr_non_blocking`), so the write is
    // error-ignoring by hand: std's `eprintln!` panics on write failure
    // (EAGAIN included), which would turn backpressure into a crash. A
    // dropped summary only loses the line — the protocol frames already
    // went out.
    if !options.quiet {
        use std::io::Write;
        let line = format!(
            "Synced {} files, {} bytes",
            stats.files_received + stats.files_sent,
            stats.bytes_transferred
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
        let _ = std::io::stderr().write_all(b"\n");
    }
    Ok(())
}

/// Best-effort diagnostics on the serve path: clear the blocking flag on
/// fd 2 so a stderr write can never stall the protocol (see `execute`).
/// Only stderr changes — stdin/stdout carry the protocol and stay blocking.
#[cfg(unix)]
fn make_stderr_non_blocking() {
    // SAFETY: F_GETFL/F_SETFL on our own stderr descriptor; the flags are
    // only re-applied to the same descriptor with the non-blocking bit set.
    let flags = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_GETFL) };
    if flags >= 0 {
        unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
}

/// Raw-fd epoll adapters for the server's stdio (Unix).
///
/// The default `tokio::io::stdin()/stdout()` route every call through the
/// `Blocking` wrapper, which reads into an internal `Vec` buffer and copies
/// into the caller's buffer (plus a blocking-pool dispatch per call). The
/// bulk chunked path pays that per frame. These adapters instead register
/// duplicated, non-blocking copies of fds 0/1 with the reactor and read and
/// write straight into the caller's buffers — the same transport model the
/// client side already enjoys (`ChildStdout`/`ChildStdin`). On WSL2 the
/// benefit is drowned in the high per-wakeup latency; on real Linux the
/// removed copy and dispatch measure.
///
/// Windows: the server falls back to `tokio::io::stdin()/stdout()` (the
/// russh transport does not use child stdio pipes).
#[cfg(unix)]
mod fdio {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::unix::AsyncFd;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    /// A duplicate of `fd`, marked non-blocking (the reactor requires it;
    /// the original fd is untouched).
    fn dup_nonblocking(fd: RawFd) -> io::Result<OwnedFd> {
        // SAFETY: `dup` takes a valid fd and returns a fresh one we own.
        let duped = unsafe { libc::dup(fd) };
        if duped < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `duped` is exclusively ours.
        let owned = unsafe { OwnedFd::from_raw_fd(duped) };
        // SAFETY: fcntl on our own fd.
        let flags = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            // SAFETY: fcntl on our own fd.
            || unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(owned)
    }

    /// Epoll-registered read end of the protocol pipe: reads go directly
    /// into the caller's buffer (zero-copy).
    pub(crate) struct FdRead {
        fd: AsyncFd<OwnedFd>,
    }

    impl FdRead {
        pub(crate) fn stdin() -> io::Result<Self> {
            Ok(Self {
                fd: AsyncFd::new(dup_nonblocking(0)?)?,
            })
        }
    }

    impl AsyncRead for FdRead {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            loop {
                let mut guard = std::task::ready!(self.fd.poll_read_ready(cx))?;
                if let Ok(Ok(n)) = guard.try_io(|fd| {
                    // SAFETY: read(2) writes initialized bytes into the
                    // unfilled region; `advance` marks them readable.
                    let n = unsafe {
                        libc::read(
                            fd.as_raw_fd(),
                            buf.unfilled_mut().as_mut_ptr().cast(),
                            buf.remaining(),
                        )
                    };
                    if n < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(usize::try_from(n).map_err(|_| io::Error::other("read length overflow"))?)
                    }
                }) {
                    if n == 0 {
                        return Poll::Ready(Ok(())); // EOF
                    }
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                // WouldBlock (or the readiness was consumed): the loop
                // re-arms via poll_read_ready.
            }
        }
    }

    /// Epoll-registered write end of the protocol pipe: writes go directly
    /// from the caller's buffer.
    pub(crate) struct FdWrite {
        fd: AsyncFd<OwnedFd>,
    }

    impl FdWrite {
        pub(crate) fn stdout() -> io::Result<Self> {
            Ok(Self {
                fd: AsyncFd::new(dup_nonblocking(1)?)?,
            })
        }
    }

    impl AsyncWrite for FdWrite {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            loop {
                let mut guard = std::task::ready!(self.fd.poll_write_ready(cx))?;
                if let Ok(Ok(n)) = guard.try_io(|fd| {
                    // SAFETY: write(2) reads from `buf`, which is valid for
                    // its length.
                    let n = unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
                    if n < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(usize::try_from(n).map_err(|_| io::Error::other("write length overflow"))?)
                    }
                }) {
                    return Poll::Ready(Ok(n));
                }
                // WouldBlock (or the readiness was consumed): the loop
                // re-arms via poll_write_ready.
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            // A pipe write is never buffered by the kernel.
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}
