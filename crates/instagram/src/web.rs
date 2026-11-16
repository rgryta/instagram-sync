//! instagram.com over HTTPS as a desktop Chrome, with the jar every response's cookies go
//! into. Only GETs of the inbox page and POSTs of the read queries in [`crate::graphql`].

use std::time::Duration;

use chrono::Utc;
use serde_json::Value;

use crate::cookies::Jar;
use crate::graphql::{self, Doc, GraphqlError};
use crate::page::{self, PageConfig, PageError};

pub const BASE: &str = "https://www.instagram.com";
pub const HOST: &str = "www.instagram.com";
const INBOX_PATH: &str = "/direct/inbox/";
/// mautrix-meta's `useragent.UserAgent`: a desktop Chrome on Linux.
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
const SEC_CH_UA: &str = r#""Chromium";v="141", "Google Chrome";v="141", "Not-A.Brand";v="99""#;
/// Pauses before each retry of a transport failure. Meta's edge resets HTTP/2 streams
/// mid-body (seen on messenger.com 2026-11-14/15); a browser just asks again.
const RETRY_PAUSES: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    /// The session is gone: only fresh cookies fix it.
    #[error("logged out: {0}")]
    LoggedOut(String),
    /// Instagram asked us to slow down (HTTP 429).
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("{0}")]
    Other(String),
}

/// The error and every source under it: reqwest's own message hides the HTTP/2 reason.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        text.push_str(": ");
        text.push_str(&next.to_string());
        source = next.source();
    }
    text
}

pub struct Web {
    http: reqwest::Client,
    jar: Jar,
    /// The `x-ig-set-www-claim` Instagram last handed out, sent back as `x-ig-www-claim`.
    www_claim: String,
    rotated: bool,
}

impl Web {
    pub fn new(jar: Jar) -> Result<Self, WebError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| WebError::Other(chain(&e)))?;
        Ok(Web {
            http,
            jar,
            www_claim: "0".into(),
            rotated: false,
        })
    }

    pub fn jar(&self) -> &Jar {
        &self.jar
    }

    /// Whether a response has set a cookie since the last call: the jar needs saving.
    pub fn take_rotated(&mut self) -> bool {
        std::mem::take(&mut self.rotated)
    }

    fn take_cookies(&mut self, headers: &reqwest::header::HeaderMap) {
        for value in headers.get_all("set-cookie") {
            if let Ok(v) = value.to_str() {
                self.jar.set(v, HOST, Utc::now());
                self.rotated = true;
            }
        }
        if let Some(claim) = headers
            .get("x-ig-set-www-claim")
            .and_then(|v| v.to_str().ok())
        {
            self.www_claim = claim.to_owned();
        }
    }

    fn browser(&self, request: reqwest::RequestBuilder, path: &str) -> reqwest::RequestBuilder {
        request
            .header("user-agent", USER_AGENT)
            .header("accept-language", "pl-PL,pl;q=0.9,en;q=0.8")
            .header("sec-ch-ua", SEC_CH_UA)
            .header("sec-ch-ua-mobile", "?0")
            .header("sec-ch-ua-platform", "\"Linux\"")
            .header("cookie", self.jar.header(HOST, path, Utc::now()))
    }

    /// The inbox page, tried again after a transport failure. A logged-out page is never
    /// retried: that answer is the session's.
    pub async fn page(&mut self) -> Result<PageConfig, WebError> {
        let mut pauses = RETRY_PAUSES.iter();
        loop {
            match self.page_once().await {
                Err(WebError::Other(reason)) => match pauses.next() {
                    Some(pause) => {
                        tracing::warn!("page load failed, trying again: {reason}");
                        tokio::time::sleep(*pause).await;
                    }
                    None => return Err(WebError::Other(reason)),
                },
                other => return other,
            }
        }
    }

    async fn page_once(&mut self) -> Result<PageConfig, WebError> {
        let response = self
            .browser(self.http.get(format!("{BASE}{INBOX_PATH}")), INBOX_PATH)
            .header(
                "accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("sec-fetch-dest", "document")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-site", "none")
            .header("sec-fetch-user", "?1")
            .header("upgrade-insecure-requests", "1")
            .send()
            .await
            .map_err(|e| WebError::Other(format!("page: {}", chain(&e))))?;
        let status = response.status();
        // Held back until the page proves logged in: a logged-out reply expires the
        // session's cookies, and taking them would turn a passing hiccup into a dead jar.
        let set_cookies = response.headers().clone();
        if status.is_redirection() {
            return Err(WebError::LoggedOut(format!(
                "the page redirected ({status})"
            )));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(WebError::RateLimited("page: HTTP 429".into()));
        }
        let html = response
            .text()
            .await
            .map_err(|e| WebError::Other(format!("page: {}", chain(&e))))?;
        if !status.is_success() {
            return Err(WebError::Other(format!("page: HTTP {status}")));
        }
        let config = page::scrape(&html).map_err(|e| match e {
            PageError::LoggedOut => WebError::LoggedOut(e.to_string()),
            e => WebError::Other(format!("page: {e}")),
        })?;
        self.take_cookies(&set_cookies);
        Ok(config)
    }

    /// `data.<field>` of one read query, tried again after a transport failure. Every
    /// [`Doc`] only reads, so a retry repeats nothing on the account.
    pub async fn query(
        &mut self,
        config: &PageConfig,
        doc: &Doc,
        field: &str,
        variables: &Value,
    ) -> Result<Option<Value>, WebError> {
        let mut pauses = RETRY_PAUSES.iter();
        loop {
            match self.query_once(config, doc, field, variables).await {
                Err(WebError::Other(reason)) => match pauses.next() {
                    Some(pause) => {
                        tracing::warn!("{} failed, trying again: {reason}", doc.name);
                        tokio::time::sleep(*pause).await;
                    }
                    None => return Err(WebError::Other(reason)),
                },
                other => return other,
            }
        }
    }

    async fn query_once(
        &mut self,
        config: &PageConfig,
        doc: &Doc,
        field: &str,
        variables: &Value,
    ) -> Result<Option<Value>, WebError> {
        let failed =
            |e: &dyn std::error::Error| WebError::Other(format!("{}: {}", doc.name, chain(e)));
        let response = self
            .browser(
                self.http.post(format!("{BASE}/api/graphql")),
                "/api/graphql",
            )
            .header("accept", "*/*")
            .header("origin", BASE)
            .header("referer", format!("{BASE}{INBOX_PATH}"))
            .header("sec-fetch-dest", "empty")
            .header("sec-fetch-mode", "cors")
            .header("sec-fetch-site", "same-origin")
            .header("x-asbd-id", "359341")
            .header("x-csrftoken", self.jar.get("csrftoken").unwrap_or_default())
            .header("x-fb-friendly-name", doc.name)
            .header("x-fb-lsd", &config.lsd)
            .header("x-ig-app-id", &config.app_id)
            .header("x-ig-www-claim", &self.www_claim)
            .form(&graphql::form(config, doc, variables))
            .send()
            .await
            .map_err(|e| failed(&e))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(WebError::LoggedOut(format!(
                "{}: redirected ({status})",
                doc.name
            )));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(WebError::RateLimited(format!("{}: HTTP 429", doc.name)));
        }
        let headers = response.headers().clone();
        let body = response.text().await.map_err(|e| failed(&e))?;
        if !status.is_success() {
            return Err(WebError::Other(format!("{}: HTTP {status}", doc.name)));
        }
        match graphql::data_field(&body, field) {
            Err(GraphqlError::LoggedOut(reason)) => {
                Err(WebError::LoggedOut(format!("{}: {reason}", doc.name)))
            }
            Err(e) => Err(WebError::Other(format!("{}: {e}", doc.name))),
            Ok(data) => {
                self.take_cookies(&headers);
                Ok(data)
            }
        }
    }
}
