//! Token-bucket bandwidth limiter for data transfer pacing.

use std::time::Duration;
use tokio::sync::Mutex;

/// A token bucket that paces the average transfer rate.
///
/// `acquire(bytes)` blocks until that many bytes' worth of budget is available
/// and then consumes it. Tokens accumulate without a cap, so an idle limiter
/// allows an immediate burst (rsync `--bwlimit` behavior) and an oversized
/// single acquire simply waits the proportional wall-clock time. Multiple
/// concurrent acquirers share the same bucket, so the aggregate rate is
/// capped, not the per-stream rate.
pub struct BandwidthLimiter {
    bytes_per_sec: f64,
    state: Mutex<BucketState>,
}

struct BucketState {
    /// Available budget in bytes; grows at `bytes_per_sec`.
    tokens: f64,
    last: tokio::time::Instant,
}

impl BandwidthLimiter {
    /// Create a limiter for `bytes_per_sec` (0 is treated as 1).
    #[must_use]
    pub fn new(bytes_per_sec: u64) -> Self {
        // Token-bucket math is inherently fractional (sub-byte refills);
        // precision loss above 2^53 B/s is irrelevant for a bandwidth cap.
        #[expect(clippy::cast_precision_loss)]
        let bytes_per_sec = bytes_per_sec.max(1) as f64;
        Self {
            bytes_per_sec,
            state: Mutex::new(BucketState {
                tokens: 0.0,
                last: tokio::time::Instant::now(),
            }),
        }
    }

    /// Wait until `bytes` worth of budget is available, then consume it.
    pub async fn acquire(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        // Same precision rationale as `new`: fractional accounting only.
        #[expect(clippy::cast_precision_loss)]
        let need = bytes as f64;
        let mut state = self.state.lock().await;
        loop {
            let now = tokio::time::Instant::now();
            let elapsed = now.saturating_duration_since(state.last).as_secs_f64();
            state.tokens += elapsed * self.bytes_per_sec;
            state.last = now;

            if state.tokens >= need {
                state.tokens -= need;
                return;
            }

            let wait = (need - state.tokens) / self.bytes_per_sec;
            drop(state);
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            state = self.state.lock().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_zero_returns_immediately() {
        let limiter = BandwidthLimiter::new(1024);
        limiter.acquire(0).await;
    }

    #[tokio::test]
    async fn small_acquire_is_immediate() {
        let limiter = BandwidthLimiter::new(1024 * 1024);
        let start = tokio::time::Instant::now();
        limiter.acquire(1024).await;
        assert!(start.elapsed().as_millis() < 100);
    }

    #[tokio::test]
    async fn large_acquire_paces_to_rate() {
        let limiter = BandwidthLimiter::new(1024 * 1024); // 1 MiB/s
        let start = tokio::time::Instant::now();
        // 2 MiB in one call must not deadlock and takes ~2s at 1 MiB/s.
        limiter.acquire(2 * 1024 * 1024).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 1800,
            "expected ~2s pacing, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn idle_burst_is_allowed() {
        // Tokens accumulate while idle: an immediate burst after a pause.
        let limiter = BandwidthLimiter::new(1024 * 1024);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let start = tokio::time::Instant::now();
        limiter.acquire(128 * 1024).await;
        assert!(start.elapsed().as_millis() < 100);
    }
}
