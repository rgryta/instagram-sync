//! The read-side store against a real Postgres.
//!
//! Ported from the read half of `tests/test_store.py` and `tests/test_revisions.py`.
//! Every query in `instagram_core::store` runs here, which is what makes the
//! runtime-checked `sqlx::query` safe to prefer over the `query_as!` macro: the macro
//! would need `DATABASE_URL` at build time, and these tests cover what it would check.

use chrono::TimeDelta;
use instagram_core::store::{self, Resolved};

use crate::support::{Conv, DM, Msg, base, db_test, message, minutes, participant, seeded, thread};

/// Two real messages and two rows Instagram wrote itself.
///
/// The boundary between them is the whole point: a count of "messages" is a claim about
/// what the archive holds, so it excludes the rows nobody sent. Both kinds are stored.
async fn seed_with_uncountable_rows(pool: &sqlx::postgres::PgPool) {
    crate::support::account(pool, "me", "1", Some("myself")).await;
    crate::support::participant(pool, "2", Some("stevie"), None).await;
    thread(
        pool,
        Conv {
            thread_id: "T1",
            last_activity_at: Some(base()),
            ..Conv::default()
        },
    )
    .await;
    message(pool, Msg::default()).await;
    message(
        pool,
        Msg {
            item_id: "m2",
            // A shared card is a real thing to send, and carries a payload.
            item_type: "MESSAGE_INLINE_SHARE",
            text: "",
            ts: base() + TimeDelta::seconds(1),
            share_kind: Some("reel"),
            share_owner: Some("surfclub"),
            ..Msg::default()
        },
    )
    .await;
    message(
        pool,
        Msg {
            item_id: "s1",
            item_type: "REACTION_LOG_XMAT",
            text: "Reacted to your message",
            ts: base() + TimeDelta::seconds(2),
            ..Msg::default()
        },
    )
    .await;
    message(
        pool,
        Msg {
            item_id: "s2",
            item_type: "REACTION_LOG_XMAT",
            text: "Liked a message",
            ts: base() + TimeDelta::seconds(3),
            ..Msg::default()
        },
    )
    .await;
}

db_test!(
    the_thread_listing_count_excludes_the_rows_nobody_sent,
    |f| {
        seed_with_uncountable_rows(&f.pool).await;
        let page = store::list_threads(&f.pool, None, None, None, 50)
            .await
            .unwrap();
        assert_eq!(
            page.threads
                .iter()
                .map(|t| t.message_count)
                .collect::<Vec<_>>(),
            [2]
        );
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM message")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(stored, 4, "every row is still stored");
    }
);

db_test!(the_person_count_excludes_them_too, |f| {
    // A person who reacts a hundred times has not sent a hundred messages, and this
    // count is what orders the people listing.
    seed_with_uncountable_rows(&f.pool).await;
    let people = store::find_people(&f.pool, None, 50).await.unwrap();
    assert_eq!(
        people.iter().map(|p| p.message_count).collect::<Vec<_>>(),
        [2]
    );
});

db_test!(status_reports_the_two_uncounted_kinds_apart, |f| {
    // Surfaced rather than hidden, and not merged: an unreadable message is an archive
    // gap worth seeing, a reaction is not. GraphQL has no unreadable `content_type` — an
    // expired story arrives as an ordinary share — so that count is honestly zero.
    seed_with_uncountable_rows(&f.pool).await;
    let rows = store::account_status(&f.pool).await.unwrap();
    assert_eq!(rows[0].messages, 2);
    assert_eq!(rows[0].system_events, 2);
    assert_eq!(rows[0].unreadable, 0);
    assert_eq!(rows[0].shares, 1);
});

db_test!(the_history_range_ignores_everything_uncounted, |f| {
    // The newest stored row is a reaction; "history ends here" must name the last thing
    // someone actually said.
    seed_with_uncountable_rows(&f.pool).await;
    let rows = store::account_status(&f.pool).await.unwrap();
    assert_eq!(
        rows[0].newest_message_at,
        Some(base() + TimeDelta::seconds(1))
    );
    assert_eq!(rows[0].oldest_message_at, Some(base()));
});

db_test!(
    get_history_with_an_after_reads_forward_from_it_and_counts_the_rest,
    |f| {
        // Asking for a period from its start used to return the END of it, with nothing
        // to say the rest existed.
        crate::support::account(&f.pool, "me", "1", None).await;
        thread(&f.pool, Conv::default()).await;
        for n in 1..6 {
            message(
                &f.pool,
                Msg {
                    item_id: &format!("i{n}"),
                    text: &format!("m{n}"),
                    ts: base() + TimeDelta::seconds(n),
                    ..Msg::default()
                },
            )
            .await;
        }
        let forward = store::get_history(&f.pool, "T1", None, Some(base()), None, 2)
            .await
            .unwrap();
        let newest = store::get_history(&f.pool, "T1", None, None, None, 2)
            .await
            .unwrap();

        assert_eq!(item_ids(&forward.messages), ["i1", "i2"]);
        assert_eq!(forward.total_matches, 5);
        assert_eq!(item_ids(&newest.messages), ["i4", "i5"]);
        assert_eq!(newest.total_matches, 5);
    }
);

db_test!(a_page_boundary_breaks_a_timestamp_tie_on_the_item_id, |f| {
    // Instagram ids sort by time in practice but that is an observation, not a contract,
    // and two items can share a timestamp. Without the tiebreak a page can repeat or
    // skip a row.
    crate::support::account(&f.pool, "me", "1", None).await;
    thread(&f.pool, Conv::default()).await;
    for item_id in ["ia", "ib", "ic"] {
        message(
            &f.pool,
            Msg {
                item_id,
                ..Msg::default()
            },
        )
        .await;
    }
    let newest = store::get_history(&f.pool, "T1", None, None, None, 2)
        .await
        .unwrap();
    assert_eq!(item_ids(&newest.messages), ["ib", "ic"]);
});

db_test!(everything_one_person_ever_authored_is_one_query, |f| {
    // The question this archive exists to answer, asked the way the partial index is
    // built — `lower(share_owner)`, because handles are typed by hand at the query end.
    crate::support::account(&f.pool, "me", "1", None).await;
    thread(&f.pool, Conv::default()).await;
    for (item_id, kind, owner, seconds) in [
        ("a", "reel", "Gus.Reyes4", 0),
        ("b", "post", "gus.reyes4", 1),
        ("c", "reel", "someone.else", 2),
    ] {
        message(
            &f.pool,
            Msg {
                item_id,
                share_kind: Some(kind),
                share_owner: Some(owner),
                ts: base() + TimeDelta::seconds(seconds),
                ..Msg::default()
            },
        )
        .await;
    }
    message(
        &f.pool,
        Msg {
            item_id: "d",
            ts: base() + TimeDelta::seconds(3),
            ..Msg::default()
        },
    )
    .await;

    let page = store::list_shares(
        &f.pool,
        None,
        Some("GUS.REYES4"),
        None,
        None,
        None,
        None,
        50,
    )
    .await
    .unwrap();
    let mut found = item_ids(&page.messages);
    found.sort();
    assert_eq!(found, ["a", "b"]);

    let reels = store::list_shares(&f.pool, None, None, Some("reel"), None, None, None, 50)
        .await
        .unwrap();
    assert_eq!(reels.messages.len(), 2);

    let owners = store::share_owners(&f.pool, None, 20).await.unwrap();
    assert_eq!(owners[0].owner, "gus.reyes4", "two spellings, one author");
    assert_eq!(owners[0].shares, 2);
});

db_test!(stored_messages_are_findable_by_full_text_search, |f| {
    // The tsvector is generated by the schema; nothing in the write path fills it.
    crate::support::account(&f.pool, "me", "1", None).await;
    thread(&f.pool, Conv::default()).await;
    message(
        &f.pool,
        Msg {
            item_id: "a",
            text: "dinner at the Ostrich tomorrow",
            ..Msg::default()
        },
    )
    .await;
    let hit = store::search_messages(&f.pool, "ostrich", None, None, None, None, None, None, 25)
        .await
        .unwrap();
    assert_eq!(hit.hits.len(), 1);
    assert_eq!(hit.hits[0].message.item_id, "a");
    assert_eq!(hit.total_matches, 1);

    let miss = store::search_messages(&f.pool, "flamingo", None, None, None, None, None, None, 25)
        .await
        .unwrap();
    assert!(miss.hits.is_empty());

    // An empty query matches no row, which is the honest answer to a search for nothing.
    let nothing = store::search_messages(&f.pool, "   ", None, None, None, None, None, None, 25)
        .await
        .unwrap();
    assert!(nothing.hits.is_empty());
});

db_test!(search_forgives_accents_typos_and_endings, |f| {
    crate::support::account(&f.pool, "me", "1", None).await;
    thread(&f.pool, Conv::default()).await;
    for (item_id, text) in [
        ("s1", "naprawa samochodu jutro"),
        ("s2", "samochód stoi"),
        ("z1", "Widziałem żółwia w zoo"),
        ("p1", "zjemy pierogi dziś?"),
    ] {
        message(
            &f.pool,
            Msg {
                item_id,
                text,
                ..Msg::default()
            },
        )
        .await;
    }
    async fn found(pool: &sqlx::PgPool, query: &str) -> Vec<String> {
        store::search_messages(pool, query, None, None, None, None, None, None, 25)
            .await
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.message.item_id)
            .collect()
    }
    // Accents fold both ways, and an exact word outranks a near one.
    assert_eq!(found(&f.pool, "samochod").await, ["s2", "s1"]);
    assert_eq!(found(&f.pool, "zolw").await, ["z1"]);
    // A typo, and words far apart in either order.
    assert_eq!(found(&f.pool, "pierogki").await, ["p1"]);
    assert_eq!(found(&f.pool, "jutro samochod").await, ["s1"]);
    // Same letters, different word: no match. A quoted phrase stays exact.
    assert!(found(&f.pool, "kot").await.is_empty());
    assert!(found(&f.pool, "\"pierogki\"").await.is_empty());
});

db_test!(
    resolve_people_is_whole_string_where_find_people_is_not,
    |f| {
        // A filter that quietly widened to everyone sharing a first name would return a
        // plausible answer to a question nobody asked.
        seeded(&f.pool).await;
        assert_eq!(store::resolve_people(&f.pool, "zaw").await.unwrap(), []);
        let whole = store::resolve_people(&f.pool, "katarzyna zawadzka")
            .await
            .unwrap();
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0].ig_user_id, "2");
        // A handle and a raw id resolve too.
        assert_eq!(
            store::resolve_people(&f.pool, "K_ZAW_92").await.unwrap()[0].ig_user_id,
            "2"
        );
        assert_eq!(
            store::resolve_people(&f.pool, "2").await.unwrap()[0].ig_user_id,
            "2"
        );
        // And the fragment the other tool takes finds her.
        let found = store::find_people(&f.pool, Some("zaw"), 50).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].dm_thread_id.as_deref(), Some(DM));
    }
);

db_test!(people_match_without_accents_and_with_them, |f| {
    // Names get typed without their diacritics, and stored names sometimes lack them.
    seeded(&f.pool).await;
    participant(&f.pool, "11", Some("m_zaz"), Some("Michał Zażółcki")).await;
    participant(&f.pool, "12", Some("t_nowe"), Some("Michal Testowy")).await;
    participant(&f.pool, "13", None, Some("Łukasz Próbny")).await;
    let ids = |people: Vec<instagram_core::models::Person>| -> Vec<String> {
        people.into_iter().map(|p| p.ig_user_id).collect()
    };

    let found = store::find_people(&f.pool, Some("Michal Z"), 50)
        .await
        .unwrap();
    assert_eq!(ids(found), ["11"]);
    let mut found = ids(store::find_people(&f.pool, Some("michał"), 50)
        .await
        .unwrap());
    found.sort();
    assert_eq!(found, ["11", "12"]);
    for typed in ["ŁUKASZ", "lukasz", "Łukasz"] {
        let found = store::find_people(&f.pool, Some(typed), 50).await.unwrap();
        assert_eq!(ids(found), ["13"], "{typed}");
    }

    let whole = store::resolve_people(&f.pool, "Michal Zazolcki")
        .await
        .unwrap();
    assert_eq!(whole.len(), 1);
    assert_eq!(whole[0].ig_user_id, "11");
    let exact = store::resolve_people(&f.pool, "Michał Zażółcki")
        .await
        .unwrap();
    assert_eq!(exact, whole);
    let reverse = store::resolve_people(&f.pool, "MICHAŁ TESTOWY")
        .await
        .unwrap();
    assert_eq!(reverse[0].ig_user_id, "12");

    store::set_alias(
        &f.pool,
        "Paweł",
        &[instagram_core::models::Identity {
            account_id: "me".into(),
            ig_user_id: "13".into(),
        }],
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        store::resolve_people(&f.pool, "pawel").await.unwrap()[0].ig_user_id,
        "13"
    );
    assert_eq!(
        ids(store::find_people(&f.pool, Some("PAWE"), 50).await.unwrap()),
        ["13"]
    );
});

db_test!(an_alias_resolves_before_a_participant_name_does, |f| {
    seeded(&f.pool).await;
    store::set_alias(
        &f.pool,
        "olek",
        &[instagram_core::models::Identity {
            account_id: "me".into(),
            ig_user_id: "2".into(),
        }],
        None,
    )
    .await
    .unwrap();
    // `olek` is also participant 3's handle. The alias wins, and does not widen to both.
    let resolved = store::resolve_people(&f.pool, "olek").await.unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].ig_user_id, "2");
});

db_test!(an_alias_listing_carries_its_members_and_its_note, |f| {
    seeded(&f.pool).await;
    let members = vec![
        instagram_core::models::Identity {
            account_id: "me".into(),
            ig_user_id: "2".into(),
        },
        instagram_core::models::Identity {
            account_id: "me".into(),
            ig_user_id: "3".into(),
        },
    ];
    store::set_alias(&f.pool, "padel", &members, Some("the regulars"))
        .await
        .unwrap();
    let aliases = store::list_aliases(&f.pool, None).await.unwrap();
    assert_eq!(aliases.len(), 1);
    assert_eq!(aliases[0].note.as_deref(), Some("the regulars"));
    assert_eq!(aliases[0].members, members);

    // A note is kept when the next write does not carry one.
    store::set_alias(&f.pool, "padel", &members[..1], None)
        .await
        .unwrap();
    let aliases = store::list_aliases(&f.pool, None).await.unwrap();
    assert_eq!(aliases[0].note.as_deref(), Some("the regulars"));
    assert_eq!(aliases[0].members.len(), 1, "the member set is replaced");
});

db_test!(deleting_an_alias_reports_whether_there_was_one, |f| {
    seeded(&f.pool).await;
    assert!(!store::delete_alias(&f.pool, "nope").await.unwrap());
    store::set_alias(
        &f.pool,
        "kasia",
        &[instagram_core::models::Identity {
            account_id: "me".into(),
            ig_user_id: "2".into(),
        }],
        None,
    )
    .await
    .unwrap();
    assert!(store::delete_alias(&f.pool, "kasia").await.unwrap());
    // The members go with it, by cascade, and the messages do not.
    assert!(store::list_aliases(&f.pool, None).await.unwrap().is_empty());
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM person_alias_member")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(members, 0);
});

db_test!(resolve_message_separates_unknown_from_not_deleted, |f| {
    // NULL alone cannot carry both answers, and the tool words them differently.
    seeded(&f.pool).await;
    assert_eq!(
        store::resolve_message(&f.pool, "me", DM, "i1")
            .await
            .unwrap(),
        Resolved::Message { deleted_at: None }
    );
    assert_eq!(
        store::resolve_message(&f.pool, "me", DM, "nope")
            .await
            .unwrap(),
        Resolved::NoSuchMessage
    );
    crate::support::mark_deleted(&f.pool, DM, "i1").await;
    assert!(matches!(
        store::resolve_message(&f.pool, "me", DM, "i1")
            .await
            .unwrap(),
        Resolved::Message {
            deleted_at: Some(_)
        }
    ));
});

db_test!(revisions_come_back_in_sequence_order, |f| {
    seeded(&f.pool).await;
    crate::support::revision(&f.pool, DM, "i1", 2, "second", Some(minutes(5)), None).await;
    crate::support::revision(&f.pool, DM, "i1", 1, "first", None, Some(minutes(5))).await;
    let rows = store::list_revisions(&f.pool, "me", DM, "i1")
        .await
        .unwrap();
    assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2]);
    // A NULL `first_seen_at` on seq 1 and only there: nothing records when a message was
    // first stored.
    assert!(rows[0].first_seen_at.is_none());
    assert!(rows[1].first_seen_at.is_some());
    assert!(rows[1].superseded_at.is_none(), "seq 2 is the current text");
});

db_test!(
    a_message_carries_its_reactions_as_objects_not_strings,
    |f| {
        // Tolerating a bare string here is what hid slack-sync's double-encoding bug: the
        // decode undid the extra encode and only SQL noticed.
        seeded(&f.pool).await;
        message(
            &f.pool,
            Msg {
                item_id: "i9",
                thread_id: DM,
                ts: minutes(45),
                reactions: serde_json::json!([{"emoji": "🔥", "sender_id": "2"}]),
                ..Msg::default()
            },
        )
        .await;
        let page = store::get_history(&f.pool, DM, None, None, None, 50)
            .await
            .unwrap();
        let message = page
            .messages
            .iter()
            .find(|m| m.item_id == "i9")
            .expect("the row is there");
        assert_eq!(message.reactions.len(), 1);
        assert_eq!(message.reactions[0].emoji.as_deref(), Some("🔥"));
        assert_eq!(message.reactions[0].sender_id.as_deref(), Some("2"));
    }
);

db_test!(the_directory_loads_every_lookup_the_renderer_needs, |f| {
    seeded(&f.pool).await;
    let directory = store::Directory::load(&f.pool).await.unwrap();
    assert_eq!(directory.people.len(), 4);
    assert_eq!(directory.threads.len(), 2);
    assert!(directory.is_me("me", Some("1")));
    assert!(!directory.is_me("me", Some("2")));
    assert_eq!(
        directory.thread_label("me", DM),
        "DM with Katarzyna Zawadzka"
    );
    assert_eq!(directory.account_of_thread(DM, None).as_deref(), Some("me"));
});

fn item_ids(messages: &[instagram_core::models::Message]) -> Vec<String> {
    messages.iter().map(|m| m.item_id.clone()).collect()
}
