//! The payload-to-row layer, against anonymised copies of real captures.
//!
//! `thread.graphql.json` and `inbox.graphql.json` are shape-for-shape copies of live
//! replies with the words and names replaced and the identifier lengths preserved, so
//! what is asserted here is what Instagram actually sent: the string milliseconds, the
//! one xma card that carries every kind of share, the `msg_reactions` list that holds no
//! emoji, and the 39-digit key beside the 16-digit handle. Because the values are
//! filler, the assertions are about relationships rather than exact words.
//!
//! Two of the seven content types are missing from the fixture (`AUDIOS` and
//! `TEXT_POST_APP_LINK_XMA` appeared once each in the whole capture). Those are inline
//! below, key-for-key in the captured shape with synthetic values.

use std::collections::BTreeSet;

use chrono::{TimeZone, Utc};
use instagram_ingest::normalize::{self, NormalizeError, Share};
use serde_json::{Value, json};

const ACCOUNT: &str = "acct";

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/../../tests/fixtures/instagram/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).expect("the fixture is JSON")
}

fn thread() -> Value {
    fixture("thread.graphql.json")["data"]["get_slide_thread_nullable"]["as_ig_direct_thread"]
        .take()
}

fn inbox_thread() -> Value {
    fixture("inbox.graphql.json")["data"]["get_slide_mailbox_for_iris_subscription"]
        ["threads_by_folder"]["edges"][0]["node"]["as_ig_direct_thread"]
        .take()
}

fn nodes_of(raw: &Value) -> Vec<Value> {
    raw["slide_messages"]["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .filter(|e| !e["node"].is_null())
        .map(|e| e["node"].clone())
        .collect()
}

fn nodes() -> Vec<Value> {
    nodes_of(&thread())
}

fn of_type(content_type: &str) -> Value {
    nodes()
        .into_iter()
        .find(|n| n["content_type"] == content_type)
        .unwrap_or_else(|| panic!("no {content_type} in the fixture"))
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A voice note. One in the entire capture, so it is not in the fixture.
fn audio_node() -> Value {
    json!({
        "id": "m000000000000000000000000000000001",
        "message_id": "m000000000000000000000000000000001",
        "content_type": "AUDIOS",
        "timestamp_ms": "1786767875160",
        "text_body": "",
        "sender": {"id": "100000000000101", "igid": "100000000000001"},
        "content": {
            "__typename": "SlideMessageAudiosContent",
            "audio_attachments": [{
                "attachment_fbid": "100000000000201",
                "waveform_data": [0, 0.25, 0.75, 0],
                "playable_duration_ms": 4000,
                "attachment_cdn_url": "https://example.com/audio",
            }],
        },
        "msg_reactions": [],
        "slide_edit_history": [],
        "replied_to_message_id": null,
    })
}

/// A Threads link. Also one in the whole capture, and the only type whose card carries a
/// subtitle worth reading.
fn link_node() -> Value {
    json!({
        "id": "m000000000000000000000000000000002",
        "message_id": "m000000000000000000000000000000002",
        "content_type": "TEXT_POST_APP_LINK_XMA",
        "timestamp_ms": "1786785671771",
        "text_body": "",
        "sender": {"id": "100000000000102", "igid": "100000000000002"},
        "content": {
            "__typename": "SlideMessageXMAContent",
            "xma_text_body": "",
            "xma": {
                "__typename": "SlideMessageStandardXMA",
                "caption_body_text": "alpha bravo charlie delta",
                "title_text": "",
                "header_title_text": "example handle",
                "subtitle_text": "echo foxtrot golf",
                "header_subtitle_text": null,
                "target_url": "https://example.com/link",
                "target_id": null,
            },
        },
        "msg_reactions": [],
        "slide_edit_history": [],
        "replied_to_message_id": null,
    })
}

/// `{**base, key: value}`.
fn with(mut base: Value, key: &str, value: Value) -> Value {
    base[key] = value;
    base
}

fn aug_14() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 14, 17, 25, 37).unwrap() + chrono::TimeDelta::milliseconds(430)
}

// ------------------------------------------------------------------------- timestamps

#[test]
fn a_timestamp_is_read_as_milliseconds() {
    assert_eq!(
        normalize::parse_timestamp(&json!("1786728337430")).unwrap(),
        aug_14()
    );
}

#[test]
fn a_parsed_timestamp_is_timezone_aware_and_utc() {
    // The type says UTC; this pins that the instant is the UTC one and not a local one.
    let ts = normalize::parse_timestamp(&json!("1786728337430")).unwrap();
    assert_eq!(ts.timestamp_millis(), 1_786_728_337_430);
    assert_eq!(ts.to_rfc3339(), "2026-08-14T17:25:37.430+00:00");
}

/// The whole reason the unit is a guarded range and not a cast. A microsecond value is a
/// valid integer and lands 56000 years in the future; nothing downstream would notice.
#[test]
fn a_microsecond_timestamp_is_rejected_rather_than_read_as_the_year_58000() {
    let err = normalize::parse_timestamp(&json!(1_786_728_337_430_384_i64)).unwrap_err();
    assert!(matches!(err, NormalizeError::NotMilliseconds(_)), "{err}");
    assert!(err.to_string().contains("milliseconds"));
}

#[test]
fn a_second_precision_timestamp_is_rejected_too() {
    let err = normalize::parse_timestamp(&json!(1_786_728_337)).unwrap_err();
    assert!(matches!(err, NormalizeError::NotMilliseconds(_)), "{err}");
}

/// Instagram sends these quoted, and nothing should depend on that holding.
#[test]
fn a_timestamp_reads_the_same_whether_it_arrives_quoted_or_not() {
    assert_eq!(
        normalize::parse_timestamp(&json!("1786728337430")),
        normalize::parse_timestamp(&json!(1_786_728_337_430_i64))
    );
}

#[test]
fn a_timestamp_that_cannot_be_read_raises_rather_than_defaulting_to_now() {
    for value in [json!(null), json!(""), json!("later"), json!({})] {
        let err = normalize::parse_timestamp(&value).unwrap_err();
        assert!(matches!(err, NormalizeError::NotATimestamp(_)), "{value}");
        assert!(err.to_string().contains("Instagram timestamp"));
    }
}

// --------------------------------------------------------------- account and people

#[test]
fn the_account_comes_from_the_threads_viewer() {
    let raw = thread();
    let account = normalize::account_from_viewer(&raw["viewer"], ACCOUNT);

    assert_eq!(account.id, ACCOUNT);
    assert_eq!(account.ig_user_id, text_of(&raw["viewer"]["id"]));
    assert_eq!(account.ig_user_id, text_of(&raw["viewer_id"]));
}

/// The inbox's `viewer` holds an id and no username, and that is the object the startup
/// path has. The id is what the archive keys on, so a missing handle is cosmetic.
#[test]
fn an_account_whose_viewer_carries_no_handle_still_identifies_itself() {
    let account = normalize::account_from_viewer(&json!({"id": "100000000000001"}), ACCOUNT);

    assert_eq!(account.ig_user_id, "100000000000001");
    assert_eq!(account.username, None);
}

#[test]
fn participants_are_parsed_from_the_users_array() {
    let raw = thread();
    let people = normalize::participants_from_thread(&raw, ACCOUNT);

    assert!(!people.is_empty());
    let got: BTreeSet<String> = people.into_iter().map(|p| p.ig_user_id).collect();
    let want: BTreeSet<String> = raw["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| text_of(&u["id"]))
        .collect();
    assert_eq!(got, want);
}

/// The listing is the shape the poll upserts from, and it omits `pk`. Asserting only
/// against the thread query let a parser that read `pk` alone look correct while it
/// returned nobody from every listing. The expected ids come from `user_id` rather than
/// a literal key, so this cannot go back to comparing two empty sets.
#[test]
fn participants_are_parsed_from_an_inbox_listing_too() {
    let raw = inbox_thread();
    let expected: Vec<Option<String>> = raw["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(normalize::user_id)
        .collect();
    assert!(!expected.is_empty() && expected.iter().all(Option::is_some));
    let expected: BTreeSet<String> = expected.into_iter().flatten().collect();

    let people = normalize::participants_from_thread(&raw, ACCOUNT);
    let got: BTreeSet<String> = people.into_iter().map(|p| p.ig_user_id).collect();
    assert_eq!(got, expected);

    // And the thread row keeps them, next to the viewer.
    let row = normalize::thread_from_payload(&raw, ACCOUNT).unwrap();
    let ids: BTreeSet<String> = row.participant_ids.into_iter().collect();
    let mut want = expected;
    want.insert(text_of(&raw["viewer_id"]));
    assert_eq!(ids, want);
}

/// Which is why the account's own row comes from `viewer`: the viewer is not in `users`.
#[test]
fn the_users_array_holds_everyone_but_the_viewer() {
    let raw = thread();
    let viewer = text_of(&raw["viewer_id"]);
    assert!(
        raw["users"]
            .as_array()
            .unwrap()
            .iter()
            .all(|u| text_of(&u["id"]) != viewer)
    );
}

#[test]
fn a_user_without_an_id_is_skipped_rather_than_stored_under_an_empty_id() {
    let mut raw = thread();
    raw["users"]
        .as_array_mut()
        .unwrap()
        .push(json!({"username": "nobody"}));

    let people = normalize::participants_from_thread(&raw, ACCOUNT);

    assert!(people.iter().all(|p| !p.ig_user_id.is_empty()));
    assert_eq!(people.len(), raw["users"].as_array().unwrap().len() - 1);
}

// ------------------------------------------------------------------------- the thread

#[test]
fn the_title_comes_from_thread_title() {
    let raw = with(thread(), "thread_title", json!("the title"));
    assert_eq!(
        normalize::thread_from_payload(&raw, ACCOUNT)
            .unwrap()
            .title
            .as_deref(),
        Some("the title")
    );
}

#[test]
fn a_thread_records_its_group_flag() {
    let raw = with(thread(), "is_group", json!(true));
    assert!(
        normalize::thread_from_payload(&raw, ACCOUNT)
            .unwrap()
            .is_group
    );
}

#[test]
fn a_threads_last_activity_is_read_in_milliseconds_too() {
    let raw = with(
        thread(),
        "last_activity_timestamp_ms",
        json!("1786728337430"),
    );

    let row = normalize::thread_from_payload(&raw, ACCOUNT).unwrap();

    assert_eq!(row.last_activity_at, Some(aug_14()));
}

#[test]
fn a_thread_with_no_activity_yet_gets_none_rather_than_the_epoch() {
    let raw = with(thread(), "last_activity_timestamp_ms", Value::Null);
    assert_eq!(
        normalize::thread_from_payload(&raw, ACCOUNT)
            .unwrap()
            .last_activity_at,
        None
    );
}

/// Without it a 1:1 thread lists one member while carrying messages from two.
#[test]
fn the_participant_ids_include_the_viewer() {
    let raw = thread();

    let row = normalize::thread_from_payload(&raw, ACCOUNT).unwrap();

    assert!(row.participant_ids.contains(&text_of(&raw["viewer_id"])));
}

/// One parser, because the inbox hands back the identical node shape.
#[test]
fn the_inbox_and_the_thread_parse_to_the_same_kind_of_object() {
    let listed = normalize::thread_from_payload(&inbox_thread(), ACCOUNT).unwrap();
    let fetched = normalize::thread_from_payload(&thread(), ACCOUNT).unwrap();

    assert_eq!(listed.thread_id.len(), 39);
    assert_eq!(fetched.thread_id.len(), 39);
}

/// Two ids, both on every thread object, not interchangeable: 39 digits is the archive's
/// key and 16 is what the query takes. Passing one for the other gets no data, no error.
#[test]
fn the_handle_that_addresses_a_thread_is_not_the_key_that_stores_it() {
    let raw = thread();

    let handle = normalize::thread_handle(&raw).unwrap();

    assert_eq!(handle, text_of(&raw["thread_fbid"]));
    assert_eq!(handle.len(), 16);
    assert_eq!(text_of(&raw["thread_id"]).len(), 39);
    assert_ne!(handle, text_of(&raw["thread_id"]));
}

// ----------------------------------------------------------------------- the messages

/// They arrive newest first, and are reversed so an insert dying halfway leaves a
/// contiguous run next to where the following page starts.
#[test]
fn messages_are_reversed_to_oldest_first() {
    let messages = normalize::messages_from_thread(&thread(), ACCOUNT).unwrap();

    assert!(messages.windows(2).all(|w| w[0].ts <= w[1].ts));
    assert_eq!(
        messages[0].item_id,
        text_of(&nodes().last().unwrap()["message_id"])
    );
}

#[test]
fn every_content_type_in_the_capture_normalises_without_raising() {
    let messages = normalize::messages_from_thread(&thread(), ACCOUNT).unwrap();

    assert_eq!(messages.len(), nodes().len());
    let got: BTreeSet<String> = messages.into_iter().map(|m| m.item_type).collect();
    let want: BTreeSet<String> = nodes()
        .iter()
        .map(|n| text_of(&n["content_type"]))
        .collect();
    assert_eq!(got, want);
}

/// `sender.igid` and not `sender_fbid`: only the former joins to `participant`. A
/// silent mismatch here would leave every message attributed to nobody.
#[test]
fn every_message_resolves_to_a_sender_in_the_thread() {
    let raw = thread();
    let mut members: BTreeSet<String> = raw["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| text_of(&u["id"]))
        .collect();
    members.insert(text_of(&raw["viewer_id"]));
    assert!(members.len() > 1);

    let messages = normalize::messages_from_thread(&raw, ACCOUNT).unwrap();

    for m in &messages {
        let sender = m.sender_id.as_ref().expect("every message has a sender");
        assert!(members.contains(sender), "{sender} is not in the thread");
    }
}

/// Relay models a connection edge as nullable, and a thread is not worth losing to one
/// hole in it.
#[test]
fn an_edge_with_no_node_is_skipped_rather_than_losing_the_page() {
    let mut raw = thread();
    raw["slide_messages"]["edges"]
        .as_array_mut()
        .unwrap()
        .push(json!({"node": null}));

    assert_eq!(
        normalize::messages_from_thread(&raw, ACCOUNT)
            .unwrap()
            .len(),
        nodes().len()
    );
}

/// The inbox embeds a preview of recent messages: no `sender.igid`, no reactions, no
/// reply id. They would parse, with a null sender on every row, and degraded rows are
/// worse than none because they look archived.
#[test]
fn the_inbox_preview_messages_are_refused_rather_than_archived() {
    let err = normalize::messages_from_thread(&inbox_thread(), ACCOUNT).unwrap_err();
    assert!(matches!(err, NormalizeError::PreviewMessages(_)), "{err}");
    assert!(err.to_string().contains("preview messages"));
}

/// `raw` is what a later migration projects new columns out of.
#[test]
fn raw_is_the_node_verbatim() {
    let node = of_type("TEXT");
    let messages = normalize::messages_from_thread(&thread(), ACCOUNT).unwrap();

    let stored = messages
        .iter()
        .find(|m| m.item_id == text_of(&node["message_id"]))
        .unwrap();

    assert_eq!(stored.raw, node);
}

#[test]
fn a_reply_records_the_item_it_answers() {
    let raw = thread();
    let answering = nodes_of(&raw)
        .into_iter()
        .find(|n| matches!(&n["replied_to_message_id"], Value::String(s) if !s.is_empty()))
        .expect("a reply in the fixture");

    let messages = normalize::messages_from_thread(&raw, ACCOUNT).unwrap();
    let stored = messages
        .iter()
        .find(|m| m.item_id == text_of(&answering["message_id"]))
        .unwrap();

    assert_eq!(
        stored.reply_to_item_id,
        Some(text_of(&answering["replied_to_message_id"]))
    );
}

#[test]
fn a_message_nobody_replied_to_has_no_reply_pointer() {
    let stored = normalize::message(&audio_node(), ACCOUNT, "T").unwrap();
    assert_eq!(stored.reply_to_item_id, None);
}

/// The emoji is gone and is not recoverable: `msg_reactions` carries only
/// `{sender_fbid, sender_igid}`, and REST sent the character.
#[test]
fn reactions_record_who_but_never_what_with() {
    let raw = thread();
    let reacted = nodes_of(&raw)
        .into_iter()
        .find(|n| n["msg_reactions"].as_array().is_some_and(|r| !r.is_empty()))
        .expect("a reacted message in the fixture");

    let messages = normalize::messages_from_thread(&raw, ACCOUNT).unwrap();
    let stored = messages
        .iter()
        .find(|m| m.item_id == text_of(&reacted["message_id"]))
        .unwrap();

    assert_eq!(
        &Value::Array(stored.reactions.clone()),
        &reacted["msg_reactions"]
    );
    assert!(stored.reactions.iter().all(|r| r.get("emoji").is_none()));
}

#[test]
fn a_message_with_no_reactions_gets_an_empty_list() {
    let stored = normalize::message(&audio_node(), ACCOUNT, "T").unwrap();
    assert!(stored.reactions.is_empty());
}

/// REST had no concept of an edited DM at all, so this column was always false.
#[test]
fn an_edited_message_is_flagged_and_keeps_its_old_wording_in_raw() {
    let node = with(
        audio_node(),
        "slide_edit_history",
        json!([{"text_body": "what it said before"}]),
    );

    let stored = normalize::message(&node, ACCOUNT, "T").unwrap();

    assert!(stored.edited);
    assert_eq!(
        stored.raw["slide_edit_history"][0]["text_body"],
        "what it said before"
    );
}

// ------------------------------------------------------------------------ message text

#[test]
fn a_text_message_is_its_own_text() {
    let node = of_type("TEXT");
    assert_eq!(normalize::message_text(&node), text_of(&node["text_body"]));
}

/// `header_title_text` (71 of 75) is the author's handle as the client shows it, and
/// `text_body` is empty on 74 of 75, so reading only `text_body` would index a blank.
#[test]
fn a_shared_post_is_indexed_by_the_handle_the_card_renders() {
    let node = of_type("MESSAGE_INLINE_SHARE");
    let header = node["content"]["xma"]["header_title_text"]
        .as_str()
        .expect("the card has a header");

    assert!(normalize::message_text(&node).contains(header));
}

#[test]
fn a_shared_story_is_indexed_by_its_caption() {
    let node = of_type("MONTAGE_SHARE_XMA");

    let text = normalize::message_text(&node);

    if let Some(caption) = node["content"]["xma"]["caption_body_text"].as_str() {
        assert!(text.contains(caption.trim()));
    }
}

#[test]
fn a_threads_link_joins_its_header_subtitle_and_caption() {
    let node = link_node();
    let text = normalize::message_text(&node);
    let xma = &node["content"]["xma"];

    for key in ["header_title_text", "subtitle_text", "caption_body_text"] {
        assert!(text.contains(xma[key].as_str().unwrap()), "{key}");
    }
}

/// It lands in `share_permalink`; a second copy here only makes every share of one post
/// match a query for any of them.
#[test]
fn a_shares_url_stays_out_of_the_text_column() {
    let node = of_type("MESSAGE_INLINE_SHARE");

    if let Some(target) = node["content"]["xma"]["target_url"].as_str() {
        assert!(!normalize::message_text(&node).contains(target));
    }
}

/// `content_type` is its own indexed column and already answers "was this an image".
#[test]
fn a_type_that_carries_nothing_searchable_stores_no_text() {
    for content_type in ["IMAGES", "REACTION_LOG_XMAT"] {
        assert_eq!(
            normalize::message_text(&of_type(content_type)),
            "",
            "{content_type}"
        );
    }
}

#[test]
fn a_voice_note_stores_no_text_because_its_words_are_in_the_audio() {
    assert_eq!(normalize::message_text(&audio_node()), "");
}

/// The type set is Instagram's to extend, and the message still lands with its type and
/// its `raw` intact.
#[test]
fn an_unknown_content_type_is_not_an_error() {
    let node = with(audio_node(), "content_type", json!("SOMETHING_NEW_XMA"));

    let stored = normalize::message(&node, ACCOUNT, "T").unwrap();

    assert_eq!(stored.text, "");
    assert_eq!(stored.item_type, "SOMETHING_NEW_XMA");
    assert_eq!(stored.share_kind, None);
}

// -------------------------------------------------------------------- share projection

#[test]
fn a_share_projects_its_kind_owner_caption_and_permalink() {
    let node = of_type("MESSAGE_INLINE_SHARE");
    let xma = &node["content"]["xma"];

    let share = normalize::share_projection(&node);

    assert_eq!(share.kind.as_deref(), xma["__typename"].as_str());
    assert_eq!(share.owner.as_deref(), xma["header_title_text"].as_str());
    assert_eq!(share.permalink.as_deref(), xma["target_url"].as_str());
}

/// Under REST a reel and a post arrived in differently named keys; one xma card replaced
/// all of them, and `xma.__typename` is the only thing that still distinguishes them.
#[test]
fn the_kind_is_the_cards_typename_because_one_card_carries_every_share() {
    let kinds: Vec<Option<String>> = nodes()
        .iter()
        .filter(|n| {
            n["content_type"] == "MESSAGE_INLINE_SHARE" || n["content_type"] == "MONTAGE_SHARE_XMA"
        })
        .map(|n| normalize::share_projection(n).kind)
        .collect();

    assert!(!kinds.is_empty());
    assert!(kinds.iter().all(Option::is_some));
}

/// Two thirds of the corpus.
#[test]
fn a_message_that_shares_nothing_projects_all_four_columns_as_null() {
    assert_eq!(
        normalize::share_projection(&of_type("TEXT")),
        Share::default()
    );
}

#[test]
fn a_share_with_no_card_projects_nothing_rather_than_half_a_row() {
    let node = with(
        link_node(),
        "content",
        json!({"__typename": "SlideMessageXMAContent"}),
    );

    assert_eq!(normalize::share_projection(&node), Share::default());
}

#[test]
fn the_caption_prefers_the_posts_own_words_over_its_title() {
    let node = with(
        link_node(),
        "content",
        json!({"xma": {
            "__typename": "SlideMessageStandardXMA",
            "caption_body_text": "what the author wrote",
            "title_text": "a title",
            "header_title_text": "someone",
        }}),
    );

    assert_eq!(
        normalize::share_projection(&node).caption.as_deref(),
        Some("what the author wrote")
    );
}

/// The same URL shared twice stores as two different strings while wrapped, and neither
/// is the page.
#[test]
fn a_redirect_wrapped_link_is_stored_as_the_page_it_points_at() {
    let node = with(
        link_node(),
        "content",
        json!({"xma": {
            "__typename": "SlideMessageStandardXMA",
            "target_url": "https://l.instagram.com/?u=https%3A%2F%2Fexample.org%2Fa&e=tok",
        }}),
    );

    assert_eq!(
        normalize::share_projection(&node).permalink.as_deref(),
        Some("https://example.org/a")
    );
}

#[test]
fn a_url_that_is_not_wrapped_is_left_exactly_as_it_came() {
    let node = with(
        link_node(),
        "content",
        json!({"xma": {
            "__typename": "SlideMessageStandardXMA",
            "target_url": "https://example.org/plain",
        }}),
    );

    assert_eq!(
        normalize::share_projection(&node).permalink.as_deref(),
        Some("https://example.org/plain")
    );
}

/// Only the redirector itself is unwrapped: another host whose name merely ends the same
/// way keeps its link, query and all.
#[test]
fn a_lookalike_host_is_not_taken_for_the_redirector() {
    let url = "https://mail.instagram.com/?u=https%3A%2F%2Fexample.org%2Fa";
    let node = with(
        link_node(),
        "content",
        json!({"xma": {"__typename": "SlideMessageStandardXMA", "target_url": url}}),
    );

    assert_eq!(
        normalize::share_projection(&node).permalink.as_deref(),
        Some(url)
    );
}

// ------------------------------------------------------------------------- pagination

#[test]
fn the_cursor_for_older_messages_is_the_pages_end_cursor() {
    let mut raw = thread();
    raw["slide_messages"]["page_info"] = json!({"has_next_page": true, "end_cursor": "opaque"});

    assert_eq!(
        normalize::older_page_cursor(&raw).as_deref(),
        Some("opaque")
    );
}

/// `end_cursor` is still a string on the last page, so checking it instead of
/// `has_next_page` would re-fetch that same page forever.
#[test]
fn the_last_page_has_no_cursor_even_though_it_still_carries_one() {
    let mut raw = thread();
    raw["slide_messages"]["page_info"] = json!({"has_next_page": false, "end_cursor": "opaque"});

    assert_eq!(normalize::older_page_cursor(&raw), None);
}

#[test]
fn a_page_that_says_nothing_about_paging_is_treated_as_the_end() {
    let mut raw = thread();
    raw["slide_messages"]["page_info"] = json!({});

    assert_eq!(normalize::older_page_cursor(&raw), None);
}

/// The canary for "Instagram changed the message vocabulary". The store's classification
/// keys off `item_type`: an unmeasured value is either a new kind of message counted as
/// an ordinary one, or a new system line counted as something a person sent. When this
/// fails, measure the new value and decide which group it belongs to.
#[test]
fn the_live_capture_holds_no_content_type_we_have_not_measured() {
    let measured: BTreeSet<&str> = [
        "TEXT",
        "MESSAGE_INLINE_SHARE",
        "REACTION_LOG_XMAT",
        "IMAGES",
        "MONTAGE_SHARE_XMA",
        "AUDIOS",
        "TEXT_POST_APP_LINK_XMA",
    ]
    .into();

    let seen: BTreeSet<String> = normalize::messages_from_thread(&thread(), "me")
        .unwrap()
        .into_iter()
        .map(|m| m.item_type)
        .collect();

    assert!(!seen.is_empty(), "the fixture produced no messages at all");
    let unmeasured: Vec<&String> = seen
        .iter()
        .filter(|t| !measured.contains(t.as_str()))
        .collect();
    assert!(
        unmeasured.is_empty(),
        "unmeasured content_type(s): {unmeasured:?}"
    );
}
