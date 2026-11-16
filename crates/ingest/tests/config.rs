//! `IngestSettings`, ported from the ingest half of `tests/test_config.py`.

use std::collections::HashMap;
use std::time::Duration;

use instagram_ingest::config::{ConfigError, IngestSettings};
use instagram_ingest::runner::next_interval;

const DSN: &str = "postgresql://u:hunter2@localhost/instagram";

fn settings(extra: &[(&str, &str)]) -> Result<IngestSettings, ConfigError> {
    let mut env: HashMap<String, String> = HashMap::from([
        ("INSTAGRAM_ACCOUNT".to_owned(), "me".to_owned()),
        ("INSTAGRAM_DSN".to_owned(), DSN.to_owned()),
    ]);
    for (name, value) in extra {
        env.insert((*name).to_owned(), (*value).to_owned());
    }
    IngestSettings::from_lookup(|name| env.get(name).cloned())
}

#[test]
fn secrets_do_not_print() {
    let settings = settings(&[]).unwrap();
    assert!(!format!("{settings:?}").contains("hunter2"));
    assert_eq!(settings.dsn.expose(), DSN);
}

#[test]
fn the_defaults_are_slow_on_purpose() {
    // Cadence is the one detection lever entirely under our control.
    let settings = settings(&[]).unwrap();
    assert!(settings.poll_interval >= 600);
    assert!(settings.rate_limit_rps <= 0.5);
    assert_eq!(settings.rate_limit_burst, 3);
    assert_eq!(settings.auth_retry, 21_600);
    assert_eq!(settings.cookies_from, None);
}

#[test]
fn ingest_settings_read_from_environment() {
    let settings = settings(&[
        ("INSTAGRAM_COOKIES_FROM", "/seed/cookies.json"),
        ("INSTAGRAM_BACKFILL_ENABLED", "false"),
        ("INSTAGRAM_POLL_INTERVAL", "1200"),
    ])
    .unwrap();
    assert_eq!(settings.account, "me");
    assert_eq!(settings.cookies_from.as_deref(), Some("/seed/cookies.json"));
    assert!(!settings.backfill_enabled);
    assert_eq!(settings.poll_interval, 1200);
}

#[test]
fn a_missing_or_malformed_setting_is_named() {
    let empty = IngestSettings::from_lookup(|_| None).unwrap_err();
    assert_eq!(empty, ConfigError::Missing("INSTAGRAM_ACCOUNT"));
    let bad = settings(&[("INSTAGRAM_POLL_INTERVAL", "soon")]).unwrap_err();
    assert!(matches!(
        bad,
        ConfigError::Invalid {
            name: "INSTAGRAM_POLL_INTERVAL",
            ..
        }
    ));
}

#[test]
fn the_interval_is_jittered_and_never_under_a_minute() {
    let settings = settings(&[
        ("INSTAGRAM_POLL_INTERVAL", "1000"),
        ("INSTAGRAM_POLL_JITTER", "0.1"),
    ])
    .unwrap();
    for _ in 0..200 {
        let wait = next_interval(&settings);
        assert!(
            wait >= Duration::from_secs(900) && wait <= Duration::from_secs(1100),
            "{wait:?}"
        );
    }
    let short = self::settings(&[("INSTAGRAM_POLL_INTERVAL", "5")]).unwrap();
    assert_eq!(next_interval(&short), Duration::from_secs(60));
}
