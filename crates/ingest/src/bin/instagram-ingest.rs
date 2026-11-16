//! The ingester process: `instagram-sync ingest`, without the browser.
//!
//! Applies the migrations, loads the session's cookie jar from the database (seeding it
//! once from `INSTAGRAM_COOKIES_FROM`), then polls and backfills forever. A session
//! Instagram rejects is recorded and retried after `INSTAGRAM_AUTH_RETRY` seconds rather
//! than ending the process. Beside the loop, the live socket nudges it into a quick pass
//! whenever the inbox changes.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use instagram_client::web::Web;
use instagram_core::db::{create_pool, ensure_schema};
use instagram_ingest::config::IngestSettings;
use instagram_ingest::listen::{self, Nudge};
use instagram_ingest::ratelimit::TokenBucket;
use instagram_ingest::runner;
use instagram_ingest::source::WebSource;

#[tokio::main]
async fn main() -> ExitCode {
    let settings = match IngestSettings::from_env() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("configuration error: {error}");
            return ExitCode::FAILURE;
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| settings.log_level.to_lowercase().into()),
        )
        .init();
    let bucket = match TokenBucket::new(settings.rate_limit_rps, settings.rate_limit_burst) {
        Ok(bucket) => Arc::new(bucket),
        Err(error) => {
            tracing::error!("configuration error: {error}");
            return ExitCode::FAILURE;
        }
    };

    let pool = match create_pool(settings.dsn.expose()).await {
        Ok(pool) => pool,
        Err(error) => {
            tracing::error!("could not connect to the database: {error}");
            return ExitCode::FAILURE;
        }
    };
    match ensure_schema(&pool).await {
        Ok(applied) if !applied.is_empty() => {
            tracing::info!("applied migrations: {}", applied.join(", "));
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!("could not apply the migrations: {error}");
            return ExitCode::FAILURE;
        }
    }

    let retry = Duration::from_secs(settings.auth_retry);
    let jar = loop {
        match runner::session_jar(&pool, &settings).await {
            Ok(Some(jar)) => break jar,
            Ok(None) => tracing::error!(
                "no Instagram session: ingest_session holds none for {} and \
                 INSTAGRAM_COOKIES_FROM names no export ({:?}); checking again in {}s",
                settings.account,
                settings.cookies_from,
                settings.auth_retry
            ),
            Err(error) => tracing::error!(
                "could not load the Instagram session, checking again in {}s: {error}",
                settings.auth_retry
            ),
        }
        tokio::time::sleep(retry).await;
    };
    let web = match Web::new(jar) {
        Ok(web) => web,
        Err(error) => {
            tracing::error!("could not build the HTTP client: {error}");
            return ExitCode::FAILURE;
        }
    };
    let nudge = Arc::new(Nudge::default());
    tokio::spawn(listen::listen(
        pool.clone(),
        bucket.clone(),
        nudge.clone(),
        settings.clone(),
    ));
    runner::run(
        &pool,
        &mut WebSource::new(web, bucket, nudge.clone()),
        &settings,
        &nudge,
    )
    .await
}
