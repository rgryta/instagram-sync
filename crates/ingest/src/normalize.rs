//! Instagram payloads to the rows the storage layer stores.
//!
//! Instagram retired `/api/v1/direct_v2/` (2026-11-02): the path 404s with the generic
//! SPA shell, which reads `not-logged-in` and looks exactly like a dead cookie. The web
//! client reads DMs over `POST /api/graphql`, and what arrives is a Relay connection,
//! not a list of items.
//!
//! Measured against 283 messages across 15 live threads, which is the whole archive's
//! `content_type` vocabulary and where each one keeps its words:
//!
//! ```text
//! TEXT                    170   node.text_body, present on all 170
//! MESSAGE_INLINE_SHARE     75   nothing in text_body (1/75); the words are on the
//!                               xma card: header_title_text 71, target_url 72,
//!                               title_text 10, caption_body_text 3
//! REACTION_LOG_XMAT        20   content.text_fragments only, and it is a system line
//! IMAGES                    8   content.attachments only, no words anywhere
//! MONTAGE_SHARE_XMA         8   xma.caption_body_text on all 8
//! AUDIOS                    1   content.audio_attachments only
//! TEXT_POST_APP_LINK_XMA    1   a Threads link, full xma card
//! ```
//!
//! Three identities were checked rather than assumed, because each one silently misfiles
//! the whole archive if it is wrong:
//!
//! * `thread_id` is 39 digits, the format the REST API used and the one already stored,
//!   so rows match instead of duplicating. `thread_fbid` is a different, 16-digit handle
//!   and is what the GraphQL query takes as a parameter.
//! * `sender.igid` resolved against `users[].pk` + `viewer_id` on 283 of 283 messages,
//!   so it is the Instagram id and joins to `participant`. `sender_fbid` is the Meta id
//!   and does not.
//! * `users` excludes the viewer on all 15 threads, 1:1 and group alike, as REST did.
//!
//! One capability is gone rather than moved. REST carried the reaction emoji on the item;
//! GraphQL's `msg_reactions` carries `sender_fbid` and `sender_igid` and nothing else, and
//! the `REACTION_LOG_XMAT` row that accompanies a reaction holds a fixed system line, not
//! the character. So who reacted is archived and what with is not, and it is not
//! reconstructable from this payload: a missing emoji does not mean "nobody reacted".

use chrono::{DateTime, Utc};
use instagram_core::models::{AccountRecord, MessageRecord, ParticipantRecord, ThreadRecord};
use serde_json::Value;

/// GraphQL quotes its timestamps and they are MILLIseconds (13 digits) where REST sent
/// 16-digit microseconds. Reading one unit as the other puts every message in the wrong
/// century, which no range query ever returns, and raises nothing. So the guard is a
/// range: 1e12 ms is 2001-09-09 and 1e14 ms is the year 5138.
const MIN_MILLISECONDS: i64 = 1_000_000_000_000;
const MAX_MILLISECONDS: i64 = 100_000_000_000_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NormalizeError {
    #[error("not an Instagram timestamp: {0}")]
    NotATimestamp(String),
    #[error(
        "{0} is not milliseconds since the epoch; GraphQL timestamps are 13 digits and a \
         value in any other unit parses into the wrong century"
    )]
    NotMilliseconds(i64),
    #[error(
        "thread {0} carries preview messages, not full ones: these come from the inbox \
         query and have no sender identity. Fetch the thread."
    )]
    PreviewMessages(String),
    #[error("the payload has no {0}")]
    Missing(&'static str),
}

/// One Instagram timestamp as a UTC instant. The only ms conversion there is.
///
/// Errors on anything it cannot read: a parser that fell back to `now()` would write a
/// plausible row, and nothing downstream would ever report the message as misfiled.
///
/// Every captured `timestamp_ms` and `last_activity_timestamp_ms` was a *string*; a
/// number reads the same. Built from integer milliseconds, never a float, whose mantissa
/// is coarser than the unit at this magnitude.
pub fn parse_timestamp(value: &Value) -> Result<DateTime<Utc>, NormalizeError> {
    let millis = match value {
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
    .ok_or_else(|| NormalizeError::NotATimestamp(value.to_string()))?;
    if !(MIN_MILLISECONDS..MAX_MILLISECONDS).contains(&millis) {
        return Err(NormalizeError::NotMilliseconds(millis));
    }
    DateTime::from_timestamp_millis(millis).ok_or(NormalizeError::NotMilliseconds(millis))
}

/// The logged-in user, off the `viewer` object a thread carries.
///
/// `viewer.id` equalled the thread's own `viewer_id` on all 15 captured threads, so
/// either answers "who are we"; `id` is preferred because it arrives attached to the
/// username rather than as a bare number. The inbox's `viewer` has no username, which is
/// cosmetic: the id is what the archive keys on.
pub fn account_from_viewer(viewer: &Value, account_id: &str) -> AccountRecord {
    AccountRecord {
        id: account_id.to_owned(),
        ig_user_id: user_id(viewer).unwrap_or_default(),
        username: string(viewer.get("username")),
    }
}

/// A participant's Instagram id, `id` then `pk`.
///
/// `users` is the one place the inbox and the thread query disagree: the thread query
/// sends `pk` and `id` holding the same number, the inbox sends only `id`. Reading `pk`
/// alone yielded no participants from a listing, and because the poll upserts a thread
/// from the listing it overwrote every polled thread's member list with just the viewer.
/// `id` is the key both shapes have, and it equals the `pk` the archive was keyed on.
pub fn user_id(user: &Value) -> Option<String> {
    scalar(user.get("id")).or_else(|| scalar(user.get("pk")))
}

/// Everyone in `users`, which is every participant except the account itself.
///
/// Measured on all 15 threads: a 1:1 thread's `users` holds the other person, a group
/// holds everyone but the viewer. The viewer is only `viewer_id`, a bare id, so the
/// account's own row comes from [`account_from_viewer`]. A user with no id is skipped.
///
/// `usersWithoutViewer` was empty on every capture and is not read: an always-empty
/// field is not a second source of truth.
pub fn participants_from_thread(raw: &Value, account_id: &str) -> Vec<ParticipantRecord> {
    users(raw)
        .filter_map(|user| {
            Some(ParticipantRecord {
                account_id: account_id.to_owned(),
                ig_user_id: user_id(user)?,
                username: string(user.get("username")),
                full_name: string(user.get("full_name")),
                is_verified: truthy(user.get("is_verified")),
                profile_pic_url: string(user.get("profile_pic_url")),
            })
        })
        .collect()
}

/// One thread, from either the inbox listing or a thread page: the inbox hands back
/// `node.as_ig_direct_thread`, all but the identical shape, so there is one parser. The
/// one difference that matters is the id key inside `users`; see [`user_id`].
///
/// `participant_ids` includes `viewer_id`. Without it a 1:1 thread lists one member while
/// carrying messages from two, and the account's own `sender_id` would resolve against a
/// thread it does not appear in.
pub fn thread_from_payload(raw: &Value, account_id: &str) -> Result<ThreadRecord, NormalizeError> {
    let mut ids: Vec<String> = users(raw).filter_map(user_id).collect();
    if let Some(viewer_id) = scalar(raw.get("viewer_id"))
        && !ids.contains(&viewer_id)
    {
        ids.push(viewer_id);
    }
    let activity = raw.get("last_activity_timestamp_ms");
    Ok(ThreadRecord {
        account_id: account_id.to_owned(),
        thread_id: required(raw, "thread_id")?,
        title: nonempty(raw.get("thread_title")),
        is_group: truthy(raw.get("is_group")),
        participant_ids: ids,
        last_activity_at: match activity {
            Some(value) if truthy(activity) => Some(parse_timestamp(value)?),
            _ => None,
        },
    })
}

/// The id the GraphQL thread query takes, which is not the id we store.
///
/// `thread_id` (39 digits) is the archive's key and `thread_fbid` (16) is the query
/// parameter; both are on every thread object and they are not interchangeable. The
/// inbox supplies the pair, so a pass that walks the inbox never has to persist the
/// handle in order to page a thread.
pub fn thread_handle(raw: &Value) -> Result<String, NormalizeError> {
    required(raw, "thread_fbid")
}

/// Every message on one page of a thread, oldest first.
///
/// Messages arrive under `slide_messages.edges[].node`, newest first, and are reversed
/// deliberately: an insert that dies halfway then leaves a contiguous run ending at the
/// page's older end, which the next backfill page is adjacent to. Nothing downstream
/// reads the order (the poll watermark is a `max()`).
///
/// An edge with no `node` is skipped: Relay models an edge as nullable, and a thread is
/// not worth losing to one hole in it.
///
/// Errors on an inbox thread. The inbox embeds a few recent messages per thread as a
/// PREVIEW projection: `id`, `sender_fbid`, `timestamp_ms`, `content_type`, `text_body`,
/// `igd_snippet` and nothing else. `id` equals `message_id` on all 283 captured messages,
/// so these would parse, and every row would carry a null `sender_id`: the preview has
/// no `sender.igid`, and `sender_fbid` is the Meta id that does not join to
/// `participant`. Reactions, reply ids and edit history are absent too. Degraded rows
/// are worse than none, because they look archived. So the inbox discovers threads and
/// messages come from the thread query.
pub fn messages_from_thread(
    raw: &Value,
    account_id: &str,
) -> Result<Vec<MessageRecord>, NormalizeError> {
    let thread_id = required(raw, "thread_id")?;
    let nodes: Vec<&Value> = array(raw.get("slide_messages").and_then(|m| m.get("edges")))
        .filter_map(|edge| edge.get("node").filter(|node| truthy(Some(node))))
        .collect();
    if nodes
        .first()
        .is_some_and(|node| node.get("message_id").is_none())
    {
        return Err(NormalizeError::PreviewMessages(thread_id));
    }
    nodes
        .into_iter()
        .rev()
        .map(|node| message(node, account_id, &thread_id))
        .collect()
}

/// The cursor for the page of messages *older* than this one, or `None` at the end.
///
/// Messages arrive newest first, so Relay's forward direction is backwards in time:
/// `has_next_page` means older messages exist and `end_cursor` is where they resume. It
/// is checked first because `end_cursor` is still a string on the last page, and passing
/// it again would re-fetch that same page forever. `has_next_page` is honest: of 15
/// captured threads, 14 returned exactly 20 messages and said true, and the short one
/// returned 3 and said false.
///
/// The cursor is opaque (132 characters, not an id) and belongs to a DIFFERENT query than
/// the one that produced it. `IGDThreadDetailQuery` serves only the newest page and takes
/// no cursor: its `min_uq_seq_id` is inert (the same window for null, a real mailbox
/// sequence id, and 1) and its page count is a Relay provider constant the server ignores
/// (20, 100 and 500 all returned 20). Paging is `IGDMessageListOffMsysQuery`, doc_id
/// 29154920810814879, an ordinary Relay connection taking `{after, before, first, last,
/// newer_than_message_id, older_than_message_id}`; this cursor goes in its `after`.
///
/// A real client fires that query only when the reader presses a key in the transcript.
/// Wheel events and assigning `scrollTop` reach the far end of the column-reverse
/// transcript and load nothing, which is why it stayed invisible until a human scrolled.
pub fn older_page_cursor(raw: &Value) -> Option<String> {
    let page = raw.get("slide_messages").and_then(|m| m.get("page_info"))?;
    if !truthy(page.get("has_next_page")) {
        return None;
    }
    scalar(page.get("end_cursor"))
}

/// What a person, and a `tsvector`, can search this message by.
///
/// A type with nothing searchable returns `""` rather than a description of itself:
/// `content_type` is its own indexed column and already answers "was this an image", and
/// "shared a reel" in `text` would teach the index a phrase matching every reel ever
/// sent. An unknown type is not an error either: the set is Instagram's to extend, and
/// the message still lands with its type and its `raw` intact.
pub fn message_text(node: &Value) -> String {
    match content_type(node).as_str() {
        "TEXT" => plain_text(node),
        // A shared post or reel. `text_body` is empty on 74 of 75, so the words are on the
        // xma card: `header_title_text` (71/75) is the author's handle as the client
        // renders it, `title_text` (10/75) and `caption_body_text` (3/75) are the post's
        // own words. All three are different facts. `target_url` is deliberately left
        // out: it lands in `share_permalink`, and a copy here only makes every share of
        // one post match a query for any of them.
        "MESSAGE_INLINE_SHARE" => join(&[
            body(node).as_deref(),
            xma_str(node, "header_title_text"),
            xma_str(node, "title_text"),
            xma_str(node, "caption_body_text"),
        ]),
        // A shared story: `caption_body_text` on all 8, and a note alongside on 6.
        "MONTAGE_SHARE_XMA" => join(&[body(node).as_deref(), xma_str(node, "caption_body_text")]),
        // A Threads link: one capture, carrying header, subtitle and caption.
        "TEXT_POST_APP_LINK_XMA" => join(&[
            body(node).as_deref(),
            xma_str(node, "header_title_text"),
            xma_str(node, "subtitle_text"),
            xma_str(node, "caption_body_text"),
        ]),
        // Checked against the capture and found to hold no words; named so a reader can
        // see they were checked rather than forgotten. IMAGES (8): `content.attachments`
        // only, cdn urls, fbids and preview sizes; a photo sent into a DM has no caption
        // field. AUDIOS (1): `content.audio_attachments`, a cdn url, a duration and a
        // waveform; a voice note's words are in the audio. REACTION_LOG_XMAT (20):
        // `content.text_fragments` holds one fixed 15-character ASCII system line,
        // identical on all 20, which is `content_type` in prose.
        "IMAGES" | "AUDIOS" | "REACTION_LOG_XMAT" => String::new(),
        _ => String::new(),
    }
}

/// `node.text_body`, the 60% case. `content.text_body` held the identical string on all
/// 170 captured TEXT messages; the outer one is read first because it is also populated
/// on the share types, where the inner object may be an xma card instead.
fn plain_text(node: &Value) -> String {
    body(node)
        .or_else(|| scalar(node.get("content").and_then(|c| c.get("text_body"))))
        .unwrap_or_default()
}

/// What was shared, as the four columns `message` stores: all `None` for a message that
/// shares nothing, which is two thirds of the corpus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Share {
    /// `xma.__typename`, verbatim; see [`SHARE_TYPES`].
    pub kind: Option<String>,
    /// The handle of whoever AUTHORED this, not the person who forwarded it (that is
    /// `sender_id`). The wrong way round answers "who sent me this reel" with the name of
    /// a stranger, and nothing in the data would look wrong.
    pub owner: Option<String>,
    pub caption: Option<String>,
    pub permalink: Option<String>,
}

/// One xma card carries every kind of share, so `kind` can no longer be read off the
/// item type as under REST, where a reel and a post arrived in differently named keys.
/// `xma.__typename` distinguishes them now; it was present on all 84 share messages and
/// is stored verbatim rather than mapped onto the old vocabulary, because a translation
/// table would make a reader trust a correspondence nobody has checked.
pub const SHARE_TYPES: [&str; 3] = [
    "MESSAGE_INLINE_SHARE",
    "MONTAGE_SHARE_XMA",
    "TEXT_POST_APP_LINK_XMA",
];

/// The share facts on one message, or an all-`None` [`Share`] if it shares nothing.
///
/// An unknown content type projects nothing rather than erroring, for the same reason
/// [`message_text`] returns `""`: the payload still lands whole in `raw` for a later
/// reader to project out of.
pub fn share_projection(node: &Value) -> Share {
    if !SHARE_TYPES.contains(&content_type(node).as_str()) {
        return Share::default();
    }
    let Some(xma) = node
        .get("content")
        .and_then(|c| c.get("xma"))
        .filter(|xma| truthy(Some(xma)))
    else {
        return Share::default();
    };
    Share {
        kind: scalar(xma.get("__typename")),
        owner: nonempty(xma.get("header_title_text")),
        caption: nonempty(xma.get("caption_body_text")).or_else(|| nonempty(xma.get("title_text"))),
        permalink: unwrap_redirect(xma.get("target_url").and_then(Value::as_str)),
    }
}

/// The real destination inside an `l.instagram.com/?u=...` redirect.
///
/// Instagram wraps outbound links in its own redirector with a per-send token, so the
/// same URL shared twice stores as two different strings and neither is the page: left
/// wrapped, no two shares of one article are ever equal. Anything that is not that
/// redirector is returned untouched.
fn unwrap_redirect(wrapped: Option<&str>) -> Option<String> {
    let wrapped = wrapped.filter(|w| !w.is_empty())?;
    if let Ok(url) = url::Url::parse(wrapped)
        && url.host_str().is_some_and(|host| host == "l.instagram.com")
        && let Some((_, target)) = url
            .query_pairs()
            .find(|(key, value)| key == "u" && !value.is_empty())
    {
        return Some(target.into_owned());
    }
    Some(wrapped.to_owned())
}

/// One message node as a row: the single-node step [`messages_from_thread`] maps over.
pub fn message(
    node: &Value,
    account_id: &str,
    thread_id: &str,
) -> Result<MessageRecord, NormalizeError> {
    let share = share_projection(node);
    let ts = node
        .get("timestamp_ms")
        .ok_or(NormalizeError::Missing("timestamp_ms"))?;
    Ok(MessageRecord {
        account_id: account_id.to_owned(),
        thread_id: thread_id.to_owned(),
        item_id: required(node, "message_id")?,
        ts: parse_timestamp(ts)?,
        // `sender.igid`, not `sender_fbid`: only the former joins to `participant`.
        // Checked against every participant list in the capture, 283 of 283.
        sender_id: scalar(node.get("sender").and_then(|s| s.get("igid"))),
        item_type: content_type(node),
        text: message_text(node),
        share_kind: share.kind,
        share_owner: share.owner,
        share_caption: share.caption,
        share_permalink: share.permalink,
        // GraphQL sends the id alone where REST embedded the whole replied-to item. It
        // may point at a row that does not exist yet, or ever, which is why the schema
        // puts no foreign key on it.
        reply_to_item_id: scalar(node.get("replied_to_message_id")),
        reactions: reactions(node),
        // An edit leaves the superseded wording in `slide_edit_history` and the current
        // text in `text_body`. REST had no edited DMs, so this was always false; the old
        // text is in `raw` rather than lost.
        edited: truthy(node.get("slide_edit_history")),
        // Verbatim, including the keys this layer understood: a field dropped here is a
        // field that needs the account fetched again to recover.
        raw: node.clone(),
    })
}

/// Who reacted to this message, not what with (see the module docs).
///
/// `msg_reactions` is a flat list of `{sender_fbid, sender_igid}`: no emoji, no
/// timestamp, no count. Entries are stored as they arrive, so two reactions by the same
/// person are indistinguishable, which is a property of the payload, not of this parser.
fn reactions(node: &Value) -> Vec<Value> {
    array(node.get("msg_reactions"))
        .filter(|entry| entry.is_object())
        .cloned()
        .collect()
}

/// The non-empty parts, trimmed, one per line. No connective phrase and no bracketed
/// labels: everything in this column is words somebody wrote, so the index never learns
/// a token this repo invented.
fn join(parts: &[Option<&str>]) -> String {
    parts
        .iter()
        .flatten()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------- the Python reads, on a JSON value

/// Python's `bool(value)`: null, false, 0, "", [] and {} are false.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `str(value) if value else None` for an id or text that may arrive quoted or bare.
fn scalar(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) if truthy(value) => Some(n.to_string()),
        _ => None,
    }
}

/// A string field as it came, `""` included.
fn string(value: Option<&Value>) -> Option<String> {
    value?.as_str().map(str::to_owned)
}

/// A string field, `""` read as absent.
fn nonempty(value: Option<&Value>) -> Option<String> {
    string(value).filter(|s| !s.is_empty())
}

/// `str(raw[key])`: a key the payload must carry.
fn required(raw: &Value, key: &'static str) -> Result<String, NormalizeError> {
    match raw.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => Err(NormalizeError::Missing(key)),
    }
}

fn array(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    value.and_then(Value::as_array).into_iter().flatten()
}

fn users(raw: &Value) -> impl Iterator<Item = &Value> {
    array(raw.get("users"))
}

fn content_type(node: &Value) -> String {
    scalar(node.get("content_type")).unwrap_or_default()
}

fn body(node: &Value) -> Option<String> {
    scalar(node.get("text_body"))
}

fn xma_str<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.get("content")?.get("xma")?.get(key)?.as_str()
}
