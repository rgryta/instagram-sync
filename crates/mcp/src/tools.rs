//! The eight tools, as plain async functions over a pool.
//!
//! Keeping them out of the server object is what lets the tests call them directly and
//! keeps the MCP wiring to registration. Nothing here holds SQL — every statement lives
//! in `instagram_core::store` — and nothing in this crate depends on `instagram-client`:
//! this process holds no Instagram credential and the linker makes reaching Instagram
//! impossible rather than merely discouraged.
//!
//! What a tool does here is resolve the caller's words into arguments and choose the
//! header.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use instagram_core::models::Identity;
use instagram_core::store::{self, Directory, Resolved};
use sqlx::postgres::PgPool;

use crate::render::{
    quoted, render_message, render_messages, render_owners, render_people, render_revisions,
    render_status, render_threads, truncation_note, when, window_note,
};

/// A caller asking for ten thousand rows gets two hundred: a page that does not fit the
/// context window is not a better answer, and `truncation_note` says so out loud.
pub const MAX_LIMIT: i64 = 200;
pub const DEFAULT_LIMIT: i64 = 50;
/// Search defaults smaller because each hit costs a headline line as well as a message.
pub const SEARCH_LIMIT: i64 = 25;
/// How many authors the shares tool offers when none was named.
const OWNER_LIMIT: i64 = 20;

/// The four values `ingest.normalize` produces. Named here so a typo is answered with
/// the list rather than with an empty result that reads like "nobody ever shared a
/// reel".
pub const SHARE_KINDS: [&str; 4] = ["reel", "post", "story", "link"];

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The caller's fault, and reported back to them with the reason.
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
}

type Result<T> = std::result::Result<T, ToolError>;

pub fn clamp(limit: Option<i64>, default: i64) -> i64 {
    match limit {
        Some(limit) if limit >= 1 => limit.min(MAX_LIMIT),
        _ => default,
    }
}

/// ISO 8601 or a Unix epoch. What this server prints, it also accepts.
///
/// A naive timestamp is read as UTC, because UTC is what every rendered line says — and
/// because that is what `parse_when` in the Python does, unlike its slack-sync sibling,
/// which reads one in the container's own timezone.
pub fn parse_when(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    // `datetime.fromisoformat` takes a space in place of the `T` and `Z` in place of
    // `+00:00`; chrono's RFC 3339 parser takes neither.
    let normalised = value.replacen(' ', "T", 1).replace('Z', "+00:00");
    if let Ok(moment) = DateTime::parse_from_rfc3339(&normalised) {
        return Ok(Some(moment.with_timezone(&Utc)));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalised, format) {
            return Ok(Some(naive.and_utc()));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(Some(
            date.and_hms_opt(0, 0, 0)
                .expect("midnight exists")
                .and_utc(),
        ));
    }
    // A Unix epoch, which is what `str(datetime.timestamp())` prints.
    if let Ok(epoch) = value.parse::<f64>() {
        let seconds = epoch.trunc() as i64;
        let nanos = ((epoch - epoch.trunc()) * 1e9).round() as u32;
        if let Some(moment) = DateTime::from_timestamp(seconds, nanos) {
            return Ok(Some(moment));
        }
    }
    Err(ToolError::Usage(format!(
        "could not read {} as a time",
        quoted(value)
    )))
}

async fn directory_of(pool: &PgPool) -> Result<Directory> {
    Ok(Directory::load(pool).await?)
}

/// Reject a `share_kind` that does not exist, with the list.
///
/// An *empty* one is passed straight through to the query, which is what the Python
/// does — `if share_kind and share_kind not in SHARE_KINDS` lets `""` past the guard,
/// and `share_kind = ''` then matches no row. Reproduced rather than improved so the
/// two servers answer the same bytes; it is a silly input either way.
fn share_kind_filter(share_kind: Option<&str>) -> std::result::Result<Option<&str>, String> {
    match share_kind {
        Some(kind) if !kind.is_empty() && !SHARE_KINDS.contains(&kind) => Err(format!(
            "unknown share kind {} — one of: {}",
            quoted(kind),
            SHARE_KINDS.join(", ")
        )),
        other => Ok(other),
    }
}

// ----------------------------------------------------------------------------- tools

/// Conversations, most recently active first.
pub async fn threads(
    pool: &PgPool,
    account: Option<&str>,
    since: Option<&str>,
    group: Option<bool>,
    limit: Option<i64>,
) -> Result<String> {
    let moment = parse_when(since)?;
    let directory = directory_of(pool).await?;
    let page =
        store::list_threads(pool, account, moment, group, clamp(limit, DEFAULT_LIMIT)).await?;
    let kind = match group {
        None => "",
        Some(true) => "group ",
        Some(false) => "one-to-one ",
    };
    let header = match moment {
        None => format!(
            "{} {kind}conversation(s), most recently active first",
            page.threads.len()
        ),
        // Said explicitly: with `since` the per-thread count is what arrived after that
        // instant, not the thread's whole stored history, and the two differ by orders
        // of magnitude on an old conversation.
        Some(moment) => format!(
            "{} {kind}conversation(s) active since {} — counts cover only what arrived after it",
            page.threads.len(),
            when(Some(moment))
        ),
    };
    Ok(render_threads(
        &page.threads,
        page.total_matches,
        &directory,
        &header,
    ))
}

/// One thread's history, oldest first: the oldest page after `after`, else the newest.
pub async fn messages(
    pool: &PgPool,
    thread_id: &str,
    account: Option<&str>,
    after: Option<&str>,
    before: Option<&str>,
    limit: Option<i64>,
) -> Result<String> {
    let directory = directory_of(pool).await?;
    let after_at = parse_when(after)?;
    let before_at = parse_when(before)?;
    let page = store::get_history(
        pool,
        thread_id,
        account,
        after_at,
        before_at,
        clamp(limit, DEFAULT_LIMIT),
    )
    .await?;
    let label = directory.any_thread_label(thread_id, account);
    let header = format!(
        "{label} [thread {thread_id}] — {} {} message(s), oldest first",
        if after_at.is_some() {
            "oldest"
        } else {
            "newest"
        },
        page.messages.len()
    );
    let footer = match (page.messages.first(), page.messages.last()) {
        (Some(first), Some(last)) => window_note(
            page.total_matches - page.messages.len() as i64,
            first.ts,
            last.ts,
            after_at.is_some(),
        ),
        _ => Vec::new(),
    };
    Ok(render_messages(
        &page.messages,
        &directory,
        &header,
        // The thread is already in the header; repeating it on every line is noise.
        false,
        &footer,
    ))
}

/// Full text over the stored corpus, best match first.
///
/// `person` narrows by sender and takes an alias, a handle, a whole display name or a
/// raw id. It resolves whole-string: a filter that quietly widened to everyone sharing
/// a first name would return a plausible answer to a question nobody asked.
#[allow(clippy::too_many_arguments)]
pub async fn search(
    pool: &PgPool,
    query: &str,
    account: Option<&str>,
    thread_id: Option<&str>,
    person: Option<&str>,
    share_kind: Option<&str>,
    after: Option<&str>,
    before: Option<&str>,
    limit: Option<i64>,
) -> Result<String> {
    if query.trim().is_empty() {
        return Ok("a search needs something to search for".to_owned());
    }
    let kind = match share_kind_filter(share_kind) {
        Ok(kind) => kind,
        Err(message) => return Ok(message),
    };
    let mut senders: Option<Vec<Identity>> = None;
    // An empty `person` is no filter at all, not a person called "": the Python guards
    // this with `if person:`, and resolving `""` would match everyone whose handle and
    // display name are both unset.
    if let Some(person) = person.filter(|person| !person.is_empty()) {
        let resolved = store::resolve_people(pool, person).await?;
        if resolved.is_empty() {
            return Ok(format!(
                "no person or alias matched {} — the people tool takes half a name",
                quoted(person)
            ));
        }
        senders = Some(resolved);
    }
    let directory = directory_of(pool).await?;
    let page = store::search_messages(
        pool,
        query,
        account,
        thread_id,
        senders.as_deref(),
        kind,
        parse_when(after)?,
        parse_when(before)?,
        clamp(limit, SEARCH_LIMIT),
    )
    .await?;

    let mut lines = vec![format!(
        "{} result(s) for {}, best match first",
        page.hits.len(),
        quoted(query)
    )];
    if page.hits.is_empty() {
        lines.push("(nothing matched)".to_owned());
    }
    for hit in &page.hits {
        lines.push(render_message(&hit.message, &directory, true));
        // The headline is worth a line only when it is not simply the message again,
        // which it is for anything short enough to render whole.
        if let Some(snippet) = hit
            .snippet
            .as_deref()
            .filter(|snippet| !snippet.is_empty() && *snippet != hit.message.text.as_str())
        {
            lines.push(format!("    …{snippet}…"));
        }
    }
    if !page.hits.is_empty() {
        lines.extend(truncation_note(page.hits.len() as i64, page.total_matches));
    }
    Ok(lines.join("\n"))
}

/// Who is in the corpus, matched on half a handle, half a name or an alias.
///
/// The aliases in force are printed underneath rather than behind a tool of their own:
/// a caller looking someone up is exactly the caller who needs to know that `kasia`
/// already names somebody, and one round trip is cheaper than teaching them a second
/// tool exists.
pub async fn people(pool: &PgPool, name: Option<&str>, limit: Option<i64>) -> Result<String> {
    let directory = directory_of(pool).await?;
    let found = store::find_people(pool, name, clamp(limit, DEFAULT_LIMIT)).await?;
    let aliases = store::list_aliases(pool, None).await?;
    Ok(render_people(&found, &directory, &aliases))
}

/// Every reel, post, story and link ever sent or received, newest first.
///
/// This is the question no Instagram client can answer, and roughly half of this corpus
/// is one of these rather than typed text.
///
/// With no `owner` filter the top authors are listed underneath. Without that the tool
/// only answers questions whose answer the caller already knew: `share_owner` is a
/// handle nobody has memorised.
#[allow(clippy::too_many_arguments)]
pub async fn shares(
    pool: &PgPool,
    owner: Option<&str>,
    share_kind: Option<&str>,
    thread_id: Option<&str>,
    account: Option<&str>,
    after: Option<&str>,
    before: Option<&str>,
    limit: Option<i64>,
) -> Result<String> {
    let kind = match share_kind_filter(share_kind) {
        Ok(kind) => kind,
        Err(message) => return Ok(message),
    };
    let directory = directory_of(pool).await?;
    let after_at = parse_when(after)?;
    let before_at = parse_when(before)?;
    let page = store::list_shares(
        pool,
        account,
        owner,
        kind,
        thread_id,
        after_at,
        before_at,
        clamp(limit, DEFAULT_LIMIT),
    )
    .await?;

    let filters = [
        owner.map(|owner| format!("by @{owner}")),
        kind.map(|kind| format!("kind {kind}")),
        thread_id.map(|thread_id| format!("in thread {thread_id}")),
        after_at.map(|moment| format!("after {}", when(Some(moment)))),
        before_at.map(|moment| format!("before {}", when(Some(moment)))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");

    let mut footer = if page.messages.is_empty() {
        Vec::new()
    } else {
        truncation_note(page.messages.len() as i64, page.total_matches)
    };
    if owner.is_none() {
        footer.push(String::new());
        footer.push(render_owners(
            &store::share_owners(pool, account, OWNER_LIMIT).await?,
            "most-shared authors — pass one as `owner`",
        ));
    }
    let header = format!(
        "{} share(s){}, newest first",
        page.messages.len(),
        if filters.is_empty() {
            String::new()
        } else {
            format!(" {filters}")
        }
    );
    Ok(render_messages(
        &page.messages,
        &directory,
        &header,
        true,
        &footer,
    ))
}

/// How much is stored, how much history is still owed, and whether ingest is stuck.
pub async fn status(pool: &PgPool) -> Result<String> {
    let directory = directory_of(pool).await?;
    let accounts = store::account_status(pool).await?;
    let loops = store::sync_loops(pool).await?;
    Ok(render_status(&accounts, &loops, &directory))
}

/// The one tool that writes, and it writes only the two alias tables.
///
/// A member already attached to another alias is moved, and the move is reported rather
/// than applied in silence.
pub async fn set_alias(
    pool: &PgPool,
    alias: &str,
    members: &[String],
    note: Option<&str>,
) -> Result<String> {
    let mut identities: Vec<Identity> = Vec::new();
    for member in members {
        match member.split_once(':') {
            Some((account_id, ig_user_id)) if !account_id.is_empty() && !ig_user_id.is_empty() => {
                identities.push(Identity {
                    account_id: account_id.to_owned(),
                    ig_user_id: ig_user_id.to_owned(),
                });
            }
            _ => {
                return Err(ToolError::Usage(format!(
                    "member {} must be written as account:ig_user_id",
                    quoted(member)
                )));
            }
        }
    }

    if identities.is_empty() {
        let removed = store::delete_alias(pool, alias).await?;
        return Ok(format!(
            "alias {} {}",
            quoted(alias),
            if removed { "deleted" } else { "did not exist" }
        ));
    }

    let moved = store::set_alias(pool, alias, &identities, note).await?;

    // Loaded after the write, and members are named as Instagram knows them: echoing
    // the alias back would leave the caller unable to check it names the right person.
    let directory = directory_of(pool).await?;
    let named = identities
        .iter()
        .map(|i| {
            format!(
                "{} [member {}:{}]",
                directory.instagram_name(&i.account_id, &i.ig_user_id),
                i.account_id,
                i.ig_user_id
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut lines = vec![format!("alias {} now names {named}", quoted(alias))];
    if !moved.is_empty() {
        lines.push(format!(
            "moved off another alias: {}",
            moved
                .iter()
                .map(|i| format!("{}:{}", i.account_id, i.ig_user_id))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(lines.join("\n"))
}

/// One message's text over time, and the deleted text if it was deleted.
pub async fn revisions(
    pool: &PgPool,
    thread_id: &str,
    item_id: &str,
    account: Option<&str>,
) -> Result<String> {
    let directory = directory_of(pool).await?;
    let account = account.filter(|account| !account.is_empty());
    let Some(account_id) = directory.account_of_thread(thread_id, account) else {
        return Ok(format!("no thread {thread_id}"));
    };
    let resolved = store::resolve_message(pool, &account_id, thread_id, item_id).await?;
    let Resolved::Message { deleted_at } = resolved else {
        return Ok(format!("no message {item_id} in thread {thread_id}"));
    };
    let rows = store::list_revisions(pool, &account_id, thread_id, item_id).await?;
    Ok(render_revisions(
        &rows,
        &format!("{} revision(s) of {item_id}, oldest first", rows.len()),
        deleted_at,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn a_caller_asking_for_ten_thousand_rows_gets_the_ceiling() {
        assert_eq!(clamp(Some(10_000), DEFAULT_LIMIT), MAX_LIMIT);
        assert_eq!(clamp(None, DEFAULT_LIMIT), DEFAULT_LIMIT);
        assert_eq!(clamp(Some(0), DEFAULT_LIMIT), DEFAULT_LIMIT);
        assert_eq!(clamp(Some(-5), DEFAULT_LIMIT), DEFAULT_LIMIT);
        assert_eq!(clamp(Some(7), DEFAULT_LIMIT), 7);
        assert_eq!(clamp(None, SEARCH_LIMIT), SEARCH_LIMIT);
    }

    #[test]
    fn an_iso_stamp_is_read_with_its_offset() {
        let parsed = parse_when(Some("2026-08-01T14:00:00+02:00")).unwrap();
        assert_eq!(
            parsed,
            Some(Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap())
        );
    }

    #[test]
    fn a_naive_stamp_is_read_as_utc_not_as_local_time() {
        // Every line this server prints is UTC, so a caller echoing one back must get
        // the instant they were shown. The container sets `TZ=Europe/Warsaw`; reading
        // these as local would shift every one of them by two hours.
        let noon = Some(Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap());
        assert_eq!(parse_when(Some("2026-08-01T12:00")).unwrap(), noon);
        assert_eq!(parse_when(Some("2026-08-01T12:00:00")).unwrap(), noon);
        assert_eq!(parse_when(Some("2026-08-01 12:00:00")).unwrap(), noon);
        assert_eq!(parse_when(Some("2026-08-01T12:00:00.000")).unwrap(), noon);
        assert_eq!(
            parse_when(Some("2026-08-01")).unwrap(),
            Some(Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap())
        );
    }

    #[test]
    fn a_z_suffix_is_an_offset() {
        assert_eq!(
            parse_when(Some("2026-08-01T12:00:00Z")).unwrap(),
            Some(Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap())
        );
    }

    #[test]
    fn an_epoch_is_read_as_readily_as_an_iso_stamp() {
        let moment = Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap();
        assert_eq!(
            parse_when(Some(&format!("{}.0", moment.timestamp()))).unwrap(),
            Some(moment)
        );
        assert_eq!(
            parse_when(Some(&moment.timestamp().to_string())).unwrap(),
            Some(moment)
        );
    }

    #[test]
    fn nothing_is_not_a_time() {
        assert_eq!(parse_when(None).unwrap(), None);
        assert_eq!(parse_when(Some("")).unwrap(), None);
        assert_eq!(parse_when(Some("   ")).unwrap(), None);
    }

    #[test]
    fn an_unreadable_timestamp_is_a_usage_error_naming_what_was_passed() {
        let error = parse_when(Some("last tuesday")).unwrap_err();
        assert_eq!(error.to_string(), "could not read 'last tuesday' as a time");
    }

    #[test]
    fn an_unknown_share_kind_is_answered_with_the_list() {
        let message = share_kind_filter(Some("tweet")).unwrap_err();
        assert_eq!(
            message,
            "unknown share kind 'tweet' — one of: reel, post, story, link"
        );
        assert_eq!(share_kind_filter(Some("reel")).unwrap(), Some("reel"));
        assert_eq!(share_kind_filter(None).unwrap(), None);
        // Not an error, because it is not one in the Python either.
        assert_eq!(share_kind_filter(Some("")).unwrap(), Some(""));
    }
}
