//! The ingester's half of `config.py`: `IngestSettings`, read from `INSTAGRAM_*`.
//!
//! The Chromium harvester's settings and the `sessionid`/`csrftoken` pair are gone: the
//! session is a cookie jar in the database, seeded once from `INSTAGRAM_COOKIES_FROM`.
//! It lives in this crate rather than in `instagram-core`, so the MCP process cannot
//! even name it.

use std::env;

use instagram_core::config::Secret;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("{name} is not valid: {value:?}")]
    Invalid { name: &'static str, value: String },
}

#[derive(Debug, Clone)]
pub struct IngestSettings {
    pub account: String,
    pub dsn: Secret,
    /// Deliberately slow. Instagram publishes no rate-limit table for the private API,
    /// so this is a self-imposed floor rather than a documented ceiling.
    pub rate_limit_rps: f64,
    pub rate_limit_burst: u32,
    pub log_level: String,
    /// 15 minutes, not 30 seconds. A DM archive has no latency requirement, and
    /// polling cadence is the one detection lever entirely under our control.
    pub poll_interval: u64,
    /// Fraction of `poll_interval` added or subtracted at random per pass.
    pub poll_jitter: f64,
    /// How many threads a poll pass will open, newest activity first.
    pub poll_thread_budget: usize,
    /// How far back a poll reconciles what it re-reads. Zero disables it.
    pub sweep_window_days: i64,
    pub backfill_enabled: bool,
    /// Requests one backfill pass may spend.
    pub backfill_request_budget: i64,
    /// And a ceiling across the day.
    pub backfill_daily_requests: i64,
    /// A Playwright `context.cookies()` export that seeds the jar when the database has
    /// none. Read once; from then on the database holds the session.
    pub cookies_from: Option<String>,
    /// Seconds to wait after Instagram rejects the session before trying again. Exiting
    /// instead restarted the container into the same rejection (2026-10-07).
    pub auth_retry: u64,
}

impl IngestSettings {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| env::var(name).ok())
    }

    /// Split out so tests need no `set_var`, which is process-global and unsafe in
    /// edition 2024.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let required = |name: &'static str| lookup(name).ok_or(ConfigError::Missing(name));
        Ok(Self {
            account: required("INSTAGRAM_ACCOUNT")?,
            dsn: Secret::new(required("INSTAGRAM_DSN")?),
            rate_limit_rps: parsed(&lookup, "INSTAGRAM_RATE_LIMIT_RPS", 0.2)?,
            rate_limit_burst: parsed(&lookup, "INSTAGRAM_RATE_LIMIT_BURST", 3)?,
            log_level: lookup("INSTAGRAM_LOG_LEVEL").unwrap_or_else(|| "INFO".to_owned()),
            poll_interval: parsed(&lookup, "INSTAGRAM_POLL_INTERVAL", 900)?,
            poll_jitter: parsed(&lookup, "INSTAGRAM_POLL_JITTER", 0.2)?,
            poll_thread_budget: parsed(&lookup, "INSTAGRAM_POLL_THREAD_BUDGET", 20)?,
            sweep_window_days: parsed(&lookup, "INSTAGRAM_SWEEP_WINDOW_DAYS", 7)?,
            backfill_enabled: flag(&lookup, "INSTAGRAM_BACKFILL_ENABLED", true)?,
            backfill_request_budget: parsed(&lookup, "INSTAGRAM_BACKFILL_REQUEST_BUDGET", 40)?,
            backfill_daily_requests: parsed(&lookup, "INSTAGRAM_BACKFILL_DAILY_REQUESTS", 400)?,
            cookies_from: lookup("INSTAGRAM_COOKIES_FROM").filter(|path| !path.is_empty()),
            auth_retry: parsed(&lookup, "INSTAGRAM_AUTH_RETRY", 21_600)?,
        })
    }
}

fn parsed<T: std::str::FromStr>(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: T,
) -> Result<T, ConfigError> {
    match lookup(name) {
        None => Ok(default),
        Some(raw) => raw
            .trim()
            .parse()
            .map_err(|_| ConfigError::Invalid { name, value: raw }),
    }
}

/// pydantic's boolean spellings.
fn flag(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: bool,
) -> Result<bool, ConfigError> {
    let Some(raw) = lookup(name) else {
        return Ok(default);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Ok(false),
        _ => Err(ConfigError::Invalid { name, value: raw }),
    }
}
