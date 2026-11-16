//! Finding people: by half a name, and by a whole one.

use sqlx::postgres::PgPool;
use sqlx::{Row, query};

use super::fold;
use crate::models::{Identity, Person};

/// People matching a handle, a display name or an alias, with where to find them.
///
/// A substring match, unlike [`resolve_people`] below: this is the tool a caller
/// reaches for when they know half a name, and narrowing it to whole strings would
/// answer "who is Kasia" with nothing.
pub async fn find_people(
    pool: &PgPool,
    fragment: Option<&str>,
    limit: i64,
) -> Result<Vec<Person>, sqlx::Error> {
    let rows = query(concat!(
        "
        SELECT p.account_id, p.ig_user_id, p.username, p.full_name, p.is_verified,
               (SELECT string_agg(am.alias, ', ' ORDER BY am.alias)
                  FROM person_alias_member am
                 WHERE am.account_id = p.account_id AND am.ig_user_id = p.ig_user_id) AS aliases,
               (SELECT count(*) FROM thread t
                 WHERE t.account_id = p.account_id
                   AND p.ig_user_id = ANY (t.participant_ids)) AS thread_count,
               -- The one thread a caller can pass straight to the messages tool. A
               -- person in five group chats and no DM legitimately has none.
               (SELECT t.thread_id FROM thread t
                 WHERE t.account_id = p.account_id AND NOT t.is_group
                   AND p.ig_user_id = ANY (t.participant_ids)
                 ORDER BY t.last_activity_at DESC NULLS LAST
                 LIMIT 1) AS dm_thread_id,
               (SELECT count(*) FROM message m
                 WHERE m.account_id = p.account_id AND m.sender_id = p.ig_user_id
                   AND ",
        is_a_message!(),
        ") AS message_count
          FROM participant p
         WHERE $1::text IS NULL
            OR p.ig_user_id = $1
            OR position($2 IN ",
        folded!("coalesce(p.username, '')"),
        ") > 0
            OR position($2 IN ",
        folded!("coalesce(p.full_name, '')"),
        ") > 0
            OR EXISTS (SELECT 1 FROM person_alias_member am
                        WHERE am.account_id = p.account_id AND am.ig_user_id = p.ig_user_id
                          AND position($2 IN ",
        folded!("am.alias"),
        ") > 0)
         ORDER BY message_count DESC, p.username, p.ig_user_id
         LIMIT $3"
    ))
    .bind(fragment)
    .bind(fragment.map(fold))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(Person {
                account_id: row.try_get("account_id")?,
                ig_user_id: row.try_get("ig_user_id")?,
                username: row.try_get("username")?,
                full_name: row.try_get("full_name")?,
                is_verified: row.try_get("is_verified")?,
                aliases: row.try_get("aliases")?,
                thread_count: row.try_get("thread_count")?,
                message_count: row.try_get("message_count")?,
                dm_thread_id: row.try_get("dm_thread_id")?,
            })
        })
        .collect()
}

/// Resolve an alias, an id or a whole name to the identities it names.
///
/// Whole-string, case- and accent-insensitive rather than a substring: a filter that quietly
/// widened to everyone sharing a first name would return a plausible answer to a
/// question nobody asked. [`find_people`] is the tool for searching by half a name.
pub async fn resolve_people(pool: &PgPool, name: &str) -> Result<Vec<Identity>, sqlx::Error> {
    let name_folded = fold(name);
    let by_alias = query(concat!(
        "SELECT account_id, ig_user_id FROM person_alias_member WHERE ",
        folded!("alias"),
        " = $1"
    ))
    .bind(&name_folded)
    .fetch_all(pool)
    .await?;
    let rows = if by_alias.is_empty() {
        query(concat!(
            "
            SELECT account_id, ig_user_id FROM participant
             WHERE ig_user_id = $1
                OR $2 IN (",
            folded!("coalesce(username, '')"),
            ", ",
            folded!("coalesce(full_name, '')"),
            ")
             ORDER BY account_id, ig_user_id
            "
        ))
        .bind(name)
        .bind(&name_folded)
        .fetch_all(pool)
        .await?
    } else {
        by_alias
    };
    rows.iter()
        .map(|row| {
            Ok(Identity {
                account_id: row.try_get("account_id")?,
                ig_user_id: row.try_get("ig_user_id")?,
            })
        })
        .collect()
}
