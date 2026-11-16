//! The token bucket, ported from `tests/test_ratelimit.py`. Tokio's clock is paused, so
//! every wait is virtual: elapsed time is exactly what the bucket slept.

use std::time::Duration;

use instagram_ingest::ratelimit::{BucketError, TokenBucket};
use tokio::time::Instant;

fn close(elapsed: Duration, seconds: f64) {
    assert!(
        (elapsed.as_secs_f64() - seconds).abs() < 1e-6,
        "slept {elapsed:?}, expected {seconds}s"
    );
}

#[test]
fn invalid_settings_are_rejected() {
    assert_eq!(TokenBucket::new(0.0, 1).err(), Some(BucketError::Rate));
    assert_eq!(TokenBucket::new(-1.0, 1).err(), Some(BucketError::Rate));
    assert_eq!(TokenBucket::new(1.0, 0).err(), Some(BucketError::Burst));
}

#[tokio::test(start_paused = true)]
async fn burst_is_free_then_the_rate_applies() {
    let bucket = TokenBucket::new(2.0, 3).unwrap();
    let start = Instant::now();
    for _ in 0..3 {
        bucket.acquire(0.0).await;
    }
    close(start.elapsed(), 0.0);
    bucket.acquire(0.0).await;
    close(start.elapsed(), 0.5);
}

#[tokio::test(start_paused = true)]
async fn the_shipped_default_rate_is_one_request_every_five_seconds() {
    // 0.2/s is the self-imposed floor the whole repo is built around, so it is pinned.
    let bucket = TokenBucket::new(0.2, 3).unwrap();
    let start = Instant::now();
    for _ in 0..4 {
        bucket.acquire(0.0).await;
    }
    close(start.elapsed(), 5.0);
}

#[tokio::test(start_paused = true)]
async fn concurrent_callers_are_serialised() {
    // Five callers through a one-token bucket cost four intervals, not zero.
    let bucket = TokenBucket::new(4.0, 1).unwrap();
    let start = Instant::now();
    tokio::join!(
        bucket.acquire(0.0),
        bucket.acquire(0.0),
        bucket.acquire(0.0),
        bucket.acquire(0.0),
        bucket.acquire(0.0),
    );
    close(start.elapsed(), 1.0);
}

#[tokio::test(start_paused = true)]
async fn tokens_refill_over_time() {
    let bucket = TokenBucket::new(1.0, 2).unwrap();
    bucket.acquire(0.0).await;
    bucket.acquire(0.0).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    // Refill is capped at `burst`; idle time does not bank an unbounded credit.
    let start = Instant::now();
    bucket.acquire(0.0).await;
    bucket.acquire(0.0).await;
    close(start.elapsed(), 0.0);
    bucket.acquire(0.0).await;
    close(start.elapsed(), 1.0);
}

#[tokio::test(start_paused = true)]
async fn a_reserve_yields_to_a_caller_without_one() {
    // Arrival order alone is fair, and fair is wrong: a continuously-running backfill
    // would take half the budget and double how long a poll pass needs.
    let bucket = TokenBucket::new(1.0, 4).unwrap();
    let start = Instant::now();
    bucket.acquire(0.0).await; // foreground spends one, 3 left
    // Background wants 1 + 2 reserved = 3, and exactly 3 are there.
    bucket.acquire(2.0).await;
    close(start.elapsed(), 0.0);
    // Now 2 remain, below the background caller's floor, so it waits...
    bucket.acquire(2.0).await;
    close(start.elapsed(), 1.0);
    // ...while the foreground caller spends what is left without waiting.
    bucket.acquire(0.0).await;
    close(start.elapsed(), 1.0);
}

#[tokio::test(start_paused = true)]
async fn a_reserve_larger_than_the_bucket_still_lets_the_caller_through() {
    // A floor the bucket can never reach would block forever rather than deprioritise.
    let bucket = TokenBucket::new(1.0, 2).unwrap();
    let start = Instant::now();
    bucket.acquire(99.0).await;
    assert!(start.elapsed() < Duration::from_secs(5));
}
