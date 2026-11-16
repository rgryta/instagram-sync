//! The ingester's writes against a real Postgres.
//!
//! Ported from the write half of `tests/test_store.py` and `tests/test_revisions.py`.
//! Every case needs the database's own behaviour — ON CONFLICT semantics, the composite
//! foreign key, the generated tsvector, row locks — so none of it is testable against a
//! fake.

use std::collections::HashSet;

use chrono::TimeDelta;
use instagram_core::models::{
    AccountRecord, MessageRecord, ParticipantRecord, StoredThread, ThreadRecord,
};
use instagram_core::store;
use serde_json::json;
use sqlx::postgres::PgPool;

use crate::support::{base, db_test};

const ME: &str = "me";
const VIEWER: &str = "100000000000009";
const ALICE: &str = "100000000000001";
const BOB: &str = "100000000000002";

fn account(id: &str, username: Option<&str>) -> AccountRecord {
    AccountRecord {
        id: id.to_owned(),
        ig_user_id: VIEWER.to_owned(),
        username: username.map(str::to_owned),
    }
}

/// A thread as an inbox listing produces one: no crawl state at all.
fn listing(thread_id: &str, account_id: &str) -> ThreadRecord {
    ThreadRecord {
        account_id: account_id.to_owned(),
        thread_id: thread_id.to_owned(),
        title: Some("alice".to_owned()),
        is_group: false,
        participant_ids: vec![ALICE.to_owned(), BOB.to_owned()],
        last_activity_at: Some(base()),
    }
}

fn participant(ig_user_id: &str) -> ParticipantRecord {
    ParticipantRecord {
        account_id: ME.to_owned(),
        ig_user_id: ig_user_id.to_owned(),
        username: None,
        full_name: None,
        is_verified: false,
        profile_pic_url: None,
    }
}

fn msg(item_id: &str, offset_seconds: i64) -> MessageRecord {
    MessageRecord {
        account_id: ME.to_owned(),
        thread_id: "T1".to_owned(),
        item_id: item_id.to_owned(),
        ts: base() + TimeDelta::seconds(offset_seconds),
        sender_id: Some(ALICE.to_owned()),
        item_type: "TEXT".to_owned(),
        text: "hello".to_owned(),
        edited: false,
        share_kind: None,
        share_owner: None,
        share_caption: None,
        share_permalink: None,
        reply_to_item_id: None,
        reactions: Vec::new(),
        raw: json!({}),
    }
}

fn texted(item_id: &str, text: &str) -> MessageRecord {
    MessageRecord {
        text: text.to_owned(),
        ..msg(item_id, 0)
    }
}

fn ids(items: &[&str]) -> Vec<String> {
    items.iter().map(|i| (*i).to_owned()).collect()
}

/// The minimum parent chain: a thread cannot exist without its account.
async fn seed(pool: &PgPool) {
    store::upsert_account(pool, &account(ME, Some("me")))
        .await
        .unwrap();
    store::upsert_thread(pool, &listing("T1", ME))
        .await
        .unwrap();
}

/// `seed` plus one message, `i1`.
async fn one(pool: &PgPool, text: &str) {
    seed(pool).await;
    store::insert_messages(pool, &[texted("i1", text)])
        .await
        .unwrap();
}

async fn thread(pool: &PgPool, thread_id: &str) -> StoredThread {
    store::get_thread(pool, ME, thread_id)
        .await
        .unwrap()
        .expect("the thread is stored")
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

// --------------------------------------------------------------------------- account

db_test!(upsert_account_round_trips_and_refreshes, |f| {
    store::upsert_account(&f.pool, &account(ME, Some("alice")))
        .await
        .unwrap();
    store::upsert_account(&f.pool, &account(ME, None))
        .await
        .unwrap();
    let (username, enabled): (Option<String>, bool) =
        sqlx::query_as("SELECT username, enabled FROM account WHERE id = 'me'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    // A refresh that carries no username must not blank the one already known.
    assert_eq!(username.as_deref(), Some("alice"));
    // `AccountRecord` has no `enabled`; the ingester always writes true.
    assert!(enabled);
});

db_test!(record_auth_error_is_not_erased_by_an_account_refresh, |f| {
    store::upsert_account(&f.pool, &account(ME, None))
        .await
        .unwrap();
    store::record_auth_error(&f.pool, ME, "checkpoint_required")
        .await
        .unwrap();
    store::upsert_account(&f.pool, &account(ME, Some("alice")))
        .await
        .unwrap();
    let (error, at): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT last_auth_error, last_auth_error_at FROM account WHERE id = 'me'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(error.as_deref(), Some("checkpoint_required"));
    assert!(at.is_some());
});

db_test!(a_successful_pass_lifts_the_dead_session_flag, |f| {
    // Nothing else resets it, so without this the status tool keeps reporting an outage
    // that has ended.
    store::upsert_account(&f.pool, &account(ME, None))
        .await
        .unwrap();
    store::record_auth_error(&f.pool, ME, "login_required")
        .await
        .unwrap();
    store::clear_auth_error(&f.pool, ME).await.unwrap();
    let (error, at): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT last_auth_error, last_auth_error_at FROM account WHERE id = 'me'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(error.is_none());
    assert!(at.is_none());
});

// ----------------------------------------------------------------------- participant

db_test!(
    upsert_participants_keeps_the_name_the_thinner_payload_omits,
    |f| {
        store::upsert_account(&f.pool, &account(ME, None))
            .await
            .unwrap();
        store::upsert_participants(
            &f.pool,
            &[
                ParticipantRecord {
                    username: Some("alice".to_owned()),
                    full_name: Some("Alice A".to_owned()),
                    ..participant(ALICE)
                },
                ParticipantRecord {
                    username: Some("bob".to_owned()),
                    ..participant(BOB)
                },
            ],
        )
        .await
        .unwrap();
        // The inbox listing carries the handle and nothing else.
        store::upsert_participants(
            &f.pool,
            &[ParticipantRecord {
                username: Some("alice".to_owned()),
                is_verified: true,
                ..participant(ALICE)
            }],
        )
        .await
        .unwrap();
        let rows: Vec<(String, Option<String>, bool)> = sqlx::query_as(
            "SELECT ig_user_id, full_name, is_verified FROM participant ORDER BY ig_user_id",
        )
        .fetch_all(&f.pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                (ALICE.to_owned(), Some("Alice A".to_owned()), true),
                (BOB.to_owned(), None, false),
            ]
        );
    }
);

db_test!(upsert_participants_of_nothing_is_a_no_op, |f| {
    store::upsert_participants(&f.pool, &[]).await.unwrap();
    assert_eq!(count(&f.pool, "SELECT count(*) FROM participant").await, 0);
});

// ---------------------------------------------------------------------------- thread

db_test!(upsert_thread_round_trips_through_get_thread, |f| {
    seed(&f.pool).await;
    let t = thread(&f.pool, "T1").await;
    assert_eq!(t.title.as_deref(), Some("alice"));
    assert_eq!(t.participant_ids, [ALICE, BOB]);
    assert_eq!(t.last_activity_at, Some(base()));
    assert!(!t.backfill_done);
});

db_test!(get_thread_of_an_unknown_thread_is_none, |f| {
    seed(&f.pool).await;
    assert!(
        store::get_thread(&f.pool, ME, "nope")
            .await
            .unwrap()
            .is_none()
    );
});

db_test!(
    re_listing_a_thread_does_not_reset_a_backfill_in_progress,
    |f| {
        // The inbox listing knows none of the crawl state, and must not write over it.
        seed(&f.pool).await;
        store::set_backfill(&f.pool, ME, "T1", Some("page-2"), false)
            .await
            .unwrap();
        store::set_watermark(&f.pool, ME, "T1", "i-9")
            .await
            .unwrap();

        store::upsert_thread(&f.pool, &listing("T1", ME))
            .await
            .unwrap();

        let t = thread(&f.pool, "T1").await;
        assert_eq!(t.backfill_cursor.as_deref(), Some("page-2"));
        assert_eq!(t.watermark_item_id.as_deref(), Some("i-9"));
    }
);

db_test!(
    re_listing_a_finished_thread_does_not_reopen_its_backfill,
    |f| {
        // The boolean is the case COALESCE cannot carry: false is a value, not an absence.
        seed(&f.pool).await;
        store::set_backfill(&f.pool, ME, "T1", None, true)
            .await
            .unwrap();

        store::upsert_thread(&f.pool, &listing("T1", ME))
            .await
            .unwrap();

        assert!(thread(&f.pool, "T1").await.backfill_done);
    }
);

db_test!(set_backfill_can_reopen_a_finished_thread, |f| {
    // The loop that owns the column is the one thing that can say false out loud.
    seed(&f.pool).await;
    store::set_backfill(&f.pool, ME, "T1", None, true)
        .await
        .unwrap();
    store::set_backfill(&f.pool, ME, "T1", Some("page-1"), false)
        .await
        .unwrap();

    let t = thread(&f.pool, "T1").await;
    assert!(!t.backfill_done);
    assert_eq!(t.backfill_cursor.as_deref(), Some("page-1"));
});

db_test!(set_backfill_clears_a_spent_cursor, |f| {
    seed(&f.pool).await;
    store::set_backfill(&f.pool, ME, "T1", Some("page-2"), false)
        .await
        .unwrap();
    store::set_backfill(&f.pool, ME, "T1", None, true)
        .await
        .unwrap();

    assert!(thread(&f.pool, "T1").await.backfill_cursor.is_none());
});

db_test!(mark_polled_stamps_the_thread, |f| {
    seed(&f.pool).await;
    store::mark_polled(&f.pool, ME, "T1").await.unwrap();
    let t = thread(&f.pool, "T1").await;
    assert!(t.last_polled_at.is_some());
    // And a later listing must not blank the stamp back to NULL.
    store::upsert_thread(&f.pool, &listing("T1", ME))
        .await
        .unwrap();
    assert_eq!(thread(&f.pool, "T1").await.last_polled_at, t.last_polled_at);
});

// ---------------------------------------------------------- threads_needing_backfill

fn thread_ids(threads: &[StoredThread]) -> Vec<&str> {
    threads.iter().map(|t| t.thread_id.as_str()).collect()
}

db_test!(
    threads_needing_backfill_puts_the_least_recently_visited_first,
    |f| {
        store::upsert_account(&f.pool, &account(ME, None))
            .await
            .unwrap();
        for thread_id in ["T1", "T2", "T3"] {
            store::upsert_thread(&f.pool, &listing(thread_id, ME))
                .await
                .unwrap();
        }
        // T2 was visited first, then T1; T3 has never been opened at all.
        store::mark_polled(&f.pool, ME, "T2").await.unwrap();
        store::mark_polled(&f.pool, ME, "T1").await.unwrap();

        let threads = store::threads_needing_backfill(&f.pool, ME, 10)
            .await
            .unwrap();

        assert_eq!(thread_ids(&threads), ["T3", "T2", "T1"]);
    }
);

db_test!(
    threads_needing_backfill_puts_the_shallowest_first_before_the_least_visited,
    |f| {
        store::upsert_account(&f.pool, &account(ME, None))
            .await
            .unwrap();
        for thread_id in ["DEEP", "MID", "OLDCURSOR", "FRESH"] {
            store::upsert_thread(&f.pool, &listing(thread_id, ME))
                .await
                .unwrap();
        }
        // DEEP is the quiet thread the poll never visits; FRESH was just polled.
        store::mark_polled(&f.pool, ME, "MID").await.unwrap();
        store::mark_polled(&f.pool, ME, "OLDCURSOR").await.unwrap();
        store::mark_polled(&f.pool, ME, "FRESH").await.unwrap();
        store::set_backfill(&f.pool, ME, "DEEP", Some("191"), false)
            .await
            .unwrap();
        store::set_backfill(&f.pool, ME, "MID", Some("40"), false)
            .await
            .unwrap();
        // A cursor from the REST era is not a page count, so it counts as depth 0.
        store::set_backfill(&f.pool, ME, "OLDCURSOR", Some("QVFIabc"), false)
            .await
            .unwrap();

        let threads = store::threads_needing_backfill(&f.pool, ME, 10)
            .await
            .unwrap();

        assert_eq!(thread_ids(&threads), ["OLDCURSOR", "FRESH", "MID", "DEEP"]);
    }
);

db_test!(
    threads_needing_backfill_skips_finished_threads_and_honours_limit,
    |f| {
        store::upsert_account(&f.pool, &account(ME, None))
            .await
            .unwrap();
        for thread_id in ["T1", "T2", "T3"] {
            store::upsert_thread(&f.pool, &listing(thread_id, ME))
                .await
                .unwrap();
        }
        store::set_backfill(&f.pool, ME, "T3", None, true)
            .await
            .unwrap();

        let threads = store::threads_needing_backfill(&f.pool, ME, 1)
            .await
            .unwrap();

        assert_eq!(thread_ids(&threads), ["T1"]);
        let all = store::threads_needing_backfill(&f.pool, ME, 10)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
    }
);

db_test!(threads_needing_backfill_is_scoped_to_the_account, |f| {
    seed(&f.pool).await;
    store::upsert_account(&f.pool, &account("other", None))
        .await
        .unwrap();
    store::upsert_thread(&f.pool, &listing("T9", "other"))
        .await
        .unwrap();

    let threads = store::threads_needing_backfill(&f.pool, ME, 10)
        .await
        .unwrap();

    assert_eq!(thread_ids(&threads), ["T1"]);
});

// -------------------------------------------------------------------------- messages

db_test!(
    insert_messages_returns_the_number_of_rows_it_actually_inserted,
    |f| {
        // A backfill loop learns from this count that a page was already stored.
        seed(&f.pool).await;
        assert_eq!(
            store::insert_messages(&f.pool, &[msg("a", 0), msg("b", 1)])
                .await
                .unwrap(),
            2
        );

        // The next page overlaps: two known items and one new one.
        let again = store::insert_messages(&f.pool, &[msg("a", 0), msg("b", 1), msg("c", 2)])
            .await
            .unwrap();
        assert_eq!(again, 1);
        assert_eq!(count(&f.pool, "SELECT count(*) FROM message").await, 3);
    }
);

db_test!(insert_messages_never_rewrites_a_stored_item, |f| {
    seed(&f.pool).await;
    store::insert_messages(&f.pool, &[texted("a", "original")])
        .await
        .unwrap();
    assert_eq!(
        store::insert_messages(&f.pool, &[texted("a", "edited")])
            .await
            .unwrap(),
        0
    );
    let text: String = sqlx::query_scalar("SELECT text FROM message WHERE item_id = 'a'")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "original");
});

db_test!(insert_messages_of_nothing_inserts_nothing, |f| {
    seed(&f.pool).await;
    assert_eq!(store::insert_messages(&f.pool, &[]).await.unwrap(), 0);
});

db_test!(insert_messages_raises_when_the_thread_is_absent, |f| {
    // The FK is the guard against rows no listing can ever reach.
    seed(&f.pool).await;
    let ghost = MessageRecord {
        thread_id: "ghost".to_owned(),
        ..msg("a", 0)
    };
    let error = store::insert_messages(&f.pool, &[ghost])
        .await
        .expect_err("a message without its thread must fail");
    assert!(
        error
            .as_database_error()
            .is_some_and(|e| e.is_foreign_key_violation()),
        "{error}"
    );
    assert_eq!(count(&f.pool, "SELECT count(*) FROM message").await, 0);
});

db_test!(
    insert_messages_round_trips_jsonb_as_objects_not_strings,
    |f| {
        // Double-encoding decodes symmetrically, so only SQL ever notices it.
        seed(&f.pool).await;
        let reactions = vec![json!({"sender_id": BOB, "emoji": "❤️"})];
        let raw = json!({"item_type": "text", "nested": {"n": 1}});
        store::insert_messages(
            &f.pool,
            &[MessageRecord {
                reactions: reactions.clone(),
                raw: raw.clone(),
                ..msg("a", 0)
            }],
        )
        .await
        .unwrap();

        let message = store::newest_message(&f.pool, ME, "T1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.reactions, reactions);
        assert_eq!(message.raw, raw);
        // Asked of the database directly, the payload is a jsonb object, not a quoted string.
        let n: Option<String> =
            sqlx::query_scalar("SELECT raw->'nested'->>'n' FROM message WHERE item_id = 'a'")
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(n.as_deref(), Some("1"));
    }
);

db_test!(the_share_projection_round_trips_through_the_store, |f| {
    seed(&f.pool).await;
    store::insert_messages(
        &f.pool,
        &[MessageRecord {
            text: "@carol.example a caption".to_owned(),
            share_kind: Some("reel".to_owned()),
            share_owner: Some("carol.example".to_owned()),
            share_caption: Some("a caption".to_owned()),
            share_permalink: Some("https://www.instagram.com/reel/FAKECODE000/".to_owned()),
            ..msg("a", 0)
        }],
    )
    .await
    .unwrap();

    let message = store::newest_message(&f.pool, ME, "T1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.share_kind.as_deref(), Some("reel"));
    assert_eq!(message.share_owner.as_deref(), Some("carol.example"));
    assert_eq!(message.share_caption.as_deref(), Some("a caption"));
    assert_eq!(
        message.share_permalink.as_deref(),
        Some("https://www.instagram.com/reel/FAKECODE000/")
    );
});

db_test!(a_message_that_shares_nothing_stores_four_nulls, |f| {
    // Half the corpus, and what the partial indexes exclude. Empty strings here would put
    // every text message into an index that exists only to answer share questions.
    seed(&f.pool).await;
    store::insert_messages(&f.pool, &[msg("a", 0)])
        .await
        .unwrap();

    let row: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT share_kind, share_owner, share_caption, share_permalink FROM message",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(row, (None, None, None, None));

    let message = store::newest_message(&f.pool, ME, "T1")
        .await
        .unwrap()
        .unwrap();
    assert!(message.share_owner.is_none());
});

db_test!(everything_one_person_ever_sent_is_one_query, |f| {
    // Asked the way the partial index is built — `lower(share_owner)`, because handles
    // are typed by hand at the query end.
    seed(&f.pool).await;
    let share = |item_id: &str, offset: i64, kind: &str, owner: &str| MessageRecord {
        share_kind: Some(kind.to_owned()),
        share_owner: Some(owner.to_owned()),
        ..msg(item_id, offset)
    };
    store::insert_messages(
        &f.pool,
        &[
            share("a", 0, "reel", "Carol.Example"),
            share("b", 1, "post", "carol.example"),
            share("c", 2, "reel", "someone.else"),
            msg("d", 3),
        ],
    )
    .await
    .unwrap();

    let found: Vec<String> = sqlx::query_scalar(
        "SELECT item_id FROM message \
          WHERE account_id = $1 AND lower(share_owner) = lower($2) ORDER BY item_id",
    )
    .bind(ME)
    .bind("CAROL.EXAMPLE")
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(found, ["a", "b"]);

    let reels: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM message WHERE account_id = $1 AND share_kind = 'reel'",
    )
    .bind(ME)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(reels, 2);
});

db_test!(stored_messages_are_findable_by_full_text_search, |f| {
    // The tsvector is generated by the schema; nothing in the write path fills it.
    seed(&f.pool).await;
    store::insert_messages(&f.pool, &[texted("a", "dinner at the Pelican tomorrow")])
        .await
        .unwrap();

    let search = "SELECT item_id FROM message WHERE search @@ websearch_to_tsquery('simple', $1)";
    let found: Vec<String> = sqlx::query_scalar(search)
        .bind("pelican")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(found, ["a"]);
    let missing: Vec<String> = sqlx::query_scalar(search)
        .bind("flamingo")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert!(missing.is_empty());
});

db_test!(newest_message_breaks_a_timestamp_tie_on_the_item_id, |f| {
    seed(&f.pool).await;
    store::insert_messages(
        &f.pool,
        &[
            msg("i-1", 0),
            // Same instant as i-3: Instagram can and does send two items on one second.
            msg("i-2", 10),
            msg("i-3", 10),
        ],
    )
    .await
    .unwrap();
    let message = store::newest_message(&f.pool, ME, "T1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.item_id, "i-3");
});

db_test!(newest_message_of_an_empty_thread_is_none, |f| {
    seed(&f.pool).await;
    assert!(
        store::newest_message(&f.pool, ME, "T1")
            .await
            .unwrap()
            .is_none()
    );
});

db_test!(newest_message_does_not_reach_into_another_thread, |f| {
    seed(&f.pool).await;
    store::upsert_thread(&f.pool, &listing("T2", ME))
        .await
        .unwrap();
    store::insert_messages(
        &f.pool,
        &[MessageRecord {
            thread_id: "T2".to_owned(),
            ..msg("a", 99)
        }],
    )
    .await
    .unwrap();
    assert!(
        store::newest_message(&f.pool, ME, "T1")
            .await
            .unwrap()
            .is_none()
    );
});

db_test!(set_watermark_records_the_newest_item_seen, |f| {
    seed(&f.pool).await;
    store::set_watermark(&f.pool, ME, "T1", "i-1")
        .await
        .unwrap();
    store::set_watermark(&f.pool, ME, "T1", "i-2")
        .await
        .unwrap();
    assert_eq!(
        thread(&f.pool, "T1").await.watermark_item_id.as_deref(),
        Some("i-2")
    );
});

// ------------------------------------------------------------------------ sync_state

db_test!(record_sync_upserts_one_row_per_loop, |f| {
    seed(&f.pool).await;
    store::record_sync(&f.pool, ME, "poll", Some("T1"), None)
        .await
        .unwrap();
    store::record_sync(&f.pool, ME, "backfill", None, None)
        .await
        .unwrap();
    store::record_sync(&f.pool, ME, "poll", Some("T2"), None)
        .await
        .unwrap();

    let rows: Vec<(
        String,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> =
        sqlx::query_as("SELECT loop, current_thread_id, last_run_at FROM sync_state ORDER BY loop")
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["backfill", "poll"]
    );
    assert_eq!(rows[1].1.as_deref(), Some("T2"));
    assert!(rows[1].2.is_some());
});

db_test!(
    record_sync_clears_an_error_the_previous_run_left_behind,
    |f| {
        // A stale error that never clears makes the status tool report a dead outage.
        seed(&f.pool).await;
        let last_error = "SELECT last_error, last_error_at FROM sync_state";
        store::record_sync(&f.pool, ME, "poll", None, Some("429 from /inbox"))
            .await
            .unwrap();
        let (error, at): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) =
            sqlx::query_as(last_error).fetch_one(&f.pool).await.unwrap();
        assert_eq!(error.as_deref(), Some("429 from /inbox"));
        assert!(at.is_some());

        store::record_sync(&f.pool, ME, "poll", Some("T1"), None)
            .await
            .unwrap();

        let (error, at): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) =
            sqlx::query_as(last_error).fetch_one(&f.pool).await.unwrap();
        assert!(error.is_none());
        assert!(at.is_none());
    }
);

// ------------------------------------------------------- revisions (test_revisions.py)

db_test!(an_unchanged_reread_writes_nothing, |f| {
    one(&f.pool, "hello").await;
    assert!(
        !store::record_revision(&f.pool, ME, "T1", "i1", Some("hello"))
            .await
            .unwrap()
    );
    assert_eq!(
        count(&f.pool, "SELECT count(*) FROM message_revision").await,
        0
    );
});

db_test!(null_and_empty_text_are_the_same_absence, |f| {
    // Most Instagram items are shares or media with no text at all.
    one(&f.pool, "").await;
    assert!(
        !store::record_revision(&f.pool, ME, "T1", "i1", None)
            .await
            .unwrap()
    );
    assert_eq!(
        count(&f.pool, "SELECT count(*) FROM message_revision").await,
        0
    );
});

db_test!(insert_messages_still_reports_only_genuinely_new_rows, |f| {
    // The number the backfill reads to know it has met itself. If this ever counts
    // updates, the crawl stops recognising ground it has covered.
    one(&f.pool, "hello").await;
    assert_eq!(
        store::insert_messages(&f.pool, &[texted("i1", "different")])
            .await
            .unwrap(),
        0
    );
    let text: String = sqlx::query_scalar("SELECT text FROM message WHERE item_id = 'i1'")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(text, "hello");
});

db_test!(first_edit_writes_both_the_old_and_the_new_text, |f| {
    one(&f.pool, "hello").await;
    assert!(
        store::record_revision(&f.pool, ME, "T1", "i1", Some("hello there"))
            .await
            .unwrap()
    );
    let rows = store::list_revisions(&f.pool, ME, "T1", "i1")
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.seq, r.text.as_deref()))
            .collect::<Vec<_>>(),
        [(1, Some("hello")), (2, Some("hello there"))]
    );
    assert!(rows[0].first_seen_at.is_none());
    assert!(rows[1].superseded_at.is_none());
});

db_test!(exactly_one_current_revision_and_it_matches_the_text, |f| {
    // Asserted in SQL: the invariant every reader depends on.
    one(&f.pool, "hello").await;
    store::record_revision(&f.pool, ME, "T1", "i1", Some("one"))
        .await
        .unwrap();
    store::record_revision(&f.pool, ME, "T1", "i1", Some("two"))
        .await
        .unwrap();
    let (current_rows, current_text, text): (i64, Option<String>, String) = sqlx::query_as(
        "
        SELECT (SELECT count(*) FROM message_revision v
                 WHERE v.account_id = m.account_id AND v.thread_id = m.thread_id
                   AND v.item_id = m.item_id AND v.superseded_at IS NULL) AS current_rows,
               (SELECT v.text FROM message_revision v
                 WHERE v.account_id = m.account_id AND v.thread_id = m.thread_id
                   AND v.item_id = m.item_id AND v.superseded_at IS NULL) AS current_text,
               m.text
          FROM message m WHERE m.item_id = 'i1'
        ",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(current_rows, 1);
    assert_eq!(current_text.as_deref(), Some("two"));
    assert_eq!(text, "two");
});

db_test!(a_message_the_archive_does_not_hold_is_never_an_edit, |f| {
    seed(&f.pool).await;
    assert!(
        !store::record_revision(&f.pool, ME, "T1", "nope", Some("text"))
            .await
            .unwrap()
    );
});

db_test!(concurrent_writers_produce_a_dense_seq, |f| {
    one(&f.pool, "hello").await;
    let (a, b) = tokio::join!(
        store::record_revision(&f.pool, ME, "T1", "i1", Some("a")),
        store::record_revision(&f.pool, ME, "T1", "i1", Some("b")),
    );
    a.unwrap();
    b.unwrap();
    let seqs: Vec<i32> = store::list_revisions(&f.pool, ME, "T1", "i1")
        .await
        .unwrap()
        .iter()
        .map(|r| r.seq)
        .collect();
    assert_eq!(seqs, [1, 2, 3]);
});

db_test!(two_absences_are_needed_to_call_a_message_deleted, |f| {
    one(&f.pool, "hello").await;
    let deleted_at = "SELECT deleted_at FROM message WHERE item_id = 'i1'";
    assert_eq!(
        store::mark_absent(&f.pool, ME, "T1", &ids(&["i1"]))
            .await
            .unwrap(),
        0
    );
    let at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(deleted_at)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(at.is_none());
    assert_eq!(
        store::mark_absent(&f.pool, ME, "T1", &ids(&["i1"]))
            .await
            .unwrap(),
        1
    );
    let at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(deleted_at)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(at.is_some());
});

async fn markers(pool: &PgPool) -> (bool, bool) {
    let (missing, deleted): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as("SELECT missing_since, deleted_at FROM message WHERE item_id = 'i1'")
        .fetch_one(pool)
        .await
        .unwrap();
    (missing.is_some(), deleted.is_some())
}

db_test!(reappearing_withdraws_the_claim, |f| {
    one(&f.pool, "hello").await;
    store::mark_absent(&f.pool, ME, "T1", &ids(&["i1"]))
        .await
        .unwrap();
    store::mark_absent(&f.pool, ME, "T1", &ids(&["i1"]))
        .await
        .unwrap();
    store::mark_present(&f.pool, ME, "T1", &ids(&["i1"]))
        .await
        .unwrap();
    assert_eq!(markers(&f.pool).await, (false, false));
});

db_test!(a_stranded_suspicion_is_released_not_promoted, |f| {
    one(&f.pool, "hello").await;
    store::mark_absent(&f.pool, ME, "T1", &ids(&["i1"]))
        .await
        .unwrap();
    assert_eq!(
        store::release_stranded(&f.pool, ME, "T1", base() + TimeDelta::days(1))
            .await
            .unwrap(),
        1
    );
    assert_eq!(markers(&f.pool).await, (false, false));
});

db_test!(the_reader_sees_the_markers, |f| {
    // `deleted_at` and `edited` reach the read model through the MCP projection.
    one(&f.pool, "hello").await;
    store::insert_messages(&f.pool, &[texted("i2", "second")])
        .await
        .unwrap();
    store::record_revision(&f.pool, ME, "T1", "i1", Some("hello there"))
        .await
        .unwrap();
    store::mark_absent(&f.pool, ME, "T1", &ids(&["i2"]))
        .await
        .unwrap();
    store::mark_absent(&f.pool, ME, "T1", &ids(&["i2"]))
        .await
        .unwrap();
    let page = store::get_history(&f.pool, "T1", None, None, None, 10)
        .await
        .unwrap();
    let by_id = |id: &str| {
        page.messages
            .iter()
            .find(|m| m.item_id == id)
            .expect("in the page")
    };
    assert!(by_id("i1").edited);
    assert!(by_id("i1").deleted_at.is_none());
    assert!(!by_id("i2").edited);
    assert!(by_id("i2").deleted_at.is_some());
});

db_test!(a_busy_thread_still_gets_deletion_detection, |f| {
    // A full page used to skip the deletion pass entirely, which left the busiest
    // threads with no deletion detection at all. What the page returned was observed.
    seed(&f.pool).await;
    store::insert_messages(
        &f.pool,
        &[msg("i.old", 0), msg("i.mid", 3600), msg("i.new", 7200)],
    )
    .await
    .unwrap();
    // The page reached back only as far as i.mid; i.old was never looked at.
    let candidates: HashSet<String> = ids(&["i.old", "i.mid"]).into_iter().collect();
    let covered = store::held_since(
        &f.pool,
        ME,
        "T1",
        base() + TimeDelta::seconds(3600),
        &candidates,
    )
    .await
    .unwrap();
    assert_eq!(covered, HashSet::from(["i.mid".to_owned()]));
});

db_test!(
    a_saved_session_is_loaded_back_and_a_new_one_replaces_it,
    |f| {
        // No account row first: the jar is seeded before any pass has read who it is.
        assert_eq!(store::load_session(&f.pool, ME).await.unwrap(), None);
        let first = r#"[{"name":"sessionid","value":"one","domain":".instagram.com"}]"#;
        store::save_session(&f.pool, ME, first).await.unwrap();
        let loaded = store::load_session(&f.pool, ME).await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&loaded).unwrap(),
            serde_json::from_str::<serde_json::Value>(first).unwrap()
        );

        let second = r#"[{"name":"sessionid","value":"two","domain":".instagram.com"}]"#;
        store::save_session(&f.pool, ME, second).await.unwrap();
        let loaded = store::load_session(&f.pool, ME).await.unwrap().unwrap();
        assert!(
            loaded.contains("two") && !loaded.contains("one"),
            "{loaded}"
        );
        assert_eq!(store::load_session(&f.pool, "other").await.unwrap(), None);
    }
);

db_test!(
    the_backfill_count_adds_up_and_starts_over_on_a_new_day,
    |f| {
        store::upsert_account(&f.pool, &account(ME, None))
            .await
            .unwrap();
        let today = base().date_naive();
        let tomorrow = today + TimeDelta::days(1);
        assert_eq!(store::backfill_spent(&f.pool, ME, today).await.unwrap(), 0);

        store::spend_backfill(&f.pool, ME, today, 30).await.unwrap();
        store::spend_backfill(&f.pool, ME, today, 12).await.unwrap();
        // The loop's own bookkeeping row is the same row, and must not reset the count.
        store::record_sync(&f.pool, ME, "backfill", None, None)
            .await
            .unwrap();
        assert_eq!(store::backfill_spent(&f.pool, ME, today).await.unwrap(), 42);

        assert_eq!(
            store::backfill_spent(&f.pool, ME, tomorrow).await.unwrap(),
            0
        );
        store::spend_backfill(&f.pool, ME, tomorrow, 5)
            .await
            .unwrap();
        assert_eq!(
            store::backfill_spent(&f.pool, ME, tomorrow).await.unwrap(),
            5
        );
    }
);
