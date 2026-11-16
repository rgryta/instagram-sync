//! The two alias tables: the only rows the MCP role may write.
//!
//! The grant in `0001_init.sql` is what makes that a database fact rather than a
//! convention.

use sqlx::postgres::PgPool;
use sqlx::{Row, query};

use crate::models::{Alias, Identity};

/// Every alias and its members, alias order, members in id order.
pub async fn list_aliases(pool: &PgPool, alias: Option<&str>) -> Result<Vec<Alias>, sqlx::Error> {
    let rows = query(
        "
        SELECT a.alias, a.note, m.account_id, m.ig_user_id
          FROM person_alias a
          LEFT JOIN person_alias_member m ON m.alias = a.alias
         WHERE ($1::text IS NULL OR a.alias = $1)
         ORDER BY a.alias, m.account_id, m.ig_user_id
        ",
    )
    .bind(alias)
    .fetch_all(pool)
    .await?;

    let mut aliases: Vec<Alias> = Vec::new();
    for row in &rows {
        let name: String = row.try_get("alias")?;
        if aliases.last().map(|entry| &entry.alias) != Some(&name) {
            aliases.push(Alias {
                alias: name,
                note: row.try_get("note")?,
                members: Vec::new(),
            });
        }
        // A LEFT JOIN row with no member is an alias with none, not a member with no id.
        let account_id: Option<String> = row.try_get("account_id")?;
        if let Some(account_id) = account_id {
            aliases
                .last_mut()
                .expect("just pushed")
                .members
                .push(Identity {
                    account_id,
                    ig_user_id: row.try_get("ig_user_id")?,
                });
        }
    }
    Ok(aliases)
}

/// Create or replace an alias's member set, returning identities moved off another.
///
/// One identity belongs to at most one alias, so attaching one that is already attached
/// elsewhere is a move. The moved identities come back rather than being reassigned in
/// silence, so the caller can say so out loud.
pub async fn set_alias(
    pool: &PgPool,
    alias: &str,
    members: &[Identity],
    note: Option<&str>,
) -> Result<Vec<Identity>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    query(
        "
        INSERT INTO person_alias (alias, note) VALUES ($1, $2)
        ON CONFLICT (alias) DO UPDATE SET
          note = COALESCE(EXCLUDED.note, person_alias.note),
          updated_at = now()
        ",
    )
    .bind(alias)
    .bind(note)
    .execute(&mut *tx)
    .await?;

    let mut moved = Vec::new();
    if !members.is_empty() {
        let accounts: Vec<String> = members.iter().map(|m| m.account_id.clone()).collect();
        let ids: Vec<String> = members.iter().map(|m| m.ig_user_id.clone()).collect();
        let rows = query(
            "
            SELECT account_id, ig_user_id FROM person_alias_member
             WHERE alias <> $1
               AND (account_id, ig_user_id)
                   IN (SELECT * FROM unnest($2::text[], $3::text[]))
            ",
        )
        .bind(alias)
        .bind(&accounts)
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        for row in &rows {
            moved.push(Identity {
                account_id: row.try_get("account_id")?,
                ig_user_id: row.try_get("ig_user_id")?,
            });
        }
    }

    query("DELETE FROM person_alias_member WHERE alias = $1")
        .bind(alias)
        .execute(&mut *tx)
        .await?;

    if members.is_empty() {
        // An alias with no members resolves to nobody. Deleting it is more honest than
        // leaving a name behind that matches no one.
        query("DELETE FROM person_alias WHERE alias = $1")
            .bind(alias)
            .execute(&mut *tx)
            .await?;
    }
    // One statement per member rather than one `unnest` insert: a caller that names the
    // same member twice must land once, and a single `ON CONFLICT` statement cannot
    // touch the same row twice — it errors. This is `executemany`'s behaviour.
    for member in members {
        query(
            "
            INSERT INTO person_alias_member (account_id, ig_user_id, alias)
            VALUES ($1, $2, $3)
            ON CONFLICT (account_id, ig_user_id) DO UPDATE SET alias = EXCLUDED.alias
            ",
        )
        .bind(&member.account_id)
        .bind(&member.ig_user_id)
        .bind(alias)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(moved)
}

/// Remove an alias and, by cascade, its members. Messages are untouched.
pub async fn delete_alias(pool: &PgPool, alias: &str) -> Result<bool, sqlx::Error> {
    let deleted = query("DELETE FROM person_alias WHERE alias = $1")
        .bind(alias)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted == 1)
}
