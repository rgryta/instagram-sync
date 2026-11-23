//! Domain models for the read side.
//!
//! These mirror the schema in `db-migrations/pg/0001_init.sql`. `item_id` stays a
//! `String` everywhere for the same reason it is `text` in the schema: it is
//! Instagram's opaque identifier, and `ts` carries the parsed instant.
//!
//! The read rows carry only what the read surface needs, and `raw` in particular is
//! deliberately absent from them: it is a whole Instagram payload, kilobytes per row on
//! a reel, and nothing the MCP renders reads it. The Python projection replaces it with
//! `'{}'::jsonb` for exactly that reason; here the column is simply not selected.
//!
//! The `*Record` types at the end are the write side: what the ingester parses out of a
//! GraphQL payload and stores, field for field what `ingest.normalize` sets on the
//! Python models.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One reaction on one message, out of the `reactions` jsonb array.
///
/// Both fields are optional because the column is Instagram's own shape stored
/// verbatim: a payload that stops carrying `emoji` must render as `?` rather than fail
/// to decode a whole page of history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub sender_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub account_id: String,
    pub thread_id: String,
    pub item_id: String,
    pub ts: DateTime<Utc>,
    pub sender_id: Option<String>,
    /// Instagram's own `content_type`, UPPERCASE, kept as a free string rather than an
    /// enum: the set is theirs to extend, and an unknown type must land in the archive
    /// as itself rather than fail validation or collapse into "other".
    pub item_type: String,
    pub text: String,
    /// Set by the reader, never by the ingester: the source stopped returning a
    /// message we hold, twice running.
    pub deleted_at: Option<DateTime<Utc>>,
    /// Derived, never stored: "was this edited" is exactly "is there a history of it".
    pub edited: bool,
    /// The share projection. `share_owner` is the handle of whoever AUTHORED the reel,
    /// post or story; `sender_id` is the person who forwarded it into the DM, and the
    /// two are almost never the same person.
    pub share_kind: Option<String>,
    pub share_owner: Option<String>,
    pub share_caption: Option<String>,
    pub share_permalink: Option<String>,
    pub reply_to_item_id: Option<String>,
    pub reactions: Vec<Reaction>,
    /// A voice message's words, as whisper heard them; never typed.
    pub transcript: Option<String>,
}

/// One `(account, person)` pair. Alias resolution returns a set of these.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Identity {
    pub account_id: String,
    pub ig_user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    pub alias: String,
    pub note: Option<String>,
    pub members: Vec<Identity>,
}

/// A thread plus the read-side aggregates the `threads` tool shows.
#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub account_id: String,
    pub thread_id: String,
    pub is_group: bool,
    pub last_activity_at: Option<DateTime<Utc>>,
    pub backfill_done: bool,
    /// With `since`, what arrived after that instant; without it, the thread's whole
    /// stored history.
    pub message_count: i64,
}

/// A person plus where to find them.
#[derive(Debug, Clone)]
pub struct Person {
    pub account_id: String,
    pub ig_user_id: String,
    pub username: Option<String>,
    pub full_name: Option<String>,
    pub is_verified: bool,
    /// Every alias naming this person, already joined by the query: showing one
    /// resolved name would hide that two aliases matched.
    pub aliases: Option<String>,
    pub thread_count: i64,
    pub message_count: i64,
    pub dm_thread_id: Option<String>,
}

/// A handle that has authored shares, with how many.
#[derive(Debug, Clone)]
pub struct ShareOwner {
    pub owner: String,
    pub shares: i64,
    pub last_shared_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct AccountStatus {
    pub account_id: String,
    pub username: Option<String>,
    pub enabled: bool,
    pub last_auth_error: Option<String>,
    pub last_auth_error_at: Option<DateTime<Utc>>,
    pub threads: i64,
    pub threads_pending_backfill: i64,
    pub messages: i64,
    /// Rows nobody sent — Instagram files one per reaction. Counted apart from
    /// `messages`, and apart from `unreadable`, because the two mean different things.
    pub system_events: i64,
    /// Rows somebody sent and nothing of which survived the trip. Zero until a
    /// `content_type` is measured that marks one; see [`crate::store::UNREADABLE_ITEM_TYPES`].
    pub unreadable: i64,
    pub shares: i64,
    pub participants: i64,
    pub oldest_message_at: Option<DateTime<Utc>>,
    pub newest_message_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct SyncLoopStatus {
    pub account_id: String,
    /// `poll` or `backfill`; the column's CHECK constraint is the closed set.
    pub loop_name: String,
    pub current_thread_id: Option<String>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
}

/// One row of a message's text over time.
#[derive(Debug, Clone)]
pub struct Revision {
    pub seq: i32,
    pub text: Option<String>,
    /// NULL on seq 1 and only there, meaning unknown: nothing records when a message
    /// was first stored.
    pub first_seen_at: Option<DateTime<Utc>>,
    pub superseded_at: Option<DateTime<Utc>>,
}

/// The logged-in account, off a thread's `viewer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    pub id: String,
    pub ig_user_id: String,
    pub username: Option<String>,
}

/// One person in a thread other than the account itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantRecord {
    pub account_id: String,
    pub ig_user_id: String,
    /// The handle. `full_name` is what the client renders next to a message, and the
    /// two are frequently unrelated; neither substitutes for the other.
    pub username: Option<String>,
    pub full_name: Option<String>,
    pub is_verified: bool,
    pub profile_pic_url: Option<String>,
}

/// One thread as a payload describes it; the sync bookkeeping columns are not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRecord {
    pub account_id: String,
    pub thread_id: String,
    pub title: Option<String>,
    pub is_group: bool,
    pub participant_ids: Vec<String>,
    pub last_activity_at: Option<DateTime<Utc>>,
}

/// One message as the ingester stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    pub account_id: String,
    pub thread_id: String,
    pub item_id: String,
    pub ts: DateTime<Utc>,
    pub sender_id: Option<String>,
    /// Instagram's own `content_type`, kept as a free string: the set is theirs to
    /// extend, and an unknown type must land in the archive as itself.
    pub item_type: String,
    pub text: String,
    /// True when a revision history exists for it.
    pub edited: bool,
    /// The share projection, NULL on everything that shares nothing. `share_owner` is
    /// the handle of whoever AUTHORED the reel, post or story; `sender_id` is the person
    /// who forwarded it into the DM.
    pub share_kind: Option<String>,
    pub share_owner: Option<String>,
    pub share_caption: Option<String>,
    pub share_permalink: Option<String>,
    pub reply_to_item_id: Option<String>,
    /// Instagram's `msg_reactions` entries, verbatim.
    pub reactions: Vec<serde_json::Value>,
    /// The message node verbatim: what a later migration backfills new columns from.
    pub raw: serde_json::Value,
}

/// A stored thread with the crawl state the ingest loops own, which the `*Record`
/// above deliberately lacks: a listing knows nothing about crawl progress.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StoredThread {
    pub account_id: String,
    pub thread_id: String,
    pub title: Option<String>,
    pub is_group: bool,
    pub participant_ids: Vec<String>,
    pub last_activity_at: Option<DateTime<Utc>>,
    pub watermark_item_id: Option<String>,
    pub backfill_cursor: Option<String>,
    pub backfill_done: bool,
    pub last_polled_at: Option<DateTime<Utc>>,
}
