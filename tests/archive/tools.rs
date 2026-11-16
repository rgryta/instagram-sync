//! The eight tools against a seeded database, and against an empty one.
//!
//! Ported from `tests/test_mcp_tools.py`, test for test. The empty-database half is not
//! filler. "No messages matched" is an answer a model can act on; a traceback is not,
//! and a tool that raises on an empty corpus is a tool that breaks on the day the
//! archive is provisioned and before the first poll pass lands.

use instagram_mcp_server::tools::{self, DEFAULT_LIMIT, MAX_LIMIT, ToolError};

use crate::support::{DM, GROUP, Msg, db_test, message, minutes, participant, seeded};

// -------------------------------------------------------------------------- threads

db_test!(threads_names_the_dm_after_the_person_in_it, |f| {
    seeded(&f.pool).await;
    let out = tools::threads(&f.pool, None, None, None, None)
        .await
        .unwrap();
    assert!(out.contains("DM with Katarzyna Zawadzka"), "{out}");
    assert!(out.contains("Padel"), "{out}");
    // The group was never backfilled, and a listing that hides that invites the reader
    // to treat a partial history as the whole one.
    assert!(out.contains("backfill incomplete"), "{out}");
    assert!(out.contains(&format!("[thread {DM}]")), "{out}");
});

db_test!(
    threads_since_narrows_and_counts_only_what_arrived_after_it,
    |f| {
        seeded(&f.pool).await;
        let out = tools::threads(&f.pool, None, Some(&minutes(20).to_rfc3339()), None, None)
            .await
            .unwrap();
        assert!(out.contains("DM with Katarzyna Zawadzka"), "{out}");
        assert!(!out.contains("Padel"), "{out}");
        assert!(out.contains("1 message(s)"), "{out}");
        assert!(
            out.contains("counts cover only what arrived after it"),
            "{out}"
        );
    }
);

db_test!(threads_filters_groups_from_dms, |f| {
    seeded(&f.pool).await;
    let groups = tools::threads(&f.pool, None, None, Some(true), None)
        .await
        .unwrap();
    let dms = tools::threads(&f.pool, None, None, Some(false), None)
        .await
        .unwrap();
    assert!(groups.contains("Padel"), "{groups}");
    assert!(!dms.contains("Padel"), "{dms}");
});

db_test!(threads_on_an_empty_corpus_answers_rather_than_raises, |f| {
    let out = tools::threads(&f.pool, None, None, None, None)
        .await
        .unwrap();
    assert!(out.contains("(no threads matched)"), "{out}");
});

// ------------------------------------------------------------------------- messages

db_test!(messages_renders_a_share_as_content_not_as_an_id, |f| {
    seeded(&f.pool).await;
    let out = tools::messages(&f.pool, DM, None, None, None, None)
        .await
        .unwrap();
    assert!(out.contains("shared a reel by @surfclub"), "{out}");
    assert!(out.contains("https://www.instagram.com/reel/abc/"), "{out}");
    assert!(out.contains("item i2"), "{out}");
});

db_test!(
    messages_is_oldest_first_under_a_header_naming_the_thread,
    |f| {
        seeded(&f.pool).await;
        let out = tools::messages(&f.pool, DM, None, None, None, None)
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("DM with Katarzyna Zawadzka"), "{out}");
        assert!(lines[1].contains("item i1"), "{out}");
        assert!(lines[2].contains("item i2"), "{out}");
    }
);

db_test!(messages_says_when_a_page_is_not_the_whole_thread, |f| {
    seeded(&f.pool).await;
    let out = tools::messages(&f.pool, DM, None, None, None, Some(1))
        .await
        .unwrap();
    assert!(
        out.contains("1 older message(s) match this request"),
        "{out}"
    );
    assert!(out.contains("before="), "{out}");
    // The newest page, not the oldest: opening a conversation on messages from years ago
    // is the wrong answer to "read this thread".
    assert!(out.contains("item i2"), "{out}");
    assert!(!out.contains("item i1"), "{out}");
});

db_test!(
    messages_after_reads_forward_and_says_how_many_more_follow,
    |f| {
        seeded(&f.pool).await;
        let out = tools::messages(
            &f.pool,
            DM,
            None,
            Some(&minutes(-1).to_rfc3339()),
            None,
            Some(1),
        )
        .await
        .unwrap();
        assert!(out.contains("oldest 1 message(s)"), "{out}");
        assert!(out.contains("item i1") && !out.contains("item i2"), "{out}");
        assert!(out.contains("1 more message(s) in this period"), "{out}");
        assert!(out.contains("after="), "{out}");
    }
);

db_test!(messages_windows_with_after_and_before, |f| {
    seeded(&f.pool).await;
    let out = tools::messages(
        &f.pool,
        DM,
        None,
        Some(&minutes(20).to_rfc3339()),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(out.contains("item i2"), "{out}");
    assert!(!out.contains("item i1"), "{out}");
});

db_test!(messages_for_an_unknown_thread_says_nothing_matched, |f| {
    seeded(&f.pool).await;
    let out = tools::messages(&f.pool, "T-nope", None, None, None, None)
        .await
        .unwrap();
    assert!(out.contains("(no messages matched)"), "{out}");
    assert!(out.contains("T-nope"), "{out}");
});

db_test!(messages_accepts_an_epoch_as_readily_as_an_iso_stamp, |f| {
    seeded(&f.pool).await;
    let epoch = format!("{}.0", minutes(20).timestamp());
    let out = tools::messages(&f.pool, DM, None, Some(&epoch), None, None)
        .await
        .unwrap();
    assert!(out.contains("item i2"), "{out}");
});

db_test!(an_unreadable_timestamp_is_a_usage_error, |f| {
    seeded(&f.pool).await;
    let error = tools::messages(&f.pool, DM, None, Some("last tuesday"), None, None)
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Usage(_)), "{error}");
    assert!(error.to_string().contains("could not read"), "{error}");
});

db_test!(a_deleted_message_is_marked_and_its_text_withheld, |f| {
    // `tests/test_revisions.py::test_the_reader_sees_the_markers`, at the tool's end:
    // `deleted_at` and `edited` reach the model through the MCP projection.
    seeded(&f.pool).await;
    crate::support::mark_deleted(&f.pool, DM, "i1").await;
    crate::support::revision(
        &f.pool,
        DM,
        "i2",
        1,
        "@surfclub big waves",
        None,
        Some(minutes(31)),
    )
    .await;
    let out = tools::messages(&f.pool, DM, None, None, None, None)
        .await
        .unwrap();
    assert!(out.contains("[deleted "), "{out}");
    assert!(!out.contains("going sailing tomorrow"), "{out}");
    assert!(out.contains("(edited)"), "{out}");
});

// --------------------------------------------------------------------------- search

db_test!(search_finds_typed_text_and_keys_the_hit, |f| {
    seeded(&f.pool).await;
    let out = search(&f.pool, "sailing").await;
    assert!(out.contains("item i1"), "{out}");
    assert!(out.contains("item i4"), "{out}");
});

db_test!(search_reaches_into_a_reel_caption, |f| {
    // The caption and author handle are indexed with the typed text, so half the corpus
    // is searchable without a single share-specific predicate.
    seeded(&f.pool).await;
    let out = search(&f.pool, "waves").await;
    assert!(out.contains("item i2"), "{out}");
    assert!(out.contains("shared a reel by @surfclub"), "{out}");
});

db_test!(search_narrows_by_sender_through_an_alias, |f| {
    seeded(&f.pool).await;
    tools::set_alias(&f.pool, "Kasia", &["me:2".to_owned()], None)
        .await
        .unwrap();
    let out = tools::search(
        &f.pool,
        "sailing",
        None,
        None,
        Some("Kasia"),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(out.contains("item i1"), "{out}");
    assert!(!out.contains("item i4"), "{out}");
});

db_test!(search_resolves_a_person_typed_without_diacritics, |f| {
    seeded(&f.pool).await;
    // Participant 2 renamed to a name with diacritics; the seeded DM message is theirs.
    sqlx::query("UPDATE participant SET full_name = 'Michał Zażółcki' WHERE ig_user_id = '2'")
        .execute(&f.pool)
        .await
        .unwrap();
    let out = tools::search(
        &f.pool,
        "sailing",
        None,
        None,
        Some("Michal Zazolcki"),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!out.contains("no person or alias matched"), "{out}");
    assert!(out.contains("item i1"), "{out}");
    assert!(!out.contains("item i4"), "{out}");
});

db_test!(people_finds_a_name_typed_without_diacritics, |f| {
    seeded(&f.pool).await;
    participant(&f.pool, "11", Some("m_zaz"), Some("Michał Zażółcki")).await;
    let out = tools::people(&f.pool, Some("Michal"), None).await.unwrap();
    assert!(out.contains("Michał Zażółcki"), "{out}");
});

db_test!(search_narrows_by_share_kind_and_thread, |f| {
    seeded(&f.pool).await;
    let reels = tools::search(
        &f.pool,
        "sailing OR waves",
        None,
        None,
        None,
        Some("reel"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(reels.contains("item i2"), "{reels}");
    let in_dm = tools::search(
        &f.pool,
        "sailing",
        None,
        Some(DM),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!in_dm.contains("item i4"), "{in_dm}");
});

db_test!(
    search_for_an_unknown_person_says_so_instead_of_widening,
    |f| {
        // An unresolvable name must not quietly become "everyone".
        seeded(&f.pool).await;
        let out = tools::search(
            &f.pool,
            "sailing",
            None,
            None,
            Some("nobody at all"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(out.contains("no person or alias matched"), "{out}");
        assert!(!out.contains("item i1"), "{out}");
    }
);

db_test!(search_rejects_a_share_kind_that_does_not_exist, |f| {
    seeded(&f.pool).await;
    let out = tools::search(
        &f.pool,
        "sailing",
        None,
        None,
        None,
        Some("tweet"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(out.contains("unknown share kind"), "{out}");
    assert!(out.contains("reel, post, story, link"), "{out}");
});

db_test!(search_with_no_hits_and_with_no_query, |f| {
    seeded(&f.pool).await;
    assert!(
        search(&f.pool, "kangaroo")
            .await
            .contains("(nothing matched)")
    );
    assert_eq!(
        search(&f.pool, "   ").await,
        "a search needs something to search for"
    );
});

db_test!(search_on_an_empty_corpus_answers_rather_than_raises, |f| {
    assert!(
        search(&f.pool, "sailing")
            .await
            .contains("(nothing matched)")
    );
});

async fn search(pool: &sqlx::postgres::PgPool, query: &str) -> String {
    tools::search(pool, query, None, None, None, None, None, None, None)
        .await
        .unwrap()
}

// --------------------------------------------------------------------------- shares

db_test!(shares_lists_every_kind_with_its_permalink, |f| {
    seeded(&f.pool).await;
    let out = shares(&f.pool).await;
    assert!(out.contains("https://www.instagram.com/reel/abc/"), "{out}");
    assert!(out.contains("https://www.instagram.com/p/xyz/"), "{out}");
    assert!(out.contains("2 share(s)"), "{out}");
    // Typed messages are not shares and must not pad the answer.
    assert!(!out.contains("item i1"), "{out}");
});

db_test!(shares_offers_the_top_authors_when_no_owner_was_named, |f| {
    // `share_owner` is a handle nobody has memorised, so the tool has to hand one over
    // or it only answers questions whose answer the caller already knew.
    seeded(&f.pool).await;
    let out = shares(&f.pool).await;
    assert!(out.contains("most-shared authors"), "{out}");
    assert!(out.contains("@surfclub — 1 share(s)"), "{out}");
});

db_test!(shares_filters_by_author_case_insensitively, |f| {
    seeded(&f.pool).await;
    let out = tools::shares(
        &f.pool,
        Some("newsroom"),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(out.contains("item i4"), "{out}");
    assert!(!out.contains("item i2"), "{out}");
    // Stored capitalised, asked for in lower case, and rendered as the author wrote it.
    assert!(out.contains("by @Newsroom"), "{out}");
    // The owner is known, so the discovery listing is not printed over the answer.
    assert!(!out.contains("most-shared authors"), "{out}");
});

db_test!(shares_filters_by_kind_and_by_thread, |f| {
    seeded(&f.pool).await;
    let reels = tools::shares(&f.pool, None, Some("reel"), None, None, None, None, None)
        .await
        .unwrap();
    assert!(
        reels.contains("item i2") && !reels.contains("item i4"),
        "{reels}"
    );
    let in_group = tools::shares(&f.pool, None, None, Some(GROUP), None, None, None, None)
        .await
        .unwrap();
    assert!(
        in_group.contains("item i4") && !in_group.contains("item i2"),
        "{in_group}"
    );
});

db_test!(shares_filters_by_date, |f| {
    seeded(&f.pool).await;
    let out = tools::shares(
        &f.pool,
        None,
        None,
        None,
        None,
        Some(&minutes(20).to_rfc3339()),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(out.contains("item i2"), "{out}");
    assert!(!out.contains("item i4"), "{out}");
});

db_test!(shares_says_when_a_page_is_capped, |f| {
    seeded(&f.pool).await;
    let out = tools::shares(&f.pool, None, None, None, None, None, None, Some(1))
        .await
        .unwrap();
    assert!(out.contains("showing 1 of 2 match(es)"), "{out}");
});

db_test!(shares_rejects_a_kind_that_does_not_exist, |f| {
    seeded(&f.pool).await;
    let out = tools::shares(&f.pool, None, Some("tweet"), None, None, None, None, None)
        .await
        .unwrap();
    assert!(out.contains("unknown share kind"), "{out}");
});

db_test!(shares_on_an_empty_corpus_answers_rather_than_raises, |f| {
    let out = shares(&f.pool).await;
    assert!(out.contains("(no messages matched)"), "{out}");
    assert!(out.contains("(nobody has shared anything yet)"), "{out}");
});

async fn shares(pool: &sqlx::postgres::PgPool) -> String {
    tools::shares(pool, None, None, None, None, None, None, None)
        .await
        .unwrap()
}

// --------------------------------------------------------------------------- people

db_test!(people_matches_half_a_name_and_carries_the_dm_thread, |f| {
    seeded(&f.pool).await;
    let out = tools::people(&f.pool, Some("zaw"), None).await.unwrap();
    assert!(out.contains("Katarzyna Zawadzka"), "{out}");
    assert!(out.contains("@k_zaw_92"), "{out}");
    assert!(out.contains(&format!("DM thread {DM}")), "{out}");
    assert!(out.contains("[member me:2]"), "{out}");
});

db_test!(people_matches_a_display_name_fragment_too, |f| {
    seeded(&f.pool).await;
    let out = tools::people(&f.pool, Some("katarzyna"), None)
        .await
        .unwrap();
    assert!(out.contains("Katarzyna Zawadzka"), "{out}");
});

db_test!(people_with_no_fragment_lists_everyone, |f| {
    seeded(&f.pool).await;
    let out = tools::people(&f.pool, None, None).await.unwrap();
    assert!(out.contains("4 person/people"), "{out}");
    // Nobody is hidden for lacking a name: the raw id is still a key.
    assert!(out.contains("[member me:4]"), "{out}");
});

db_test!(people_marks_the_account_owner, |f| {
    seeded(&f.pool).await;
    let out = tools::people(&f.pool, Some("myself"), None).await.unwrap();
    assert!(out.contains("this is you"), "{out}");
});

db_test!(people_on_no_match, |f| {
    seeded(&f.pool).await;
    let out = tools::people(&f.pool, Some("nobody at all"), None)
        .await
        .unwrap();
    assert!(out.contains("no matching people"), "{out}");
});

db_test!(people_on_an_empty_corpus_answers_rather_than_raises, |f| {
    let out = tools::people(&f.pool, None, None).await.unwrap();
    assert!(out.contains("no matching people"), "{out}");
});

// ------------------------------------------------------------------------ set-alias

db_test!(
    setting_an_alias_round_trips_into_people_and_into_rendering,
    |f| {
        seeded(&f.pool).await;
        let written = tools::set_alias(&f.pool, "Kasia", &["me:2".to_owned()], Some("padel"))
            .await
            .unwrap();
        // Named as Instagram knows her, not as the alias: otherwise the caller cannot
        // check that the alias landed on the right person.
        assert!(written.contains("Katarzyna Zawadzka"), "{written}");

        let listing = tools::people(&f.pool, Some("zaw"), None).await.unwrap();
        assert!(listing.contains("alias Kasia"), "{listing}");
        assert!(listing.contains("1 alias(es)"), "{listing}");
        assert!(listing.contains("Kasia — padel"), "{listing}");
        // And the alias now outranks her Instagram name everywhere a message is
        // rendered.
        let history = tools::messages(&f.pool, DM, None, None, None, None)
            .await
            .unwrap();
        assert!(history.contains("Kasia:"), "{history}");
    }
);

db_test!(an_alias_can_be_found_by_its_own_name, |f| {
    seeded(&f.pool).await;
    tools::set_alias(&f.pool, "Kasia", &["me:2".to_owned()], None)
        .await
        .unwrap();
    let out = tools::people(&f.pool, Some("kasia"), None).await.unwrap();
    assert!(out.contains("Katarzyna Zawadzka"), "{out}");
});

db_test!(
    moving_a_member_to_another_alias_is_reported_not_silent,
    |f| {
        seeded(&f.pool).await;
        tools::set_alias(&f.pool, "Kasia", &["me:2".to_owned()], None)
            .await
            .unwrap();
        let out = tools::set_alias(&f.pool, "Kate", &["me:2".to_owned()], None)
            .await
            .unwrap();
        assert!(out.contains("moved off another alias: me:2"), "{out}");
    }
);

db_test!(an_alias_with_no_members_is_deleted, |f| {
    seeded(&f.pool).await;
    tools::set_alias(&f.pool, "Kasia", &["me:2".to_owned()], None)
        .await
        .unwrap();
    assert_eq!(
        tools::set_alias(&f.pool, "Kasia", &[], None).await.unwrap(),
        "alias 'Kasia' deleted"
    );
    let listing = tools::people(&f.pool, Some("zaw"), None).await.unwrap();
    assert!(listing.contains("no aliases set"), "{listing}");
    assert_eq!(
        tools::set_alias(&f.pool, "Kasia", &[], None).await.unwrap(),
        "alias 'Kasia' did not exist"
    );
});

db_test!(
    a_malformed_member_is_a_usage_error_not_a_half_written_alias,
    |f| {
        seeded(&f.pool).await;
        let error = tools::set_alias(&f.pool, "Kasia", &["2".to_owned()], None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("account:ig_user_id"), "{error}");
        let listing = tools::people(&f.pool, Some("zaw"), None).await.unwrap();
        assert!(listing.contains("no aliases set"), "{listing}");
    }
);

db_test!(naming_the_same_member_twice_lands_once, |f| {
    // `executemany` tolerates it; a single `ON CONFLICT` statement over an `unnest`
    // would fail with "cannot affect row a second time", which is why the writer loops.
    seeded(&f.pool).await;
    tools::set_alias(
        &f.pool,
        "Kasia",
        &["me:2".to_owned(), "me:2".to_owned()],
        None,
    )
    .await
    .unwrap();
    let listing = tools::people(&f.pool, Some("zaw"), None).await.unwrap();
    assert!(listing.contains("1 alias(es)"), "{listing}");
});

// --------------------------------------------------------------------------- status

db_test!(status_reports_the_corpus_the_backlog_and_the_loops, |f| {
    seeded(&f.pool).await;
    let out = tools::status(&f.pool).await.unwrap();
    assert!(out.contains("4 message(s) of which 2 share(s)"), "{out}");
    assert!(out.contains("1 thread(s) still owing backfill"), "{out}");
    assert!(out.contains("poll loop: last run"), "{out}");
    assert!(out.contains("last working on Padel"), "{out}");
    assert!(out.contains("rate limited"), "{out}");
});

db_test!(
    status_counts_a_reaction_log_row_apart_from_the_messages,
    |f| {
        // The bug this is here for: under `direct_v2`'s names nothing matched, so reaction
        // lines counted as real messages and `system_events` was permanently zero.
        seeded(&f.pool).await;
        message(
            &f.pool,
            Msg {
                item_id: "i5",
                thread_id: DM,
                item_type: "REACTION_LOG_XMAT",
                text: "Liked a message",
                ts: minutes(40),
                ..Msg::default()
            },
        )
        .await;
        let out = tools::status(&f.pool).await.unwrap();
        assert!(out.contains("4 message(s) of which 2 share(s)"), "{out}");
        assert!(out.contains("plus 1 reaction/call event(s)"), "{out}");
        // And it is not counted as a message anywhere else either.
        let threads = tools::threads(&f.pool, None, None, Some(false), None)
            .await
            .unwrap();
        assert!(threads.contains("(DM, 2 message(s)"), "{threads}");
    }
);

db_test!(status_reports_a_dead_session_as_frozen, |f| {
    seeded(&f.pool).await;
    crate::support::record_auth_error(&f.pool, "checkpoint_required").await;
    let out = tools::status(&f.pool).await.unwrap();
    assert!(out.contains("SESSION FAILING"), "{out}");
    assert!(out.contains("checkpoint_required"), "{out}");
});

db_test!(status_before_the_ingester_has_ever_run, |f| {
    let out = tools::status(&f.pool).await.unwrap();
    assert!(out.contains("no account registered yet"), "{out}");
});

// ------------------------------------------------------------------------ revisions

db_test!(revisions_show_one_messages_text_over_time, |f| {
    seeded(&f.pool).await;
    crate::support::revision(&f.pool, DM, "i1", 1, "hello", None, Some(minutes(5))).await;
    crate::support::revision(
        &f.pool,
        DM,
        "i1",
        2,
        "hello there, going sailing tomorrow",
        Some(minutes(5)),
        None,
    )
    .await;
    let out = tools::revisions(&f.pool, DM, "i1", None).await.unwrap();
    assert!(out.contains("2 revision(s) of i1"), "{out}");
    // A NULL start is unknown, not missing.
    assert!(out.contains("unknown ->"), "{out}");
    assert!(out.contains("not necessarily what was first sent"), "{out}");
});

db_test!(revisions_show_the_text_of_a_deleted_message, |f| {
    // The only view that does. Every other one withholds it on purpose.
    seeded(&f.pool).await;
    crate::support::mark_deleted(&f.pool, DM, "i1").await;
    let out = tools::revisions(&f.pool, DM, "i1", None).await.unwrap();
    assert!(out.contains("deleted "), "{out}");
});

db_test!(
    revisions_separate_an_unknown_message_from_an_unchanged_one,
    |f| {
        seeded(&f.pool).await;
        let unchanged = tools::revisions(&f.pool, DM, "i1", None).await.unwrap();
        assert!(unchanged.contains("no recorded changes"), "{unchanged}");
        let unknown = tools::revisions(&f.pool, DM, "nope", None).await.unwrap();
        assert!(unknown.contains("no message nope in thread"), "{unknown}");
        let no_thread = tools::revisions(&f.pool, "T-nope", "i1", None)
            .await
            .unwrap();
        assert!(no_thread.contains("no thread T-nope"), "{no_thread}");
    }
);

// ---------------------------------------------------------------------------- limits

db_test!(the_seed_uses_the_vocabulary_the_live_account_sends, |f| {
    // `tests/test_mcp_tools.py` seeds `item_type="text"`, lower case, which no live
    // `content_type` ever is. It makes no assertion fail there, but it does mean the
    // Python suite never renders a plain message through the path a real one takes —
    // the trap that shipped `[TEXT] hello` lives one test file away. This seed is
    // upper case, so that path is the one under test here.
    seeded(&f.pool).await;
    let out = tools::messages(&f.pool, DM, None, None, None, None)
        .await
        .unwrap();
    assert!(out.contains("hello there, going sailing tomorrow"), "{out}");
    assert!(!out.contains("[TEXT]"), "{out}");
});

#[test]
fn the_limit_ceiling_is_the_one_the_python_uses() {
    assert_eq!(MAX_LIMIT, 200);
    assert_eq!(DEFAULT_LIMIT, 50);
}
