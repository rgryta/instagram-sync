//! The live socket's side of the ingester: listen on Instagram's gateway socket
//! (`instagram_client::live`) and nudge the runner into a quick pass when the inbox changes.
//!
//! A delta is only a trigger here. The pass reads the change through the same queries as
//! every other pass, so nothing depends on a delta's fields, and a delta that is missed
//! (the socket down, a reconnect) costs at most the wait for the next full pass. For the
//! same reason every connection starts from a fresh INBOX seq id rather than resuming.
//!
//! Each (re)connect costs two requests from the shared bucket: the page and the INBOX
//! query. The socket's own frames are not requests and take no token.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use instagram_client::cookies::Jar;
use instagram_client::graphql;
use instagram_client::live::{self, Event, Live};
use instagram_client::web::{Web, WebError};
use instagram_core::store;
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::config::IngestSettings;
use crate::ratelimit::TokenBucket;

/// Seconds before reconnecting, by how many connections in a row have failed.
const BACKOFF: [u64; 5] = [10, 60, 300, 900, 1_800];
/// A socket open this long was healthy: the backoff starts over.
const HEALTHY: Duration = Duration::from_secs(600);

/// The socket's signal to the runner: a change is waiting. `pending` lets the backfill
/// stop between pages for it (`source::WebSource`); `wake` lets an idle runner wait for it.
#[derive(Default)]
pub struct Nudge {
    pending: AtomicBool,
    wake: Notify,
}

impl Nudge {
    pub fn send(&self) {
        self.pending.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// Forget what is pending: the pass about to run reads it.
    pub fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }

    /// Wait until a nudge is pending, and take it.
    pub async fn take(&self) {
        while !self.pending.swap(false, Ordering::AcqRel) {
            self.wake.notified().await;
        }
    }
}

/// Connect, listen, and reconnect, forever. A rejected session waits
/// `INSTAGRAM_AUTH_RETRY`, as the runner does.
pub async fn listen(
    pool: PgPool,
    bucket: Arc<TokenBucket>,
    nudge: Arc<Nudge>,
    settings: IngestSettings,
) {
    let mut failures: u32 = 0;
    loop {
        let mut opened = None;
        let Err(ended) = follow(&pool, &bucket, &nudge, &settings, failures, &mut opened).await;
        if opened.is_some_and(|at: Instant| at.elapsed() >= HEALTHY) {
            failures = 0;
        }
        let pause = match ended {
            WebError::LoggedOut(_) => settings.auth_retry,
            _ => BACKOFF[(failures as usize).min(BACKOFF.len() - 1)],
        };
        failures += 1;
        tracing::warn!("live socket ended, reconnecting in {pause}s: {ended}");
        tokio::time::sleep(Duration::from_secs(pause)).await;
    }
}

/// One connection, until it ends. `opened` is set once the stream is live.
async fn follow(
    pool: &PgPool,
    bucket: &TokenBucket,
    nudge: &Nudge,
    settings: &IngestSettings,
    retries: u32,
    opened: &mut Option<Instant>,
) -> Result<Infallible, WebError> {
    let other = |e: &dyn std::fmt::Display| WebError::Other(e.to_string());
    // The runner saves every rotation, so the saved jar is the freshest. Rotations this
    // connection receives are not saved back: two writers would race over one row.
    let saved = store::load_session(pool, &settings.account)
        .await
        .map_err(|e| other(&e))?
        .ok_or_else(|| WebError::Other("no saved session yet".into()))?;
    let mut web = Web::new(Jar::from_json(&saved).map_err(|e| other(&e))?)?;
    bucket.acquire(0.0).await;
    let config = web.page().await?;
    bucket.acquire(0.0).await;
    let inbox = web
        .query(
            &config,
            &graphql::INBOX,
            graphql::INBOX_FIELD,
            &graphql::inbox_variables(&config),
        )
        .await?
        .ok_or_else(|| WebError::Other("the inbox answered null".into()))?;
    let seq_id = live::mailbox_seq_id(&inbox)
        .ok_or_else(|| WebError::Other("the inbox carries no seq id".into()))?;
    let mut jar = web.jar().clone();
    let snapshot_at_ms = Utc::now().timestamp_millis();
    let mut socket = Live::connect(&config, &mut jar, seq_id, snapshot_at_ms, retries)
        .await
        .map_err(|e| other(&e))?;
    *opened = Some(Instant::now());
    tracing::info!("live socket open");
    loop {
        let deadline = Instant::now() + Duration::from_secs(3_600);
        match socket.next_event(deadline).await.map_err(|e| other(&e))? {
            None | Some(Event::Noop) => {}
            Some(Event::Connected {
                subscribed_seq_id,
                latest_seq_id,
            }) => {
                let behind = latest_seq_id.zip(subscribed_seq_id).map(|(l, s)| l - s);
                tracing::info!("live socket subscribed, {behind:?} behind the latest");
            }
            // The type name only: the rest carries names and message text.
            Some(Event::Delta { typename, .. }) => {
                tracing::info!("live: {typename}");
                nudge.send();
            }
            Some(Event::Resnapshot(why)) => {
                return Err(WebError::Other(format!("resnapshot {why}")));
            }
            Some(Event::Failure(why)) => {
                return Err(WebError::Other(format!("sync failure {why}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Nudge;
    use std::time::Duration;

    #[tokio::test]
    async fn a_nudge_is_pending_until_taken_and_a_burst_is_one_nudge() {
        let nudge = Nudge::default();
        assert!(!nudge.pending());
        nudge.send();
        nudge.send();
        assert!(nudge.pending());
        nudge.take().await;
        assert!(!nudge.pending());
        // The burst left a stale wake-up behind; it must not count as a second nudge.
        let again = tokio::time::timeout(Duration::from_millis(50), nudge.take()).await;
        assert!(again.is_err(), "nothing was pending");
    }
}
