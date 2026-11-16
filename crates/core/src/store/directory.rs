//! The lookup tables that turn ids into names, and the precedence rules that pick one.
//!
//! Loaded per request rather than cached: the whole corpus is a few dozen people and a
//! few dozen threads, and a cached directory would render someone under a handle they
//! have since changed with nothing on screen to signal it.

use std::collections::HashMap;

use sqlx::Row;
use sqlx::postgres::PgPool;

#[derive(Debug, Clone, Default)]
pub struct DirectoryPerson {
    /// The handle.
    pub username: Option<String>,
    /// What the Instagram client renders next to a message. Frequently unrelated to
    /// the handle, and neither substitutes for the other.
    pub full_name: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DirectoryThread {
    pub title: Option<String>,
    pub is_group: bool,
    pub participant_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DirectoryAccount {
    /// The only record of which participant is the account owner: a DM has no title,
    /// and naming one means naming the person who is not you.
    pub ig_user_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct Directory {
    pub people: HashMap<(String, String), DirectoryPerson>,
    pub aliases: HashMap<(String, String), String>,
    pub threads: HashMap<(String, String), DirectoryThread>,
    pub accounts: HashMap<String, DirectoryAccount>,
}

/// An empty string is an absence, not a name: "called nothing" must fall through to the
/// next source. Applied at lookup rather than at load, so a `Directory` built by hand —
/// in a test, or by a later caller — cannot carry a blank name past the precedence
/// rules.
fn named(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|text| !text.is_empty())
}

pub fn key(a: &str, b: &str) -> (String, String) {
    (a.to_owned(), b.to_owned())
}

impl Directory {
    /// Everything the renderer needs to turn ids into names, in four queries.
    pub async fn load(pool: &PgPool) -> Result<Self, sqlx::Error> {
        let mut directory = Self::default();

        let rows =
            sqlx::query("SELECT account_id, ig_user_id, username, full_name FROM participant")
                .fetch_all(pool)
                .await?;
        for row in rows {
            directory.people.insert(
                (row.try_get("account_id")?, row.try_get("ig_user_id")?),
                DirectoryPerson {
                    username: row.try_get("username")?,
                    full_name: row.try_get("full_name")?,
                },
            );
        }

        let rows = sqlx::query("SELECT account_id, ig_user_id, alias FROM person_alias_member")
            .fetch_all(pool)
            .await?;
        for row in rows {
            directory.aliases.insert(
                (row.try_get("account_id")?, row.try_get("ig_user_id")?),
                row.try_get("alias")?,
            );
        }

        let rows = sqlx::query(
            "SELECT account_id, thread_id, title, is_group, participant_ids FROM thread",
        )
        .fetch_all(pool)
        .await?;
        for row in rows {
            directory.threads.insert(
                (row.try_get("account_id")?, row.try_get("thread_id")?),
                DirectoryThread {
                    title: row.try_get("title")?,
                    is_group: row.try_get("is_group")?,
                    participant_ids: row.try_get("participant_ids")?,
                },
            );
        }

        let rows = sqlx::query("SELECT id, ig_user_id FROM account")
            .fetch_all(pool)
            .await?;
        for row in rows {
            directory.accounts.insert(
                row.try_get("id")?,
                DirectoryAccount {
                    ig_user_id: row.try_get("ig_user_id")?,
                },
            );
        }

        Ok(directory)
    }

    /// Alias, else full name, else handle, else the raw id.
    ///
    /// The precedence is fixed and is the tier order tested one by one. An alias is
    /// something the operator set deliberately, so it outranks anything Instagram
    /// supplies. `full_name` outranks `username` because it is what the Instagram
    /// client renders next to a message and therefore the only name a reader has ever
    /// seen; a handle like `k_zaw_92` names nobody they would recognise. The raw id is
    /// last and is never hidden — an unknown person renders as their id rather than as
    /// "unknown", so the caller still has something to look up.
    pub fn person_name(&self, account_id: &str, ig_user_id: Option<&str>) -> String {
        let Some(ig_user_id) = ig_user_id else {
            return "unknown".to_owned();
        };
        if let Some(alias) = self.aliases.get(&key(account_id, ig_user_id)) {
            return alias.clone();
        }
        self.instagram_name(account_id, ig_user_id)
    }

    /// The name Instagram knows, ignoring aliases.
    ///
    /// The alias listing needs this: rendering a member under their alias would repeat
    /// the alias back and say nothing about who is behind it.
    pub fn instagram_name(&self, account_id: &str, ig_user_id: &str) -> String {
        match self.people.get(&key(account_id, ig_user_id)) {
            Some(person) => named(&person.full_name)
                .or_else(|| named(&person.username))
                .unwrap_or(ig_user_id)
                .to_owned(),
            None => ig_user_id.to_owned(),
        }
    }

    pub fn is_me(&self, account_id: &str, ig_user_id: Option<&str>) -> bool {
        match (self.accounts.get(account_id), ig_user_id) {
            (Some(account), Some(ig_user_id)) => account.ig_user_id == ig_user_id,
            _ => false,
        }
    }

    /// Thread members who are not the account owner.
    pub fn others_in(&self, account_id: &str, thread_id: &str) -> Vec<&str> {
        match self.threads.get(&key(account_id, thread_id)) {
            Some(thread) => thread
                .participant_ids
                .iter()
                .map(String::as_str)
                .filter(|id| !self.is_me(account_id, Some(id)))
                .collect(),
            None => Vec::new(),
        }
    }

    /// A title if there is one, else the people in it.
    ///
    /// A DM has no title at all, so rendering one means naming the person on the other
    /// end — `340282366841710300000000000000000000042` is not something anyone can act
    /// on, and no other tool here will decode it.
    pub fn thread_label(&self, account_id: &str, thread_id: &str) -> String {
        let Some(thread) = self.threads.get(&key(account_id, thread_id)) else {
            return thread_id.to_owned();
        };
        if let Some(title) = named(&thread.title) {
            return title.to_owned();
        }
        let others = self.others_in(account_id, thread_id);
        if others.is_empty() {
            return thread_id.to_owned();
        }
        let names: Vec<String> = others
            .iter()
            .map(|id| self.person_name(account_id, Some(id)))
            .collect();
        if !thread.is_group {
            return format!("DM with {}", names[0]);
        }
        let shown = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        if names.len() > 3 {
            format!("group with {shown} and {} more", names.len() - 3)
        } else {
            format!("group with {shown}")
        }
    }

    /// Name a thread even when no account was named, and even when it holds nothing.
    ///
    /// Without an account there is no key into the directory, and falling back to a
    /// 39-digit Instagram thread id for the common "just read this conversation" call
    /// would make the header useless.
    pub fn any_thread_label(&self, thread_id: &str, account: Option<&str>) -> String {
        match self.account_of_thread(thread_id, account) {
            Some(account_id) => self.thread_label(&account_id, thread_id),
            None => thread_id.to_owned(),
        }
    }

    /// Which account holds a thread. Sorted, so two identical requests cannot differ
    /// just because a hash map iterated differently.
    pub fn account_of_thread(&self, thread_id: &str, account: Option<&str>) -> Option<String> {
        if let Some(account) = account {
            return Some(account.to_owned());
        }
        let mut owners: Vec<&String> = self
            .threads
            .keys()
            .filter(|(_, id)| id == thread_id)
            .map(|(account_id, _)| account_id)
            .collect();
        owners.sort();
        owners.first().map(|id| (*id).clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> Directory {
        let mut directory = Directory::default();
        directory.people.insert(
            key("me", "2"),
            DirectoryPerson {
                username: Some("k_zaw_92".into()),
                full_name: Some("Katarzyna Zawadzka".into()),
            },
        );
        directory.accounts.insert(
            "me".into(),
            DirectoryAccount {
                ig_user_id: "1".into(),
            },
        );
        directory
    }

    #[test]
    fn an_alias_beats_the_full_name() {
        let mut directory = directory();
        directory.aliases.insert(key("me", "2"), "Kasia".into());
        assert_eq!(directory.person_name("me", Some("2")), "Kasia");
    }

    #[test]
    fn the_full_name_beats_the_handle() {
        assert_eq!(
            directory().person_name("me", Some("2")),
            "Katarzyna Zawadzka"
        );
    }

    #[test]
    fn the_handle_is_used_when_there_is_no_full_name() {
        let mut directory = directory();
        directory.people.insert(
            key("me", "2"),
            DirectoryPerson {
                username: Some("k_zaw_92".into()),
                full_name: None,
            },
        );
        assert_eq!(directory.person_name("me", Some("2")), "k_zaw_92");
    }

    #[test]
    fn an_unknown_person_renders_as_their_raw_id() {
        // Never "unknown": the id is the key the caller still has something to do with.
        let directory = directory();
        assert_eq!(directory.person_name("me", Some("9")), "9");
        assert_eq!(directory.person_name("me", None), "unknown");
    }

    #[test]
    fn an_empty_name_field_falls_through_to_the_next_source() {
        let mut directory = directory();
        directory.people.insert(
            key("me", "2"),
            DirectoryPerson {
                username: Some("k_zaw_92".into()),
                full_name: Some(String::new()),
            },
        );
        assert_eq!(directory.instagram_name("me", "2"), "k_zaw_92");
    }

    #[test]
    fn the_instagram_name_ignores_an_alias() {
        let mut directory = directory();
        directory.aliases.insert(key("me", "2"), "Kasia".into());
        assert_eq!(directory.instagram_name("me", "2"), "Katarzyna Zawadzka");
    }

    #[test]
    fn a_dm_is_named_after_the_person_on_the_other_end() {
        let mut directory = directory();
        directory.threads.insert(
            key("me", "T1"),
            DirectoryThread {
                title: None,
                is_group: false,
                participant_ids: vec!["1".into(), "2".into()],
            },
        );
        assert_eq!(
            directory.thread_label("me", "T1"),
            "DM with Katarzyna Zawadzka"
        );
    }

    #[test]
    fn a_group_uses_its_title_when_it_has_one() {
        let mut directory = directory();
        directory.threads.insert(
            key("me", "T1"),
            DirectoryThread {
                title: Some("Padel".into()),
                is_group: true,
                participant_ids: vec!["1".into(), "2".into()],
            },
        );
        assert_eq!(directory.thread_label("me", "T1"), "Padel");
    }

    #[test]
    fn an_untitled_group_names_its_first_three_members_and_counts_the_rest() {
        let mut directory = directory();
        for n in 2..7 {
            directory.people.insert(
                key("me", &n.to_string()),
                DirectoryPerson {
                    username: None,
                    full_name: Some(format!("P{n}")),
                },
            );
        }
        directory.threads.insert(
            key("me", "T1"),
            DirectoryThread {
                title: None,
                is_group: true,
                participant_ids: (1..7).map(|n| n.to_string()).collect(),
            },
        );
        assert_eq!(
            directory.thread_label("me", "T1"),
            "group with P2, P3, P4 and 2 more"
        );
    }

    #[test]
    fn an_untitled_group_of_three_does_not_say_and_zero_more() {
        let mut directory = directory();
        directory.threads.insert(
            key("me", "T1"),
            DirectoryThread {
                title: None,
                is_group: true,
                participant_ids: vec!["1".into(), "2".into()],
            },
        );
        assert_eq!(
            directory.thread_label("me", "T1"),
            "group with Katarzyna Zawadzka"
        );
    }

    #[test]
    fn an_unknown_thread_renders_as_its_id() {
        assert_eq!(directory().thread_label("me", "T1"), "T1");
    }

    #[test]
    fn a_thread_is_named_even_with_no_account_given() {
        let mut directory = directory();
        directory.threads.insert(
            key("me", "T1"),
            DirectoryThread {
                title: Some("Padel".into()),
                is_group: true,
                participant_ids: vec!["1".into()],
            },
        );
        assert_eq!(directory.any_thread_label("T1", None), "Padel");
        assert_eq!(directory.any_thread_label("T-nope", None), "T-nope");
    }
}
