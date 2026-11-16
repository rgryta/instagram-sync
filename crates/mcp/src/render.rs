//! Rows to text a model can read.
//!
//! Output is prose, not JSON: an Instagram payload is large enough to eat a context
//! window for no benefit, and a model reads a rendered line better than a nested
//! object. Every message line carries its `item_id` and its thread, because those are
//! the keys the drill-in tools take.
//!
//! Roughly half of this corpus is not typed text but a reel, a post, a story or a link
//! someone forwarded. A renderer that prints `text` and stops shows that half as blank
//! lines, so a share is rendered as what it is: who authored it, its caption, its link.

use chrono::{DateTime, TimeDelta, Utc};
use instagram_core::models::{
    AccountStatus, Alias, Message, Person, Revision, ShareOwner, SyncLoopStatus, ThreadSummary,
};
use instagram_core::store::Directory;

/// Enough to recognise what a reel is about, and no more. Creator captions run to
/// hundreds of words; printed whole they crowd out every other result in an answer.
pub const CAPTION_LIMIT: usize = 200;

/// ISO 8601 in UTC.
///
/// `2026-08-14 20:50` drops the offset, so a model cannot tell it from local time and
/// cannot order it against anything else. An explicit offset is unambiguous.
///
/// UTC, not local: these timestamps are read alongside output from other tools, and a
/// shared zero point makes them directly comparable.
pub fn when(moment: Option<DateTime<Utc>>) -> String {
    match moment {
        Some(moment) => moment.format("%Y-%m-%dT%H:%M+00:00").to_string(),
        None => "never".to_owned(),
    }
}

/// A value quoted the way Python's `repr` quotes a `str`, because several tool replies
/// interpolate one with `{value!r}` and the two servers are meant to answer the same
/// bytes. Single quotes, unless the value contains one and no double quote.
///
/// shortcut: control characters other than tab, newline and carriage return are passed
/// through rather than escaped as `\xNN`; upgrade this if a tool argument ever carries
/// one, which would mean a caller pasted a terminal escape into a search.
pub fn quoted(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// How a `share_kind` reads in a sentence. `share_kind` is normalised at ingest across
/// Instagram's own key-naming (`clip`, `direct_media_share`), so this maps the four
/// values that exist rather than every item type.
fn share_noun(kind: &str) -> String {
    match kind {
        "reel" => "a reel".to_owned(),
        "post" => "a post".to_owned(),
        "story" => "a story".to_owned(),
        "link" => "a link".to_owned(),
        other => format!("a {other}"),
    }
}

/// A caption, shortened to one readable line.
///
/// Creator captions run to hundreds of words of hashtags and booking instructions — one
/// live reel produced twenty lines on its own, so five shares buried the rest of the
/// answer. Newlines collapse and the tail is cut, visibly: the permalink is on the same
/// line, so the full text is one click away and nothing is actually lost.
///
/// Counted in characters rather than bytes, because the corpus is Polish and emoji: a
/// byte limit would cut a 200-character caption at 90 characters and could split a code
/// point in half.
pub fn clip_caption(caption: &str) -> String {
    let flat = caption.split_whitespace().collect::<Vec<_>>().join(" ");
    let total = flat.chars().count();
    if total <= CAPTION_LIMIT {
        return flat;
    }
    let cut: String = flat.chars().take(CAPTION_LIMIT).collect();
    // Cut on a word boundary so the ellipsis does not land mid-word. With no space in
    // the whole window there is no boundary to find, and the hard cut stands.
    let head = match cut.rsplit_once(' ') {
        Some((head, _)) => head,
        None => cut.as_str(),
    };
    format!("{head}… (+{} chars)", total - head.chars().count())
}

/// What the sender typed on top of the share, if anything.
///
/// `ingest.normalize` builds `text` for a share out of the author's handle, the caption
/// and — on a story reply — the sender's own words, because all three belong in the
/// search index. Rendering `text` as well as the share fields would print the handle
/// and caption twice, so the two known halves are subtracted and only a genuinely
/// human-written remainder is shown.
fn typed_over(message: &Message) -> String {
    let mut leftover = message.text.clone();
    let handle = message
        .share_owner
        .as_ref()
        .map(|owner| format!("@{owner}"));
    for part in [handle.as_deref(), message.share_caption.as_deref()] {
        if let Some(part) = part.filter(|part| !part.is_empty()) {
            leftover = leftover.replacen(part, "", 1);
        }
    }
    leftover.trim().to_owned()
}

/// A forwarded reel, post, story or link, rendered as the thing it is.
fn share_body(message: &Message) -> String {
    let kind = message.share_kind.as_deref().unwrap_or("");
    let mut parts = vec![format!("shared {}", share_noun(kind))];
    if let Some(owner) = &message.share_owner {
        parts.push(format!("by @{owner}"));
    }
    if let Some(caption) = &message.share_caption {
        parts.push(format!("\u{201c}{}\u{201d}", clip_caption(caption)));
    }
    if let Some(permalink) = &message.share_permalink {
        parts.push(permalink.clone());
    } else if kind == "story" {
        // An expired story is gone from Instagram's side and no URL can be built for
        // it. Saying so beats a blank, and beats a fabricated link that 404s.
        parts.push("(no link — the story has expired)".to_owned());
    }
    let typed = typed_over(message);
    if !typed.is_empty() {
        parts.push(format!("— said: {typed}"));
    }
    parts.join(" ")
}

fn reaction_text(message: &Message, directory: &Directory) -> String {
    message
        .reactions
        .iter()
        .map(|reaction| {
            let emoji = reaction
                .emoji
                .as_deref()
                .filter(|emoji| !emoji.is_empty())
                .unwrap_or("?");
            let who = directory.person_name(&message.account_id, reaction.sender_id.as_deref());
            format!("{emoji} {who}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The content of one message, whatever kind of thing it is.
pub fn body_of(message: &Message) -> String {
    if message.share_kind.is_some() {
        return share_body(message);
    }
    if message.text.is_empty() {
        // A photo, a disappearing image, a video call: nothing was typed and nothing
        // was projected, so the type is the only content there is. It beats a blank
        // line.
        return format!("[{}]", message.item_type);
    }
    // A system line reads as prose ("Reacted 😂 to your message") and would otherwise
    // be indistinguishable from something a person typed. `TEXT` is upper case because
    // it is GraphQL's `content_type`; comparing it against `"text"` prefixed every
    // plain message a person ever sent with `[TEXT]`.
    if message.item_type == "TEXT" {
        message.text.clone()
    } else {
        format!("[{}] {}", message.item_type, message.text)
    }
}

/// One message as one line, keyed by the ids a drill-in call needs.
pub fn render_message(message: &Message, directory: &Directory, with_thread: bool) -> String {
    let label = if with_thread {
        format!(
            "[{}] ",
            directory.thread_label(&message.account_id, &message.thread_id)
        )
    } else {
        String::new()
    };
    let who = if directory.is_me(&message.account_id, message.sender_id.as_deref()) {
        "me".to_owned()
    } else {
        directory.person_name(&message.account_id, message.sender_id.as_deref())
    };
    let keys = format!("  [thread {} item {}]", message.thread_id, message.item_id);
    if message.deleted_at.is_some() {
        // The text stays in the archive and in the revision log; this layer withholds
        // it. Printing what someone deleted next to the note that they deleted it would
        // make the marker pointless.
        return format!(
            "{label}{} {who}: [deleted {}]{keys}",
            when(Some(message.ts)),
            when(message.deleted_at)
        );
    }
    let mut line = format!(
        "{label}{} {who}: {}",
        when(Some(message.ts)),
        body_of(message)
    );
    if message.edited {
        line.push_str(" (edited)");
    }
    if let Some(reply_to) = &message.reply_to_item_id {
        line.push_str(&format!("  (reply to {reply_to})"));
    }
    if !message.reactions.is_empty() {
        line.push_str(&format!("  — {}", reaction_text(message, directory)));
    }
    line + &keys
}

/// Say it out loud when a page is not the whole answer.
///
/// A page that looks complete is how a reader concludes "there were only twelve" from a
/// corpus of four hundred.
pub fn truncation_note(shown: i64, total: i64) -> Vec<String> {
    if total <= shown {
        return Vec::new();
    }
    vec![format!(
        "note: showing {shown} of {total} match(es) — raise `limit` or narrow the filters"
    )]
}

/// A block of messages. An empty result is a rendered answer, not an error.
pub fn render_messages(
    messages: &[Message],
    directory: &Directory,
    header: &str,
    with_thread: bool,
    footer: &[String],
) -> String {
    let mut lines = vec![header.to_owned()];
    if messages.is_empty() {
        lines.push("(no messages matched)".to_owned());
    } else {
        lines.extend(
            messages
                .iter()
                .map(|message| render_message(message, directory, with_thread)),
        );
    }
    lines.extend(footer.iter().cloned());
    lines.join("\n")
}

pub fn render_threads(
    threads: &[ThreadSummary],
    total_matches: i64,
    directory: &Directory,
    header: &str,
) -> String {
    if threads.is_empty() {
        return format!("{header}\n(no threads matched)");
    }
    let mut lines = vec![header.to_owned()];
    for thread in threads {
        let kind = if thread.is_group { "group" } else { "DM" };
        let people = directory
            .others_in(&thread.account_id, &thread.thread_id)
            .iter()
            .map(|id| directory.person_name(&thread.account_id, Some(id)))
            .collect::<Vec<_>>()
            .join(", ");
        let pending = if thread.backfill_done {
            ""
        } else {
            ", backfill incomplete"
        };
        lines.push(format!(
            "{} ({kind}, {} message(s), last {}{pending})  with: {}  [thread {}]",
            directory.thread_label(&thread.account_id, &thread.thread_id),
            thread.message_count,
            when(thread.last_activity_at),
            if people.is_empty() {
                "nobody else stored".to_owned()
            } else {
                people
            },
            thread.thread_id
        ));
    }
    lines.extend(truncation_note(threads.len() as i64, total_matches));
    lines.join("\n")
}

/// People, then the aliases in force.
///
/// Every name form is printed on purpose: showing one resolved name would hide that two
/// people matched, and the caller could not tell which one they got.
pub fn render_people(people: &[Person], directory: &Directory, aliases: &[Alias]) -> String {
    let mut lines: Vec<String> = Vec::new();
    if people.is_empty() {
        lines.push("no matching people".to_owned());
    } else {
        lines.push(format!("{} person/people", people.len()));
        for person in people {
            // An empty string is as much an absence as NULL here: Python filters the
            // list on truthiness, so a participant row with `full_name = ''` must not
            // contribute an empty name form.
            let present = |value: &Option<String>| value.clone().filter(|text| !text.is_empty());
            let mut known: Vec<String> = Vec::new();
            if let Some(alias) = present(&person.aliases) {
                known.push(format!("alias {alias}"));
            }
            if let Some(full_name) = present(&person.full_name) {
                known.push(full_name);
            }
            if let Some(username) = present(&person.username) {
                known.push(format!("@{username}"));
            }
            if directory.is_me(&person.account_id, Some(&person.ig_user_id)) {
                known.push("this is you".to_owned());
            }
            if person.is_verified {
                known.push("verified".to_owned());
            }
            let where_ = match &person.dm_thread_id {
                Some(thread_id) => format!("DM thread {thread_id}"),
                None => "no DM thread".to_owned(),
            };
            lines.push(format!(
                "{} ({}) — {} thread(s), {} message(s), {where_}  [member {}:{}]",
                directory.person_name(&person.account_id, Some(&person.ig_user_id)),
                if known.is_empty() {
                    "no name stored".to_owned()
                } else {
                    known.join("; ")
                },
                person.thread_count,
                person.message_count,
                person.account_id,
                person.ig_user_id
            ));
        }
    }
    lines.push(String::new());
    lines.push(render_aliases(aliases, directory));
    lines.join("\n")
}

/// Aliases and who is behind each one.
///
/// Members are rendered by the name Instagram knows, never by the alias: echoing the
/// alias back would leave the caller unable to check whether it names the right person.
pub fn render_aliases(aliases: &[Alias], directory: &Directory) -> String {
    if aliases.is_empty() {
        return "no aliases set".to_owned();
    }
    let mut lines = vec![format!("{} alias(es)", aliases.len())];
    for entry in aliases {
        let members = entry
            .members
            .iter()
            .map(|member| {
                format!(
                    "{} [member {}:{}]",
                    directory.instagram_name(&member.account_id, &member.ig_user_id),
                    member.account_id,
                    member.ig_user_id
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let note = match &entry.note {
            Some(note) => format!(" — {note}"),
            None => String::new(),
        };
        lines.push(format!(
            "{}{note}: {}",
            entry.alias,
            if members.is_empty() {
                "(no members)".to_owned()
            } else {
                members
            }
        ));
    }
    lines.join("\n")
}

pub fn render_owners(owners: &[ShareOwner], header: &str) -> String {
    if owners.is_empty() {
        return format!("{header}\n(nobody has shared anything yet)");
    }
    let mut lines = vec![header.to_owned()];
    for owner in owners {
        lines.push(format!(
            "@{} — {} share(s), last {}",
            owner.owner,
            owner.shares,
            when(owner.last_shared_at)
        ));
    }
    lines.join("\n")
}

pub fn render_status(
    accounts: &[AccountStatus],
    loops: &[SyncLoopStatus],
    directory: &Directory,
) -> String {
    if accounts.is_empty() {
        return "no account registered yet — the ingester has never run".to_owned();
    }
    let mut lines = vec!["ingest status".to_owned()];
    for account in accounts {
        let state = if account.enabled {
            "enabled"
        } else {
            "disabled"
        };
        lines.push(format!(
            "{} (@{}, {state}): {} thread(s), {} message(s) of which {} share(s), \
             {} participant(s)",
            account.account_id,
            account.username.as_deref().unwrap_or("?"),
            account.threads,
            account.messages,
            account.shares,
            account.participants
        ));
        lines.push(format!(
            "  history {} to {}, {} thread(s) still owing backfill",
            when(account.oldest_message_at),
            when(account.newest_message_at),
            account.threads_pending_backfill
        ));
        // Said out loud rather than quietly deducted. The two are separate because they
        // mean different things: the first is noise Instagram files by itself, the
        // second is the archive admitting it holds nothing for something that was sent.
        if account.system_events > 0 {
            lines.push(format!(
                "  plus {} reaction/call event(s), stored and shown when reading a thread \
                 but not counted as messages",
                account.system_events
            ));
        }
        if account.unreadable > 0 {
            lines.push(format!(
                "  plus {} message(s) Instagram would not hand over (unsupported type, \
                 deleted post) — nothing of them was archived",
                account.unreadable
            ));
        }
        if let Some(error) = &account.last_auth_error {
            // A dead session is not a transient fault: recovery is a human minting a new
            // `sessionid` by hand, so the archive is frozen until they do.
            lines.push(format!(
                "  SESSION FAILING since {}: {error} — nothing new is being archived until \
                 a human mints a fresh session",
                when(account.last_auth_error_at)
            ));
        }
    }
    for entry in loops {
        let mut line = format!(
            "{} loop: last run {}",
            entry.loop_name,
            when(entry.last_run_at)
        );
        if let Some(thread_id) = &entry.current_thread_id {
            line.push_str(&format!(
                ", last working on {}",
                directory.thread_label(&entry.account_id, thread_id)
            ));
        }
        lines.push(line);
        if let Some(error) = &entry.last_error {
            lines.push(format!("  ERROR at {}: {error}", when(entry.last_error_at)));
        }
    }
    if loops.is_empty() {
        lines.push("no loop has reported a run yet".to_owned());
    }
    lines.join("\n")
}

/// One message's text over time, oldest first.
///
/// The only place a deleted message's text is shown. Every other view withholds it on
/// purpose; asking for the history is asking for exactly this.
pub fn render_revisions(
    revisions: &[Revision],
    header: &str,
    deleted_at: Option<DateTime<Utc>>,
) -> String {
    if revisions.is_empty() && deleted_at.is_none() {
        return format!("{header}\nno recorded changes");
    }
    let mut lines = vec![header.to_owned()];
    for revision in revisions {
        // A NULL start is unknown, not missing: nothing records when a message was
        // first stored, so the earliest text held has no honest start time.
        let start = match revision.first_seen_at {
            Some(moment) => when(Some(moment)),
            None => "unknown".to_owned(),
        };
        let end = match revision.superseded_at {
            Some(moment) => when(Some(moment)),
            None => "current".to_owned(),
        };
        // A NULL and an empty string are the same absence here, as they are everywhere
        // else in this renderer.
        let text = revision
            .text
            .as_deref()
            .filter(|text| !text.is_empty())
            .unwrap_or("(no text)");
        lines.push(format!("  {}  {start} -> {end}   \"{text}\"", revision.seq));
    }
    if deleted_at.is_some() {
        lines.push(format!("deleted {}", when(deleted_at)));
    }
    if !revisions.is_empty() {
        lines.push(
            "note: revision 1 is the earliest text this archive saw, which is not \
             necessarily what was first sent."
                .to_owned(),
        );
    }
    lines.join("\n")
}

/// Say how much of the requested period was not shown, and the call that fetches it.
///
/// Without this a capped page is indistinguishable from the whole period, and a caller
/// who asked for July reads the last day of it as all of July. The cursor overlaps the
/// page by a second so that a message sharing the boundary's timestamp is never skipped.
pub fn window_note(
    more: i64,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
    oldest_first: bool,
) -> Vec<String> {
    if more <= 0 {
        return Vec::new();
    }
    let second = TimeDelta::seconds(1);
    if oldest_first {
        let cursor = (last - second).format("%Y-%m-%dT%H:%M:%SZ");
        vec![format!(
            "note: {more} more message(s) in this period after the last one shown - \
             next page: after={cursor} (a second of overlap, so none is skipped)"
        )]
    } else {
        let cursor = (first + second).format("%Y-%m-%dT%H:%M:%SZ");
        vec![format!(
            "note: {more} older message(s) match this request - \
             next page: before={cursor} (a second of overlap, so none is skipped)"
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use instagram_core::models::{Identity, Reaction};
    use instagram_core::store::{DirectoryAccount, DirectoryPerson, DirectoryThread};

    fn base() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap()
    }

    fn directory() -> Directory {
        let mut directory = Directory::default();
        directory.accounts.insert(
            "me".into(),
            DirectoryAccount {
                ig_user_id: "1".into(),
            },
        );
        directory
    }

    fn kasia() -> Directory {
        let mut directory = directory();
        directory.people.insert(
            ("me".into(), "2".into()),
            DirectoryPerson {
                username: Some("k_zaw_92".into()),
                full_name: Some("Katarzyna Zawadzka".into()),
            },
        );
        directory
    }

    fn message() -> Message {
        Message {
            account_id: "me".into(),
            thread_id: "T1".into(),
            item_id: "i1".into(),
            ts: base(),
            sender_id: Some("2".into()),
            item_type: "TEXT".into(),
            text: "hello".into(),
            deleted_at: None,
            edited: false,
            share_kind: None,
            share_owner: None,
            share_caption: None,
            share_permalink: None,
            reply_to_item_id: None,
            reactions: Vec::new(),
        }
    }

    #[test]
    fn a_quoted_value_reads_the_way_pythons_repr_prints_one() {
        assert_eq!(quoted("sailing"), "'sailing'");
        assert_eq!(quoted("it's"), "\"it's\"");
        assert_eq!(quoted("a\"b"), "'a\"b'");
        assert_eq!(quoted("both'\""), "'both\\'\"'");
        assert_eq!(quoted("line\nbreak"), "'line\\nbreak'");
    }

    #[test]
    fn never_renders_a_local_looking_timestamp() {
        // A stamp without an offset cannot be ordered against anything else.
        assert_eq!(when(Some(base())), "2026-08-01T12:00+00:00");
        assert_eq!(when(None), "never");
    }

    #[test]
    fn the_account_owner_is_rendered_as_me() {
        let line = render_message(
            &Message {
                sender_id: Some("1".into()),
                ..message()
            },
            &directory(),
            false,
        );
        assert!(
            line.starts_with("2026-08-01T12:00+00:00 me: hello"),
            "{line}"
        );
    }

    #[test]
    fn a_message_carries_the_keys_a_drill_in_needs() {
        let line = render_message(&message(), &directory(), false);
        assert!(line.ends_with("[thread T1 item i1]"), "{line}");
    }

    #[test]
    fn a_plain_message_is_not_labelled_with_its_own_type() {
        // GraphQL's `content_type` is upper case. Compared against `"text"`, every
        // message a person ever typed came back as `[TEXT] hello`.
        assert_eq!(body_of(&message()), "hello");
    }

    #[test]
    fn a_reel_is_rendered_as_the_thing_it_is() {
        let body = body_of(&Message {
            item_type: "MESSAGE_INLINE_SHARE".into(),
            text: "@surfclub big waves".into(),
            share_kind: Some("reel".into()),
            share_owner: Some("surfclub".into()),
            share_caption: Some("big waves".into()),
            share_permalink: Some("https://www.instagram.com/reel/abc/".into()),
            ..message()
        });
        assert_eq!(
            body,
            "shared a reel by @surfclub \u{201c}big waves\u{201d} https://www.instagram.com/reel/abc/"
        );
    }

    #[test]
    fn what_the_sender_typed_over_a_share_survives_and_is_not_duplicated() {
        let body = body_of(&Message {
            item_type: "MESSAGE_INLINE_SHARE".into(),
            text: "@surfclub big waves look at this".into(),
            share_kind: Some("story".into()),
            share_owner: Some("surfclub".into()),
            share_caption: Some("big waves".into()),
            ..message()
        });
        assert!(body.ends_with("— said: look at this"), "{body}");
        assert_eq!(body.matches("big waves").count(), 1, "{body}");
    }

    #[test]
    fn an_expired_story_says_there_is_no_link_rather_than_inventing_one() {
        let body = body_of(&Message {
            item_type: "MESSAGE_INLINE_SHARE".into(),
            text: String::new(),
            share_kind: Some("story".into()),
            ..message()
        });
        assert!(body.contains("(no link — the story has expired)"), "{body}");
    }

    #[test]
    fn a_message_with_no_text_renders_its_type() {
        assert_eq!(
            body_of(&Message {
                item_type: "IMAGES".into(),
                text: String::new(),
                ..message()
            }),
            "[IMAGES]"
        );
    }

    #[test]
    fn a_non_text_item_with_text_is_labelled_so_it_is_not_read_as_typed() {
        assert_eq!(
            body_of(&Message {
                item_type: "REACTION_LOG_XMAT".into(),
                text: "Liked a message".into(),
                ..message()
            }),
            "[REACTION_LOG_XMAT] Liked a message"
        );
    }

    #[test]
    fn reactions_and_replies_are_shown() {
        let mut directory = directory();
        directory.people.insert(
            ("me".into(), "3".into()),
            DirectoryPerson {
                username: None,
                full_name: Some("Olek".into()),
            },
        );
        let line = render_message(
            &Message {
                reply_to_item_id: Some("i0".into()),
                reactions: vec![Reaction {
                    emoji: Some("🔥".into()),
                    sender_id: Some("3".into()),
                }],
                ..message()
            },
            &directory,
            false,
        );
        assert!(line.contains("(reply to i0)"), "{line}");
        assert!(line.contains("🔥 Olek"), "{line}");
    }

    #[test]
    fn a_reaction_with_no_emoji_still_names_who_reacted() {
        let line = render_message(
            &Message {
                reactions: vec![Reaction::default()],
                ..message()
            },
            &directory(),
            false,
        );
        assert!(line.contains("— ? unknown"), "{line}");
    }

    #[test]
    fn a_deleted_message_withholds_its_text() {
        let line = render_message(
            &Message {
                deleted_at: Some(base()),
                ..message()
            },
            &directory(),
            false,
        );
        assert!(!line.contains("hello"), "{line}");
        assert!(line.contains("[deleted 2026-08-01T12:00+00:00]"), "{line}");
    }

    #[test]
    fn an_empty_result_is_an_answer_not_an_error() {
        assert!(
            render_messages(&[], &directory(), "nothing", true, &[])
                .contains("(no messages matched)")
        );
    }

    #[test]
    fn truncation_is_said_out_loud() {
        assert!(truncation_note(12, 12).is_empty());
        assert!(truncation_note(12, 400)[0].contains("showing 12 of 400"));
    }

    #[test]
    fn a_footer_follows_the_messages() {
        let out = render_messages(
            &[message()],
            &directory(),
            "head",
            true,
            &["tail".to_owned()],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "head");
        assert_eq!(lines[lines.len() - 1], "tail");
    }

    #[test]
    fn a_long_caption_is_shortened_visibly_and_says_by_how_much() {
        let line = clip_caption(&"word ".repeat(200));
        assert!(line.chars().count() < 260, "{line}");
        assert!(line.ends_with("chars)"), "{line}");
        assert!(line.contains('…'), "{line}");
    }

    #[test]
    fn a_short_caption_is_left_exactly_as_it_is() {
        assert_eq!(clip_caption("a tidy caption"), "a tidy caption");
    }

    #[test]
    fn a_caption_with_newlines_collapses_to_one_line() {
        assert_eq!(clip_caption("one\n\ntwo   three\n"), "one two three");
    }

    #[test]
    fn the_cut_lands_on_a_word_boundary() {
        let line = clip_caption(&"alpha bravo charlie delta ".repeat(40));
        assert!(line.contains('…'), "{line}");
        assert!(!line.split('…').next().unwrap().ends_with(' '), "{line}");
    }

    #[test]
    fn a_caption_is_cut_by_characters_not_by_bytes() {
        // Polish and emoji are two to four bytes a character. A byte limit would cut
        // this at well under 200 characters, and could split a code point.
        let caption = "ą".repeat(300);
        let line = clip_caption(&caption);
        assert!(line.starts_with(&"ą".repeat(200)), "{line}");
        assert!(line.contains("(+100 chars)"), "{line}");
    }

    #[test]
    fn a_thread_listing_names_the_dm_and_flags_incomplete_backfill() {
        let mut directory = kasia();
        directory.threads.insert(
            ("me".into(), "T1".into()),
            DirectoryThread {
                title: None,
                is_group: false,
                participant_ids: vec!["1".into(), "2".into()],
            },
        );
        let out = render_threads(
            &[ThreadSummary {
                account_id: "me".into(),
                thread_id: "T1".into(),
                is_group: false,
                last_activity_at: Some(base()),
                backfill_done: false,
                message_count: 3,
            }],
            9,
            &directory,
            "threads",
        );
        assert!(out.contains("DM with Katarzyna Zawadzka"), "{out}");
        assert!(out.contains("backfill incomplete"), "{out}");
        assert!(out.contains("showing 1 of 9"), "{out}");
    }

    #[test]
    fn an_empty_thread_listing_says_so() {
        assert!(render_threads(&[], 0, &directory(), "threads").contains("(no threads matched)"));
    }

    fn person() -> Person {
        Person {
            account_id: "me".into(),
            ig_user_id: "2".into(),
            username: Some("k_zaw_92".into()),
            full_name: Some("Katarzyna Zawadzka".into()),
            is_verified: false,
            aliases: None,
            thread_count: 2,
            message_count: 40,
            dm_thread_id: Some("T1".into()),
        }
    }

    #[test]
    fn people_show_every_name_form_so_an_ambiguous_match_is_visible() {
        let out = render_people(&[person()], &kasia(), &[]);
        assert!(out.contains("Katarzyna Zawadzka"), "{out}");
        assert!(out.contains("@k_zaw_92"), "{out}");
        assert!(out.contains("[member me:2]"), "{out}");
        assert!(out.contains("no aliases set"), "{out}");
    }

    #[test]
    fn a_person_with_no_name_at_all_still_renders() {
        let out = render_people(
            &[Person {
                username: None,
                full_name: None,
                dm_thread_id: None,
                ..person()
            }],
            &directory(),
            &[],
        );
        assert!(out.contains("no name stored"), "{out}");
        assert!(out.contains("no DM thread"), "{out}");
    }

    #[test]
    fn no_matching_people_is_an_answer() {
        assert!(render_people(&[], &directory(), &[]).starts_with("no matching people"));
    }

    #[test]
    fn the_alias_listing_names_who_is_behind_the_alias() {
        // Rendering a member under their own alias would repeat the alias back and leave
        // the caller unable to check whether it names the right person.
        let mut directory = kasia();
        directory
            .aliases
            .insert(("me".into(), "2".into()), "Kasia".into());
        let listing = render_aliases(
            &[Alias {
                alias: "Kasia".into(),
                note: Some("padel".into()),
                members: vec![Identity {
                    account_id: "me".into(),
                    ig_user_id: "2".into(),
                }],
            }],
            &directory,
        );
        assert!(listing.contains("Katarzyna Zawadzka"), "{listing}");
        assert!(listing.contains("padel"), "{listing}");
        assert!(listing.contains("[member me:2]"), "{listing}");
        // The alias appears once, as the label, and never as the name of its own member.
        assert_eq!(listing.matches("Kasia").count(), 1, "{listing}");
    }

    #[test]
    fn an_alias_with_no_members_says_so_rather_than_rendering_blank() {
        let listing = render_aliases(
            &[Alias {
                alias: "Kasia".into(),
                note: None,
                members: Vec::new(),
            }],
            &directory(),
        );
        assert!(listing.contains("(no members)"), "{listing}");
    }

    #[test]
    fn no_aliases_says_so() {
        assert_eq!(render_aliases(&[], &directory()), "no aliases set");
    }

    #[test]
    fn share_owners_are_listed_with_their_counts() {
        let out = render_owners(
            &[ShareOwner {
                owner: "surfclub".into(),
                shares: 12,
                last_shared_at: Some(base()),
            }],
            "authors",
        );
        assert!(out.contains("@surfclub — 12 share(s)"), "{out}");
        assert!(render_owners(&[], "authors").contains("(nobody has shared anything yet)"));
    }

    fn status_row() -> AccountStatus {
        AccountStatus {
            account_id: "me".into(),
            username: Some("myself".into()),
            enabled: true,
            last_auth_error: None,
            last_auth_error_at: None,
            threads: 4,
            threads_pending_backfill: 2,
            messages: 100,
            system_events: 12,
            unreadable: 5,
            shares: 48,
            participants: 7,
            oldest_message_at: Some(base()),
            newest_message_at: Some(base()),
        }
    }

    #[test]
    fn status_reports_the_corpus_and_the_backfill_debt() {
        let out = render_status(&[status_row()], &[], &directory());
        assert!(out.contains("48 share(s)"), "{out}");
        assert!(out.contains("2 thread(s) still owing backfill"), "{out}");
        assert!(out.contains("no loop has reported a run yet"), "{out}");
    }

    #[test]
    fn status_says_out_loud_what_it_declined_to_count_as_a_message() {
        // Without these lines a caller comparing 100 against Instagram's own screen has
        // no way to learn where the difference went.
        let out = render_status(&[status_row()], &[], &directory());
        assert!(out.contains("100 message(s)"), "{out}");
        assert!(out.contains("12 reaction/call event(s)"), "{out}");
        assert!(
            out.contains("5 message(s) Instagram would not hand over"),
            "{out}"
        );
    }

    #[test]
    fn status_stays_quiet_when_there_is_nothing_uncounted() {
        let out = render_status(
            &[AccountStatus {
                system_events: 0,
                unreadable: 0,
                ..status_row()
            }],
            &[],
            &directory(),
        );
        assert!(!out.contains("reaction/call event(s)"), "{out}");
        assert!(!out.contains("would not hand over"), "{out}");
    }

    #[test]
    fn a_dead_session_is_reported_as_frozen_not_as_a_transient_fault() {
        let out = render_status(
            &[AccountStatus {
                last_auth_error: Some("checkpoint_required".into()),
                last_auth_error_at: Some(base()),
                ..status_row()
            }],
            &[],
            &directory(),
        );
        assert!(out.contains("SESSION FAILING"), "{out}");
        assert!(out.contains("until a human mints a fresh session"), "{out}");
    }

    #[test]
    fn a_loop_error_is_shown_against_the_thread_it_was_working_on() {
        let mut directory = directory();
        directory.threads.insert(
            ("me".into(), "T1".into()),
            DirectoryThread {
                title: Some("Padel".into()),
                is_group: true,
                participant_ids: vec!["1".into(), "2".into()],
            },
        );
        let out = render_status(
            &[status_row()],
            &[SyncLoopStatus {
                account_id: "me".into(),
                loop_name: "backfill".into(),
                current_thread_id: Some("T1".into()),
                last_run_at: Some(base()),
                last_error: Some("429".into()),
                last_error_at: Some(base()),
            }],
            &directory,
        );
        assert!(out.contains("last working on Padel"), "{out}");
        assert!(out.contains("ERROR at") && out.contains("429"), "{out}");
    }

    #[test]
    fn status_before_the_ingester_has_ever_run() {
        assert!(render_status(&[], &[], &directory()).starts_with("no account registered yet"));
    }

    #[test]
    fn revisions_show_the_deleted_text_and_say_what_revision_one_means() {
        let out = render_revisions(
            &[
                Revision {
                    seq: 1,
                    text: Some("before".into()),
                    first_seen_at: None,
                    superseded_at: Some(base()),
                },
                Revision {
                    seq: 2,
                    text: Some("after".into()),
                    first_seen_at: Some(base()),
                    superseded_at: None,
                },
            ],
            "h",
            Some(Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap()),
        );
        assert!(out.contains("before"), "{out}");
        assert!(out.contains("unknown ->"), "{out}");
        assert!(out.contains("deleted "), "{out}");
        assert!(out.contains("not necessarily what was first sent"), "{out}");
    }

    #[test]
    fn revisions_are_not_an_error_when_there_is_no_history() {
        assert!(render_revisions(&[], "h", None).contains("no recorded changes"));
    }

    #[test]
    fn a_revision_with_no_text_says_so() {
        let out = render_revisions(
            &[Revision {
                seq: 1,
                text: None,
                first_seen_at: Some(base()),
                superseded_at: None,
            }],
            "h",
            None,
        );
        assert!(out.contains("\"(no text)\""), "{out}");
    }

    #[test]
    fn a_capped_page_says_how_many_more_the_period_holds() {
        let first = Utc.with_ymd_and_hms(2026, 7, 22, 11, 55, 2).unwrap();
        let last = Utc.with_ymd_and_hms(2026, 7, 26, 9, 0, 0).unwrap();

        let forward = window_note(405, first, last, true);
        let backward = window_note(405, first, last, false);

        assert!(forward[0].contains("405 more message(s)"), "{}", forward[0]);
        assert!(
            forward[0].contains("after=2026-07-26T08:59:59Z"),
            "{}",
            forward[0]
        );
        assert!(
            backward[0].contains("405 older message(s)"),
            "{}",
            backward[0]
        );
        assert!(
            backward[0].contains("before=2026-07-22T11:55:03Z"),
            "{}",
            backward[0]
        );
        assert!(window_note(0, first, last, true).is_empty());
    }
}
