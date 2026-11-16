//! What the loops ask Instagram, and the [`Source`] that answers over plain HTTPS.
//!
//! The Python ingester read these through `InstagramClient`, which drove Chromium; the
//! loops only ever used three calls, so they are a trait here and the tests drive the
//! loops through a fake, as `tests/test_ingest_loops.py` did.
//!
//! [`WebSource`] keeps the client's semantics. The page is loaded once per pass (the
//! harvester restarted Chromium for every pass, which reloaded it), the inbox is cached
//! for that pass only, and every request (the page and each query) first takes a token
//! from the bucket.

use std::collections::HashSet;
use std::sync::Arc;

use instagram_client::graphql::{self, Doc};
use instagram_client::page::PageConfig;
use instagram_client::web::{Web, WebError};
use serde_json::{Value, json};

use crate::listen::Nudge;
use crate::normalize;
use crate::ratelimit::TokenBucket;

/// The calls the poll and backfill loops make.
#[allow(async_fn_in_trait)]
pub trait Source {
    /// The signed-in user as a `viewer` object: `{"id", "username"}`.
    async fn me(&mut self) -> Result<Value, WebError>;

    /// Every thread the inbox lists, as `as_ig_direct_thread` objects in Instagram's own
    /// newest-first order, each `thread_id` once. Cached until [`Source::end_pass`].
    async fn inbox(&mut self) -> Result<Vec<Value>, WebError>;

    /// The inbox's first page alone, where a thread that just moved sits: what a pass the
    /// live socket triggers reads. Not cached.
    async fn inbox_head(&mut self) -> Result<Vec<Value>, WebError> {
        self.inbox().await
    }

    /// One conversation's pages, newest first, and whether `until` was reached.
    ///
    /// The newest page always, then up to `pages` older ones: `pages = 0` is the newest
    /// page alone. Paging stops early at the page that holds the message `until`, or at
    /// one with nothing older behind it. Each page has the thread query's shape, so
    /// `normalize::messages_from_thread` reads them all.
    async fn thread(
        &mut self,
        thread_fbid: &str,
        until: Option<&str>,
        pages: usize,
        reserve: f64,
    ) -> Result<(Vec<Value>, bool), WebError>;

    /// Forget this pass's page and inbox, as the harvester's `aclose()` did. Returns the
    /// cookie jar's JSON when Instagram rotated a cookie, for the caller to save.
    fn end_pass(&mut self) -> Option<String>;
}

pub struct WebSource {
    web: Web,
    bucket: Arc<TokenBucket>,
    nudge: Arc<Nudge>,
    config: Option<PageConfig>,
    inbox: Option<Vec<Value>>,
}

impl WebSource {
    pub fn new(web: Web, bucket: Arc<TokenBucket>, nudge: Arc<Nudge>) -> Self {
        Self {
            web,
            bucket,
            nudge,
            config: None,
            inbox: None,
        }
    }

    async fn config(&mut self, reserve: f64) -> Result<PageConfig, WebError> {
        if let Some(config) = &self.config {
            return Ok(config.clone());
        }
        self.bucket.acquire(reserve).await;
        let config = self.web.page().await?;
        self.config = Some(config.clone());
        Ok(config)
    }

    async fn ask(
        &mut self,
        config: &PageConfig,
        reserve: f64,
        doc: &Doc,
        field: &str,
        variables: Value,
    ) -> Result<Option<Value>, WebError> {
        self.bucket.acquire(reserve).await;
        self.web.query(config, doc, field, &variables).await
    }

    /// The inbox's pages in order, or with `all` false its first page alone.
    async fn walk_inbox(&mut self, all: bool) -> Result<Vec<Value>, WebError> {
        let config = self.config(0.0).await?;
        let mut connection = self
            .ask(
                &config,
                0.0,
                &graphql::INBOX,
                graphql::INBOX_FIELD,
                graphql::inbox_variables(&config),
            )
            .await?
            .and_then(|data| data.get("threads_by_folder").cloned());

        let mut threads = Vec::new();
        let mut seen = HashSet::new();
        let mut cursors = HashSet::new();
        while let Some(page) = connection.take() {
            let edges = page.get("edges").and_then(Value::as_array);
            for raw in edges.into_iter().flatten() {
                let Some(raw) = raw
                    .pointer("/node/as_ig_direct_thread")
                    .filter(|r| r.is_object())
                else {
                    continue;
                };
                // A thread that moves mid-walk can appear on two pages.
                if let Some(thread_id) = id(raw.get("thread_id"))
                    && seen.insert(thread_id)
                {
                    threads.push(raw.clone());
                }
            }
            let info = page.get("page_info");
            if !all
                || info
                    .and_then(|i| i.get("has_next_page"))
                    .and_then(Value::as_bool)
                    != Some(true)
            {
                break;
            }
            let Some(cursor) = info
                .and_then(|i| i.get("end_cursor"))
                .and_then(Value::as_str)
            else {
                break;
            };
            let Some(viewer_fbid) = config.viewer_fbid.as_deref() else {
                tracing::warn!(
                    "the page carries no IG_USER_EIMU; the inbox stops at its first page"
                );
                break;
            };
            // A cursor seen before would walk the same pages forever.
            if !cursors.insert(cursor.to_owned()) {
                break;
            }
            connection = self
                .ask(
                    &config,
                    0.0,
                    &graphql::THREAD_LIST,
                    graphql::THREAD_LIST_FIELD,
                    graphql::thread_list_variables(viewer_fbid, cursor),
                )
                .await?
                .and_then(|data| data.get("threads_by_folder").cloned());
        }
        Ok(threads)
    }
}

/// A string or numeric id as a string.
fn id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Whether a page holds the message `until`.
fn holds(page: &Value, until: Option<&str>) -> bool {
    let Some(until) = until else {
        return false;
    };
    page.pointer("/slide_messages/edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|edge| id(edge.pointer("/node/message_id")).as_deref() == Some(until))
}

impl Source for WebSource {
    async fn me(&mut self) -> Result<Value, WebError> {
        let config = self.config(0.0).await?;
        Ok(json!({"id": config.viewer_id, "username": config.viewer_username}))
    }

    async fn inbox(&mut self) -> Result<Vec<Value>, WebError> {
        if let Some(threads) = &self.inbox {
            return Ok(threads.clone());
        }
        let threads = self.walk_inbox(true).await?;
        self.inbox = Some(threads.clone());
        Ok(threads)
    }

    async fn inbox_head(&mut self) -> Result<Vec<Value>, WebError> {
        match &self.inbox {
            Some(threads) => Ok(threads.clone()),
            None => self.walk_inbox(false).await,
        }
    }

    async fn thread(
        &mut self,
        thread_fbid: &str,
        until: Option<&str>,
        pages: usize,
        reserve: f64,
    ) -> Result<(Vec<Value>, bool), WebError> {
        // A caller that yields the bucket (backfill) yields to a waiting change too: it
        // stops before the next page, keeps what it has, and resumes on a later pass.
        let yields = reserve > 0.0;
        if yields && self.nudge.pending() {
            return Ok((Vec::new(), false));
        }
        let config = self.config(reserve).await?;
        let detail = self
            .ask(
                &config,
                reserve,
                &graphql::THREAD,
                graphql::THREAD_FIELD,
                graphql::thread_variables(thread_fbid),
            )
            .await?;
        // `get_slide_thread_nullable: null` is a thread that is gone.
        let Some(newest) = detail
            .as_ref()
            .and_then(|d| d.get("as_ig_direct_thread"))
            .filter(|t| id(t.get("thread_id")).is_some())
        else {
            return Ok((Vec::new(), false));
        };
        // The older pages answer with a bare connection, so the newest page's thread
        // fields are carried onto them: `normalize` reads `thread_id` from there.
        let mut shell = newest.clone();
        if let Some(fields) = shell.as_object_mut() {
            fields.remove("slide_messages");
        }
        let mut reached = holds(newest, until);
        let mut out = vec![newest.clone()];
        while !reached && out.len() <= pages && !(yields && self.nudge.pending()) {
            let Some(cursor) = out.last().and_then(normalize::older_page_cursor) else {
                break;
            };
            let older = self
                .ask(
                    &config,
                    reserve,
                    &graphql::MESSAGES,
                    graphql::MESSAGES_FIELD,
                    graphql::messages_variables(thread_fbid, &cursor),
                )
                .await?;
            let Some(connection) = older
                .as_ref()
                .and_then(|o| o.pointer("/as_ig_direct_thread/slide_messages"))
                .filter(|c| c.is_object())
            else {
                break;
            };
            let mut page = shell.clone();
            page["slide_messages"] = connection.clone();
            reached = holds(&page, until);
            out.push(page);
        }
        Ok((out, reached))
    }

    fn end_pass(&mut self) -> Option<String> {
        self.config = None;
        self.inbox = None;
        self.web.take_rotated().then(|| self.web.jar().to_json())
    }
}
