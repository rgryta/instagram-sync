//! Every SQL statement in the project.
//!
//! No other module writes a query. Functions are plain `async fn` over a `PgPool`
//! rather than methods on a type, so tests can call them directly without constructing
//! a service.
//!
//! Split by concern rather than left as the one 1,255-line module the Python original
//! is. The ingester's writes, and the reads only its loops make, are in `ingest.rs`;
//! the one write the MCP role is granted — the two alias tables — lives in `aliases.rs`.
//!
//! Two rules hold throughout. Queries are **runtime-checked** `sqlx::query*`, never the
//! `query_as!` macros: those need a live `DATABASE_URL` at build time or a committed
//! `.sqlx/` cache regenerated on every SQL edit, inside what has to stay a hermetic
//! Docker build — and `tests/archive/store.rs` already runs every query against a real
//! Postgres. And **no caller value is ever interpolated into SQL**: where a direction
//! or a filter has to vary, there are two static query constants rather than one
//! formatted string. sqlx 0.9's `SqlSafeStr` bound makes the second rule a compile
//! error rather than a convention.

// `macro_rules!` is textually scoped, so these have to be declared above the `mod`
// statements that use them. They are the Python module's shared SQL fragments
// (`_MESSAGE_COLUMNS`, `_IS_A_MESSAGE`, `_TOTAL_MATCHES`) in the only form that keeps
// the queries `const`.

/// The columns every MCP-side message read selects, and the only place they are
/// written down.
///
/// `raw` is not among them. It is a whole Instagram payload, kilobytes per row on a
/// reel, and nothing the MCP renders reads it — the Python projection replaces it with
/// `'{}'::jsonb` for exactly that reason, which costs a column to transfer for no
/// reader. `edited` is derived rather than stored: "was this edited" is exactly "is
/// there a history of it", so a column saying so could drift from the rows it
/// describes.
macro_rules! message_columns {
    () => {
        "account_id, thread_id, item_id, ts, sender_id, item_type, text, \
         share_kind, share_owner, share_caption, share_permalink, reply_to_item_id, \
         reactions, deleted_at, transcript, \
         EXISTS (SELECT 1 FROM message_revision v \
                  WHERE v.account_id = message.account_id \
                    AND v.thread_id = message.thread_id \
                    AND v.item_id = message.item_id) AS edited"
    };
}

/// Computed before LIMIT, so it is the size of the whole match set rather than of the
/// page. Every tool that truncates prints it: a page that silently looks complete is
/// how a reader concludes "there are only 12" from a corpus of 400.
macro_rules! total_matches {
    () => {
        "count(*) OVER () AS total_matches"
    };
}

/// Written against the alias `m`, which each query that uses it supplies.
///
/// The arrays are literals rather than built from [`SYSTEM_ITEM_TYPES`] at runtime,
/// because a query assembled from values is a query sqlx 0.9 will not accept. A unit
/// test below pins the two together, so editing one and not the other fails the suite.
macro_rules! is_a_message {
    () => {
        "m.item_type <> ALL ('{REACTION_LOG_XMAT}'::text[])"
    };
}

macro_rules! is_system {
    () => {
        "m.item_type = ANY ('{REACTION_LOG_XMAT}'::text[])"
    };
}

macro_rules! is_unreadable {
    () => {
        "m.item_type = ANY ('{}'::text[])"
    };
}

/// The accent-folding table, as literals so `folded!` can splice it into a `const` query.
/// One char in, one char out, lowercase only: `lower()` runs first. [`FOLD_FROM`] and
/// [`FOLD_TO`] are the same strings for [`fold`], the Rust half of the comparison.
macro_rules! fold_from {
    () => {
        "ąćęłńóśźżáàâäãåéèêëěíìîïòôöõőúùûüůűýÿñňçčšžřďť"
    };
}

macro_rules! fold_to {
    () => {
        "acelnoszzaaaaaaeeeeeiiiiooooouuuuuuyynnccszrdt"
    };
}

/// A column folded for an accent- and case-insensitive comparison against [`fold`]ed
/// input. It defeats any index on the column; the people tables are small enough not to care.
macro_rules! folded {
    ($column:literal) => {
        concat!(
            "translate(lower(",
            $column,
            "), '",
            fold_from!(),
            "', '",
            fold_to!(),
            "')"
        )
    };
}

mod accounts;
mod aliases;
mod directory;
mod ingest;
mod messages;
mod people;
mod threads;
mod voice;

pub use accounts::{account_status, sync_loops};
pub use aliases::{delete_alias, list_aliases, set_alias};
pub use directory::{Directory, DirectoryAccount, DirectoryPerson, DirectoryThread};
pub use ingest::{
    backfill_spent, clear_auth_error, get_thread, held_since, insert_messages, load_session,
    mark_absent, mark_polled, mark_present, newest_message, oldest_item_id, record_auth_error,
    record_revision, record_sync, release_stranded, save_session, set_backfill, set_watermark,
    spend_backfill, stored_window, text_differs, threads_needing_backfill, upsert_account,
    upsert_participants, upsert_thread,
};
pub use messages::{
    MessagePage, Resolved, SearchHit, SearchPage, get_history, list_revisions, list_shares,
    resolve_message, search_messages, share_owners,
};
pub use people::{find_people, resolve_people};
pub use threads::{ThreadPage, list_threads};
pub use voice::{
    ExpiredVoice, WaitingVoice, expired_voice, give_up_voice, relink_voice, set_transcript,
    waiting_voice,
};

/// Accented letters [`fold`] and `folded!` map to plain ones, char for char.
pub const FOLD_FROM: &str = fold_from!();
/// What each char of [`FOLD_FROM`] becomes.
pub const FOLD_TO: &str = fold_to!();

/// Lowercase, then strip accents, so "Michał", "MICHAL" and "michal" all compare equal.
///
/// The Rust half of a person-name match; the stored side goes through `folded!`.
pub fn fold(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match FOLD_FROM.chars().position(|from| from == c) {
            Some(i) => FOLD_TO
                .chars()
                .nth(i)
                .expect("the fold table is one-to-one"),
            None => c,
        })
        .collect()
}

/// Two kinds of row are stored but are not messages, for two different reasons, and the
/// rows themselves are kept in both cases — the ingester stays a faithful mirror, and a
/// thread read without them loses who reacted to what.
///
/// Nobody sent these. Instagram files a row for a reaction ("Reacted 😂 to your
/// message"). These are `content_type` values from `/api/graphql` — UPPERCASE, and a
/// different vocabulary from the retired `direct_v2` REST names (`action_log`,
/// `video_call_event`) that stood here until 2026-11-12. Those matched nothing, so
/// reaction lines counted as real messages and the system-event total was permanently
/// zero. Of the seven measured values this is the only one nobody sent.
pub const SYSTEM_ITEM_TYPES: [&str; 1] = ["REACTION_LOG_XMAT"];

/// Somebody did send these, and nothing of what they sent survived the trip.
///
/// Empty on purpose. Under `direct_v2` a whole row was typed `placeholder` when
/// Instagram had no content left to give. GraphQL carries that inside the share card
/// instead (an expired story arrives as a normal `MESSAGE_INLINE_SHARE` whose xma says
/// "Story unavailable"), so no `content_type` marks it and nothing can be counted here.
/// Kept as the seam: if a future capture shows such a type, name it here and the counts
/// follow — along with the literal in `is_unreadable!`, which the test below pins to it.
pub const UNREADABLE_ITEM_TYPES: [&str; 0] = [];

/// Every `content_type` measured against the live account, and the canary's reference
/// set. An eighth value is either a new kind of message counted as an ordinary one or a
/// new system line counted as something a person sent; both of those shipped once
/// already. `tests/archive/vocabulary.rs` fails when the live capture holds one.
pub const MEASURED_ITEM_TYPES: [&str; 7] = [
    "TEXT",
    "MESSAGE_INLINE_SHARE",
    "REACTION_LOG_XMAT",
    "IMAGES",
    "MONTAGE_SHARE_XMA",
    "AUDIOS",
    "TEXT_POST_APP_LINK_XMA",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The Python `_types()` helper, kept as a test rather than as runtime string
    /// building: this is the only place the two spellings of the same list can be
    /// compared.
    fn type_array(groups: &[&[&str]]) -> String {
        let names: Vec<&str> = groups.iter().flat_map(|g| g.iter().copied()).collect();
        format!("'{{{}}}'::text[]", names.join(","))
    }

    #[test]
    fn the_sql_type_lists_match_the_constants() {
        assert_eq!(
            is_a_message!(),
            format!(
                "m.item_type <> ALL ({})",
                type_array(&[&SYSTEM_ITEM_TYPES, &UNREADABLE_ITEM_TYPES])
            )
        );
        assert_eq!(
            is_system!(),
            format!("m.item_type = ANY ({})", type_array(&[&SYSTEM_ITEM_TYPES]))
        );
        assert_eq!(
            is_unreadable!(),
            format!(
                "m.item_type = ANY ({})",
                type_array(&[&UNREADABLE_ITEM_TYPES])
            )
        );
    }

    #[test]
    fn the_fold_table_is_one_to_one_and_fold_follows_it() {
        assert_eq!(FOLD_FROM.chars().count(), FOLD_TO.chars().count());
        for (from, to) in FOLD_FROM.chars().zip(FOLD_TO.chars()) {
            assert_eq!(fold(&from.to_string()), to.to_string(), "{from}");
        }
        assert_eq!(fold("ŁUKASZ Michał Śliwa"), "lukasz michal sliwa");
        assert!(folded!("x").starts_with("translate(lower(x), '"));
    }

    #[test]
    fn every_classified_type_is_one_that_was_actually_measured() {
        // A classification over a `content_type` nobody has seen is a guess, and
        // guesses are what the `direct_v2` names were.
        for name in SYSTEM_ITEM_TYPES.iter().chain(UNREADABLE_ITEM_TYPES.iter()) {
            assert!(
                MEASURED_ITEM_TYPES.contains(name),
                "{name} is classified but was never measured"
            );
        }
    }
}
