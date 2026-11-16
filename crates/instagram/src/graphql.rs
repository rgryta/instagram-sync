//! The four persisted queries the direct inbox reads with, by the friendly names and
//! document ids mautrix-meta's `instameow` lists (`messagix/graphql/docs.go`, 2026-11-14).
//!
//! These are the whole surface: there is no way to name a mutation here, so this client
//! cannot mark a thread read, react or send. The Chromium harvester had to abort the
//! `…Mutation` requests Instagram's own page fires on opening a thread; with no page
//! running, none are fired.

use serde_json::{Value, json};

use crate::page::PageConfig;

/// A persisted read query: what `fb_api_req_friendly_name` and `doc_id` carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Doc {
    pub name: &'static str,
    pub doc_id: &'static str,
}

/// The inbox's first page: 15 threads, each with a five-message preview.
pub const INBOX: Doc = Doc {
    name: "PolarisDirectInboxQuery",
    doc_id: "27262915580045003",
};
/// The inbox's further pages, five threads each.
pub const THREAD_LIST: Doc = Doc {
    name: "IGDThreadListOffMsysPaginationQuery",
    doc_id: "27712934271665380",
};
/// One thread with its newest 20 full messages.
pub const THREAD: Doc = Doc {
    name: "IGDThreadDetailQuery",
    doc_id: "37117700834487428",
};
/// A thread's older messages, 20 per page, newest first.
pub const MESSAGES: Doc = Doc {
    name: "IGDMessageListOffMsysQuery",
    doc_id: "27502152406082940",
};

/// The field of `data` each query answers under.
pub const INBOX_FIELD: &str = "get_slide_mailbox_for_iris_subscription";
pub const THREAD_LIST_FIELD: &str = "fetch__SlideMailbox";
pub const THREAD_FIELD: &str = "get_slide_thread_nullable";
pub const MESSAGES_FIELD: &str = "fetch__SlideThread";

/// Messages per page: what the web client and mautrix ask for, and all the thread query
/// returns whatever is asked.
pub const PAGE: u32 = 20;

pub fn inbox_variables(config: &PageConfig) -> Value {
    json!({
        "device_id_for_iris_subscription": config.device_id,
        "__relay_internal__pv__IGDIsProfessionalAccountGKrelayprovider": false,
        "__relay_internal__pv__IGDPinnedThreadsRenderEnabledGKrelayprovider": true,
        "__relay_internal__pv__IGDMaxUnreadMessagesCountrelayprovider": 5,
        "__relay_internal__pv__PolarisAIGMAccountLabelEnabledrelayprovider": false,
        "__relay_internal__pv__IGDThreadListActionsEnabledGKrelayprovider": true,
    })
}

/// The inbox page after `cursor`, keyed on the viewer's `IG_USER_EIMU`.
pub fn thread_list_variables(viewer_fbid: &str, cursor: &str) -> Value {
    json!({
        "count": 5,
        "cursor": cursor,
        "folder": "INBOX",
        "newer_than_timestamp_ms": null,
        "id": viewer_fbid,
        "__relay_internal__pv__IGDPinnedThreadsRenderEnabledGKrelayprovider": true,
        "__relay_internal__pv__IGDMaxUnreadMessagesCountrelayprovider": 5,
        "__relay_internal__pv__PolarisAIGMAccountLabelEnabledrelayprovider": false,
        "__relay_internal__pv__IGDThreadListActionsEnabledGKrelayprovider": true,
    })
}

/// One thread by its `thread_fbid` (16 digits), which is not the 39-digit `thread_id` the
/// archive stores.
pub fn thread_variables(thread_fbid: &str) -> Value {
    json!({
        "min_uq_seq_id": null,
        "thread_fbid": thread_fbid,
        "__relay_internal__pv__IGDEnableOffMsysChatThemesQErelayprovider": false,
        "__relay_internal__pv__IGDInitialMessagePageCountrelayprovider": PAGE,
        "__relay_internal__pv__PolarisAIGMAccountLabelEnabledrelayprovider": false,
    })
}

/// The page of `thread_fbid`'s messages after `cursor` (a previous page's `end_cursor`).
pub fn messages_variables(thread_fbid: &str, cursor: &str) -> Value {
    json!({
        "after": cursor,
        "before": null,
        "first": PAGE,
        "last": null,
        "newer_than_message_id": null,
        "older_than_message_id": null,
        "id": thread_fbid,
        "__relay_internal__pv__IGDInitialMessagePageCountrelayprovider": PAGE,
    })
}

/// The form body, as mautrix's `MakeGraphQLRequest` sends it for Instagram. `jazoest` is
/// the page's checksum of `fb_dtsg`: "2" and the sum of its bytes.
pub fn form(config: &PageConfig, doc: &Doc, variables: &Value) -> Vec<(&'static str, String)> {
    let jazoest = format!("2{}", config.dtsg.bytes().map(u32::from).sum::<u32>());
    let (spin_r, spin_b, spin_t) = config.spin.clone().unwrap_or_default();
    vec![
        ("__user", "0".into()),
        ("__a", "1".into()),
        ("__req", "1".into()),
        ("__comet_req", config.comet_req.clone()),
        ("fb_dtsg", config.dtsg.clone()),
        ("jazoest", jazoest),
        ("lsd", config.lsd.clone()),
        ("__spin_r", spin_r),
        ("__spin_b", spin_b),
        ("__spin_t", spin_t),
        ("__d", "www".into()),
        ("__jssesw", "1".into()),
        ("fb_api_caller_class", "RelayModern".into()),
        ("fb_api_req_friendly_name", doc.name.into()),
        ("variables", variables.to_string()),
        ("server_timestamps", "true".into()),
        ("doc_id", doc.doc_id.into()),
    ]
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GraphqlError {
    #[error("the answer is not JSON")]
    NotJson,
    /// Instagram's logged-out answer (`require_login`), which no retry fixes.
    #[error("the session is logged out: {0}")]
    LoggedOut(String),
    #[error("the answer carries errors: {0}")]
    Errors(String),
    #[error("the answer carries no data.{0}")]
    NoData(String),
}

/// `data.<field>` of an answer. Errors beside a usable `data` are tolerated, as mautrix
/// does for the thread queries; `null` there (a thread gone) is `None`.
pub fn data_field(body: &str, field: &str) -> Result<Option<Value>, GraphqlError> {
    let body = body.trim_start();
    let body = body.strip_prefix("for (;;);").unwrap_or(body);
    let answer: Value = serde_json::from_str(body).map_err(|_| GraphqlError::NotJson)?;
    let summary = |v: &Value| v.to_string().chars().take(300).collect::<String>();
    if answer.get("require_login").and_then(Value::as_bool) == Some(true) {
        return Err(GraphqlError::LoggedOut(summary(&answer)));
    }
    match answer.get("data").and_then(|d| d.get(field)) {
        Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(value.clone())),
        None => match answer.get("errors").filter(|e| !e.is_null()) {
            Some(errors) => Err(GraphqlError::Errors(summary(errors))),
            None => Err(GraphqlError::NoData(field.to_owned())),
        },
    }
}
