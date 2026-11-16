//! Test fixtures.
//!
//! Postgres tests run against a throwaway testcontainer and report a loud skip when
//! Docker is unreachable. They must never reach the homelab database: **nothing here
//! reads a DSN from the environment**, the container builds its own. That is the
//! structural half of "never point tests at a real database", and it is why
//! `#[sqlx::test]` is not used anywhere — that macro wants `DATABASE_URL` from the
//! environment and creates databases itself, which re-opens the door this closes.
//!
//! The seeding below is raw SQL rather than the store's own writers, because the
//! writers are the ingester's and are phase 2's. Migrate this onto them when they land.

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use instagram_core::db::{create_pool, ensure_schema};
use sqlx::postgres::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt, ReuseDirective};
use tokio::sync::{Mutex, OnceCell};

/// Every table the migrations create, minus `schema_migration`: truncating that would
/// make the shared schema look unapplied to the next test. `message_revision` goes with
/// `message` through the CASCADE.
const TRUNCATE_ALL: &str = "TRUNCATE message, thread, participant, person_alias_member, \
                            person_alias, sync_state, account, ingest_session CASCADE";

/// One named container serves every run. It is parked in a `static`, which Rust never
/// drops, so it is marked for reuse rather than reaping: the next run attaches to the
/// same container instead of leaving a second one behind. Remove it with
/// `docker rm -f instagram-sync-test-pg` when you want the disk back.
const CONTAINER_NAME: &str = "instagram-sync-test-pg";

static CONTAINER: OnceCell<Option<(ContainerAsync<Postgres>, String)>> = OnceCell::const_new();
/// One database, many tests, and each one truncates: they have to take turns.
static TURN: Mutex<()> = Mutex::const_new(());

pub struct Fixture {
    pub pool: PgPool,
    _turn: tokio::sync::MutexGuard<'static, ()>,
}

/// A pool over an empty database, or `None` when Docker is unreachable.
///
/// Returning `None` rather than failing is deliberate: Postgres tests skip when Docker
/// is not there, as `conftest.py` does. Rust has no "skipped" outcome, so each caller
/// prints the reason and returns — loudly enough to notice in a log.
pub async fn fixture() -> Option<Fixture> {
    let turn = TURN.lock().await;
    let dsn = container_dsn().await?;
    let pool = create_pool(&dsn).await.expect("the container is up");
    sqlx::query(TRUNCATE_ALL)
        .execute(&pool)
        .await
        .expect("truncate");
    Some(Fixture { pool, _turn: turn })
}

/// The shared container's DSN, starting it if this is the first caller.
///
/// Takes no turn of its own, so it is callable from inside a test that already holds
/// one. `server.rs` needs the DSN itself, to open a second pool against a database the
/// migrations were deliberately not applied to.
pub async fn container_dsn() -> Option<String> {
    CONTAINER
        .get_or_init(|| async {
            let request = Postgres::default()
                .with_tag("18-alpine")
                .with_container_name(CONTAINER_NAME)
                .with_reuse(ReuseDirective::Always);
            match request.start().await {
                Ok(container) => {
                    let host = container.get_host().await.ok()?;
                    let port = container.get_host_port_ipv4(5432).await.ok()?;
                    let dsn = format!("postgresql://postgres:postgres@{host}:{port}/postgres");
                    let pool = create_pool(&dsn).await.ok()?;
                    ensure_schema(&pool).await.ok()?;
                    pool.close().await;
                    Some((container, dsn))
                }
                Err(error) => {
                    eprintln!("SKIPPING the postgres tests: docker is not reachable: {error}");
                    None
                }
            }
        })
        .await
        .as_ref()
        .map(|(_, dsn)| dsn.clone())
}

/// Skip the body with a printed reason when Docker is unreachable.
macro_rules! db_test {
    ($name:ident, |$fixture:ident| $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($fixture) = crate::support::fixture().await else {
                return;
            };
            $body
        }
    };
}
pub(crate) use db_test;

/// A fixed instant, so an assertion about ordering never depends on how fast the test
/// ran. The same one `tests/test_mcp_tools.py` uses.
pub fn base() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap()
}

pub fn minutes(count: i64) -> DateTime<Utc> {
    base() + TimeDelta::minutes(count)
}

pub async fn account(pool: &PgPool, id: &str, ig_user_id: &str, username: Option<&str>) {
    sqlx::query(
        "INSERT INTO account (id, ig_user_id, username) VALUES ($1, $2, $3) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(ig_user_id)
    .bind(username)
    .execute(pool)
    .await
    .expect("insert account");
}

pub async fn participant(
    pool: &PgPool,
    ig_user_id: &str,
    username: Option<&str>,
    full_name: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO participant (account_id, ig_user_id, username, full_name) \
         VALUES ('me', $1, $2, $3) ON CONFLICT (account_id, ig_user_id) DO NOTHING",
    )
    .bind(ig_user_id)
    .bind(username)
    .bind(full_name)
    .execute(pool)
    .await
    .expect("insert participant");
}

pub struct Conv<'a> {
    pub thread_id: &'a str,
    pub title: Option<&'a str>,
    pub is_group: bool,
    pub participant_ids: Vec<&'a str>,
    pub last_activity_at: Option<DateTime<Utc>>,
    pub backfill_done: bool,
}

impl Default for Conv<'_> {
    fn default() -> Self {
        Self {
            thread_id: "T1",
            title: None,
            is_group: false,
            participant_ids: vec!["1", "2"],
            last_activity_at: None,
            backfill_done: false,
        }
    }
}

pub async fn thread(pool: &PgPool, conv: Conv<'_>) {
    let ids: Vec<String> = conv
        .participant_ids
        .iter()
        .map(|id| (*id).to_owned())
        .collect();
    sqlx::query(
        "INSERT INTO thread (account_id, thread_id, title, is_group, participant_ids, \
                             last_activity_at, backfill_done) \
         VALUES ('me', $1, $2, $3, $4, $5, $6) \
         ON CONFLICT (account_id, thread_id) DO NOTHING",
    )
    .bind(conv.thread_id)
    .bind(conv.title)
    .bind(conv.is_group)
    .bind(&ids)
    .bind(conv.last_activity_at)
    .bind(conv.backfill_done)
    .execute(pool)
    .await
    .expect("insert thread");
}

pub struct Msg<'a> {
    pub item_id: &'a str,
    pub thread_id: &'a str,
    pub sender_id: Option<&'a str>,
    /// UPPERCASE, as GraphQL's `content_type` is. The Python test seed says `"text"`;
    /// see `tools.rs::the_seed_uses_the_vocabulary_the_live_account_sends`.
    pub item_type: &'a str,
    pub text: &'a str,
    pub ts: DateTime<Utc>,
    pub share_kind: Option<&'a str>,
    pub share_owner: Option<&'a str>,
    pub share_caption: Option<&'a str>,
    pub share_permalink: Option<&'a str>,
    pub reply_to_item_id: Option<&'a str>,
    pub reactions: serde_json::Value,
}

impl Default for Msg<'_> {
    fn default() -> Self {
        Self {
            item_id: "i1",
            thread_id: "T1",
            sender_id: Some("2"),
            item_type: "TEXT",
            text: "hello",
            ts: base(),
            share_kind: None,
            share_owner: None,
            share_caption: None,
            share_permalink: None,
            reply_to_item_id: None,
            reactions: serde_json::json!([]),
        }
    }
}

pub async fn message(pool: &PgPool, msg: Msg<'_>) {
    sqlx::query(
        "INSERT INTO message (account_id, thread_id, item_id, ts, sender_id, item_type, text, \
                              share_kind, share_owner, share_caption, share_permalink, \
                              reply_to_item_id, reactions, raw) \
         VALUES ('me', $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, '{}'::jsonb)",
    )
    .bind(msg.thread_id)
    .bind(msg.item_id)
    .bind(msg.ts)
    .bind(msg.sender_id)
    .bind(msg.item_type)
    .bind(msg.text)
    .bind(msg.share_kind)
    .bind(msg.share_owner)
    .bind(msg.share_caption)
    .bind(msg.share_permalink)
    .bind(msg.reply_to_item_id)
    .bind(sqlx::types::Json(msg.reactions))
    .execute(pool)
    .await
    .expect("insert message");
}

/// `store.record_sync`, which the ingester owns.
pub async fn record_sync(
    pool: &PgPool,
    loop_name: &str,
    thread_id: Option<&str>,
    error: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO sync_state (account_id, loop, current_thread_id, last_run_at, \
                                 last_error, last_error_at) \
         VALUES ('me', $1, $2, now(), $3, \
                 CASE WHEN $3::text IS NULL THEN NULL ELSE now() END) \
         ON CONFLICT (account_id, loop) DO UPDATE SET \
           current_thread_id = EXCLUDED.current_thread_id, \
           last_run_at = now(), \
           last_error = EXCLUDED.last_error, \
           last_error_at = EXCLUDED.last_error_at",
    )
    .bind(loop_name)
    .bind(thread_id)
    .bind(error)
    .execute(pool)
    .await
    .expect("record sync");
}

pub async fn record_auth_error(pool: &PgPool, error: &str) {
    sqlx::query(
        "UPDATE account SET last_auth_error = $1, last_auth_error_at = now() WHERE id = 'me'",
    )
    .bind(error)
    .execute(pool)
    .await
    .expect("record auth error");
}

/// One row of the revision log, as `store.record_revision` would have written it.
pub async fn revision(
    pool: &PgPool,
    thread_id: &str,
    item_id: &str,
    seq: i32,
    text: &str,
    first_seen_at: Option<DateTime<Utc>>,
    superseded_at: Option<DateTime<Utc>>,
) {
    sqlx::query(
        "INSERT INTO message_revision (account_id, thread_id, item_id, seq, text, \
                                       first_seen_at, superseded_at) \
         VALUES ('me', $1, $2, $3, $4, $5, $6)",
    )
    .bind(thread_id)
    .bind(item_id)
    .bind(seq)
    .bind(text)
    .bind(first_seen_at)
    .bind(superseded_at)
    .execute(pool)
    .await
    .expect("insert revision");
}

/// What the second clean pass that failed to see a message does.
pub async fn mark_deleted(pool: &PgPool, thread_id: &str, item_id: &str) {
    sqlx::query(
        "UPDATE message SET deleted_at = now(), missing_since = now() \
         WHERE account_id = 'me' AND thread_id = $1 AND item_id = $2",
    )
    .bind(thread_id)
    .bind(item_id)
    .execute(pool)
    .await
    .expect("mark deleted");
}

pub const DM: &str = "T-dm";
pub const GROUP: &str = "T-grp";

/// One account, four people, a DM and a group, text and shares in both.
///
/// Ported from the `seeded` fixture in `tests/test_mcp_tools.py`. The DM is deliberately
/// untitled and the group titled, because naming a DM after the person in it is the case
/// that has no fallback.
pub async fn seeded(pool: &PgPool) {
    account(pool, "me", "1", Some("myself")).await;
    participant(pool, "1", Some("myself"), Some("Me")).await;
    participant(pool, "2", Some("k_zaw_92"), Some("Katarzyna Zawadzka")).await;
    participant(pool, "3", Some("olek"), None).await;
    participant(pool, "4", None, None).await;
    thread(
        pool,
        Conv {
            thread_id: DM,
            participant_ids: vec!["1", "2"],
            last_activity_at: Some(minutes(30)),
            backfill_done: true,
            ..Conv::default()
        },
    )
    .await;
    thread(
        pool,
        Conv {
            thread_id: GROUP,
            title: Some("Padel"),
            is_group: true,
            participant_ids: vec!["1", "2", "3"],
            last_activity_at: Some(minutes(10)),
            backfill_done: false,
        },
    )
    .await;
    message(
        pool,
        Msg {
            item_id: "i1",
            thread_id: DM,
            text: "hello there, going sailing tomorrow",
            ..Msg::default()
        },
    )
    .await;
    message(
        pool,
        Msg {
            item_id: "i2",
            thread_id: DM,
            ts: minutes(30),
            item_type: "MESSAGE_INLINE_SHARE",
            text: "@surfclub big waves in Hel",
            share_kind: Some("reel"),
            share_owner: Some("surfclub"),
            share_caption: Some("big waves in Hel"),
            share_permalink: Some("https://www.instagram.com/reel/abc/"),
            ..Msg::default()
        },
    )
    .await;
    message(
        pool,
        Msg {
            item_id: "i3",
            thread_id: GROUP,
            sender_id: Some("3"),
            text: "padel on Thursday?",
            ..Msg::default()
        },
    )
    .await;
    message(
        pool,
        Msg {
            item_id: "i4",
            thread_id: GROUP,
            ts: minutes(10),
            sender_id: Some("1"),
            item_type: "MESSAGE_INLINE_SHARE",
            // `text` carries the author's handle exactly as `share_owner` does, because
            // `ingest.normalize` builds both from the same payload key. The stored
            // capitalisation is the author's own; the shares filter matches it
            // case-insensitively, which is what one of the tests checks.
            text: "@Newsroom the sailing report",
            share_kind: Some("post"),
            share_owner: Some("Newsroom"),
            share_caption: Some("the sailing report"),
            share_permalink: Some("https://www.instagram.com/p/xyz/"),
            ..Msg::default()
        },
    )
    .await;
    record_sync(pool, "poll", None, None).await;
    record_sync(pool, "backfill", Some(GROUP), Some("rate limited")).await;
}
