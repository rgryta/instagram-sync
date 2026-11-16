//! A port of `ingest/runner.py`: poll, then backfill with what is left, forever.
//!
//! Poll and backfill share one [`Source`] and so one token bucket, which is what makes
//! the priority between them real: backfill reserves headroom, poll does not.
//!
//! Two deliberate changes from Python. A rejected session no longer ends the process:
//! `return 2` under docker's `unless-stopped` restarted straight into the same rejection
//! (2026-10-07), so it is recorded and tried again after `INSTAGRAM_AUTH_RETRY`. And an
//! HTTP 429 doubles the wait before the next pass.
//!
//! Between full passes the live socket (`crate::listen`) can nudge the loop into a quick
//! pass: the inbox's first page and the threads on it that moved, no backfill.

use std::time::Duration;

use tokio::time::Instant;

use chrono::Utc;
use instagram_client::cookies::Jar;
use instagram_client::web::WebError;
use instagram_core::store;
use sqlx::PgPool;

use crate::config::IngestSettings;
use crate::listen::Nudge;
use crate::source::Source;
use crate::{IngestError, backfill, normalize, poll};

/// The sleep before the next pass, jittered by up to `poll_jitter` either way, and
/// never under a minute. A metronome is a machine signature.
pub fn next_interval(settings: &IngestSettings) -> Duration {
    let interval = settings.poll_interval as f64;
    let spread = (interval * settings.poll_jitter).abs();
    let jitter = if spread > 0.0 {
        rand::random_range(-spread..=spread)
    } else {
        0.0
    };
    Duration::from_secs_f64((interval + jitter).max(60.0))
}

/// The session's cookie jar: the database's, or else seeded once from the
/// `INSTAGRAM_COOKIES_FROM` export and saved. `None` when neither has one.
pub async fn session_jar(pool: &PgPool, settings: &IngestSettings) -> Result<Option<Jar>, String> {
    let account = &settings.account;
    if let Some(saved) = store::load_session(pool, account)
        .await
        .map_err(|e| e.to_string())?
    {
        return Jar::from_json(&saved)
            .map(Some)
            .map_err(|e| format!("the saved session is not a cookie jar: {e}"));
    }
    let Some(path) = &settings.cookies_from else {
        return Ok(None);
    };
    let export = match std::fs::read_to_string(path) {
        Ok(export) => export,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("INSTAGRAM_COOKIES_FROM: {e}")),
    };
    let jar = Jar::from_json(&export)
        .map_err(|e| format!("INSTAGRAM_COOKIES_FROM is not a cookie export: {e}"))?;
    store::save_session(pool, account, &jar.to_json())
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("seeded the session for {account} from INSTAGRAM_COOKIES_FROM");
    Ok(Some(jar))
}

/// How long a nudge waits for the rest of its burst (a message, its read receipt, a
/// reaction) before the quick pass; so also at most one quick pass per this.
pub const SETTLE: Duration = Duration::from_secs(30);

/// The loop's state between passes.
#[derive(Default)]
pub struct Runner {
    signed_in: bool,
}

impl Runner {
    /// One pass, its failure recorded, and the jar saved if it rotated. Returns how long
    /// to wait before the next pass; nothing here ends the process.
    pub async fn pass(
        &mut self,
        pool: &PgPool,
        source: &mut impl Source,
        settings: &IngestSettings,
    ) -> Duration {
        self.any_pass(pool, source, settings, false).await.0
    }

    /// A pass the live socket asked for: the inbox's first page, no backfill. `Err` is how
    /// long to hold off after a failure, which is recorded as a full pass's would be.
    pub async fn quick_pass(
        &mut self,
        pool: &PgPool,
        source: &mut impl Source,
        settings: &IngestSettings,
    ) -> Result<(), Duration> {
        match self.any_pass(pool, source, settings, true).await {
            (_, true) => Ok(()),
            (wait, false) => Err(wait),
        }
    }

    /// The wait before the next full pass, and whether this one worked.
    async fn any_pass(
        &mut self,
        pool: &PgPool,
        source: &mut impl Source,
        settings: &IngestSettings,
        quick: bool,
    ) -> (Duration, bool) {
        let account_id = settings.account.as_str();
        let outcome = self.try_pass(pool, source, settings, quick).await;
        let worked = outcome.is_ok();
        let wait = match outcome {
            Ok(()) => next_interval(settings),
            Err(IngestError::Web(WebError::LoggedOut(reason))) => {
                let error = WebError::LoggedOut(reason).to_string();
                if let Err(e) = store::record_auth_error(pool, account_id, &error).await {
                    tracing::error!("could not record the auth error: {e}");
                }
                tracing::error!(
                    "session rejected, trying again in {}s: {error}",
                    settings.auth_retry
                );
                Duration::from_secs(settings.auth_retry)
            }
            Err(IngestError::Web(WebError::RateLimited(reason))) => {
                let error = WebError::RateLimited(reason).to_string();
                tracing::warn!("poll pass rate limited, waiting twice as long: {error}");
                record_failure(pool, account_id, &error).await;
                next_interval(settings) * 2
            }
            Err(error) => {
                tracing::error!("poll pass failed, continuing: {error}");
                record_failure(pool, account_id, &error.to_string()).await;
                next_interval(settings)
            }
        };
        if let Some(jar) = source.end_pass()
            && let Err(e) = store::save_session(pool, account_id, &jar).await
        {
            tracing::error!("could not save the rotated session: {e}");
        }
        (wait, worked)
    }

    async fn try_pass(
        &mut self,
        pool: &PgPool,
        source: &mut impl Source,
        settings: &IngestSettings,
        quick: bool,
    ) -> Result<(), IngestError> {
        let account_id = settings.account.as_str();
        if !self.signed_in {
            let viewer = source.me().await?;
            store::upsert_account(pool, &normalize::account_from_viewer(&viewer, account_id))
                .await?;
            tracing::info!(
                "signed in as {}",
                normalize::user_id(&viewer).unwrap_or_default()
            );
            self.signed_in = true;
        }

        let tally = poll::poll_once(
            pool,
            source,
            account_id,
            settings.poll_thread_budget,
            settings.sweep_window_days,
            quick,
        )
        .await?;
        store::record_sync(pool, account_id, "poll", None, None).await?;
        // A completed pass is proof the session works, so retire any dead-session flag.
        store::clear_auth_error(pool, account_id).await?;
        tracing::info!(
            "{}: {} threads seen, {} opened, {} new messages, {} revised, {} deleted",
            if quick { "live poll" } else { "poll" },
            tally.threads_seen,
            tally.threads_opened,
            tally.messages,
            tally.revisions,
            tally.deletions
        );

        if settings.backfill_enabled && !quick {
            // The ceiling is a UTC calendar day, kept in the database so a restart does
            // not reset it. Wall-clock days rather than a rolling window on purpose: the
            // account should look like a person, and a person's traffic has days in it.
            let today = Utc::now().date_naive();
            let left = (settings.backfill_daily_requests
                - store::backfill_spent(pool, account_id, today).await?)
                .max(0);
            let allowance = settings.backfill_request_budget.min(left);
            if allowance <= 0 {
                tracing::info!("backfill: daily request ceiling reached, resting until tomorrow");
            } else {
                let back = backfill::backfill_once(pool, source, account_id, allowance).await?;
                let spent = i64::try_from(back.requests).unwrap_or(i64::MAX);
                store::spend_backfill(pool, account_id, today, spent).await?;
                store::record_sync(pool, account_id, "backfill", None, None).await?;
                tracing::info!(
                    "backfill: {} requests, {} messages, {} threads finished ({} left today)",
                    back.requests,
                    back.messages,
                    back.threads_finished,
                    (left - spent).max(0)
                );
            }
        }
        Ok(())
    }
}

async fn record_failure(pool: &PgPool, account_id: &str, error: &str) {
    if let Err(e) = store::record_sync(pool, account_id, "poll", None, Some(error)).await {
        tracing::error!("could not record the failed pass: {e}");
    }
}

/// Pass after pass, forever, with a quick pass whenever `nudge` fires in between.
///
/// The nudges of a burst fold into one quick pass, and any during that pass into one
/// more. A full pass's backfill stops between pages for a nudge (see `WebSource`), so a
/// change does not wait out a long backfill.
pub async fn run(
    pool: &PgPool,
    source: &mut impl Source,
    settings: &IngestSettings,
    nudge: &Nudge,
) -> ! {
    let mut runner = Runner::default();
    loop {
        let due = Instant::now() + runner.pass(pool, source, settings).await;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(due) => break,
                () = nudge.take() => {}
            }
            tokio::time::sleep_until((Instant::now() + SETTLE).min(due)).await;
            if Instant::now() >= due {
                break;
            }
            nudge.clear();
            if let Err(wait) = runner.quick_pass(pool, source, settings).await {
                // No more quick passes into a failing session: wait it out, then go full.
                tokio::time::sleep_until(due.max(Instant::now() + wait)).await;
                break;
            }
        }
    }
}
