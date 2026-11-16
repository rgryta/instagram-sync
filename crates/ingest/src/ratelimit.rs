//! A port of `instagram/ratelimit.py`: an async token bucket, shared by every request
//! the process makes.
//!
//! Instagram publishes no rate-limit table for the private API, so the default rate is a
//! self-imposed floor rather than a documented ceiling: there is no number to stay
//! under, only a shape that stays boring. This is the single place to slow down when
//! reality disagrees with the guess.

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BucketError {
    #[error("rate must be positive")]
    Rate,
    #[error("burst must be at least 1")]
    Burst,
}

/// `rate` tokens per second, at most `burst` held at once.
///
/// Time is tokio's monotonic clock, not the wall clock, so a clock step cannot hand out
/// a windfall of tokens or stall the bucket for hours, and tests can pause it.
pub struct TokenBucket {
    rate: f64,
    burst: f64,
    /// Tokens held, and when that was last brought up to date.
    state: Mutex<(f64, Instant)>,
}

impl TokenBucket {
    pub fn new(rate: f64, burst: u32) -> Result<Self, BucketError> {
        if rate.is_nan() || rate <= 0.0 {
            return Err(BucketError::Rate);
        }
        if burst < 1 {
            return Err(BucketError::Burst);
        }
        let burst = f64::from(burst);
        Ok(Self {
            rate,
            burst,
            state: Mutex::new((burst, Instant::now())),
        })
    }

    fn refill(&self, state: &mut (f64, Instant)) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(state.1).as_secs_f64();
        *state = ((state.0 + elapsed * self.rate).min(self.burst), now);
    }

    /// Wait until a token is available, then spend it.
    ///
    /// The lock is held across the sleep so waiters are served in arrival order and two
    /// callers cannot both decide the same token is theirs.
    ///
    /// `reserve` is how much of the bucket the caller agrees to leave alone: it waits for
    /// `1 + reserve` tokens but still spends only one. A background caller passing a
    /// reserve yields to a foreground one that passes none, without a scheduler. The
    /// reserve is capped below `burst`, because a reserve the bucket can never hold would
    /// block the caller forever rather than deprioritise it.
    pub async fn acquire(&self, reserve: f64) {
        let headroom = 1.0 + reserve.min(self.burst - 1.0);
        let mut state = self.state.lock().await;
        self.refill(&mut state);
        if state.0 < headroom {
            tokio::time::sleep(Duration::from_secs_f64((headroom - state.0) / self.rate)).await;
            self.refill(&mut state);
        }
        state.0 -= 1.0;
    }
}
