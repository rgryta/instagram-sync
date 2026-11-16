//! What `instagram.com/direct/inbox/` tells a client before it can query GraphQL.
//!
//! The page carries server-side `define` blocks — `["Name",[],{…},id]` inside its
//! scheduled JS. An Instagram-only account has no Facebook id: every `USER_ID` and
//! `fbId` on the page is `"0"`, and the session is the viewer's `PolarisViewer.id`.
//! `tests/page.rs` holds the shapes.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageConfig {
    /// `PolarisViewer.id`: the account's Instagram pk, the same number as `ds_user_id`.
    pub viewer_id: String,
    /// `PolarisViewer.data.username`: the account's handle, which no inbox reply carries.
    pub viewer_username: Option<String>,
    /// `CurrentUserInitialData.IG_USER_EIMU`: the id messages name the viewer by
    /// (`sender_fbid`), and the one thread-list paging is keyed on.
    pub viewer_fbid: Option<String>,
    /// `PolarisViewer.data.fbid`: on an Instagram-only account the same number as
    /// `CurrentUserInitialData.NON_FACEBOOK_USER_ID` (mautrix's `GetOwnFBID` compares the
    /// two). The gateway socket is opened as this id (`x-dgw-uuid`, see `live.rs`).
    pub non_facebook_user_id: Option<String>,
    /// `CurrentUserInitialData.APP_ID`: GraphQL's `x-ig-app-id`.
    pub app_id: String,
    /// `IGDMqttWebDeviceID.clientId`: this browser's device id, which the inbox query
    /// names its subscription by.
    pub device_id: String,
    /// `LSD.token`, sent as `lsd` and `x-fb-lsd`.
    pub lsd: String,
    /// `DTSGInitData.token`, sent as `fb_dtsg`.
    pub dtsg: String,
    /// `SiteData.__spin_r`, `__spin_b`, `__spin_t`: the site revision GraphQL calls name.
    pub spin: Option<(String, String, String)>,
    /// `__comet_req`, the page's Comet request kind.
    pub comet_req: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PageError {
    /// The cookies are no longer a session: the page carries no viewer.
    #[error("the page is not logged in")]
    LoggedOut,
    #[error("the page carries no {0}")]
    Missing(&'static str),
}

/// The object a `define` block holds: the JSON right after `"Name",[],`.
pub fn define(html: &str, name: &str) -> Option<Value> {
    let marker = format!("[\"{name}\",[],");
    let start = html.find(&marker)? + marker.len();
    serde_json::Deserializer::from_str(&html[start..])
        .into_iter::<Value>()
        .next()?
        .ok()
}

fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() && s != "0" => Some(s.clone()),
        Value::Number(n) if n.as_u64() != Some(0) => Some(n.to_string()),
        _ => None,
    }
}

/// `"__comet_req":7`, read where the page writes it.
fn comet_req(html: &str) -> Option<String> {
    let at = html.find("\"__comet_req\":")? + "\"__comet_req\":".len();
    let digits: String = html[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty()).then_some(digits)
}

pub fn scrape(html: &str) -> Result<PageConfig, PageError> {
    let viewer = define(html, "PolarisViewer");
    let viewer_id = text(viewer.as_ref().and_then(|v| v.get("id"))).ok_or(PageError::LoggedOut)?;
    let viewer_username = text(viewer.as_ref().and_then(|v| v.pointer("/data/username")));
    let user = define(html, "CurrentUserInitialData");
    let field = |define_name: &str, key: &str| {
        text(define(html, define_name).as_ref().and_then(|v| v.get(key)))
    };
    let site = define(html, "SiteData");
    let site_field = |key: &str| text(site.as_ref().and_then(|s| s.get(key)));
    Ok(PageConfig {
        viewer_id,
        viewer_username,
        viewer_fbid: text(user.as_ref().and_then(|u| u.get("IG_USER_EIMU"))),
        non_facebook_user_id: text(viewer.as_ref().and_then(|v| v.pointer("/data/fbid"))),
        app_id: text(user.as_ref().and_then(|u| u.get("APP_ID")))
            .ok_or(PageError::Missing("CurrentUserInitialData.APP_ID"))?,
        device_id: field("IGDMqttWebDeviceID", "clientId")
            .ok_or(PageError::Missing("IGDMqttWebDeviceID.clientId"))?,
        lsd: field("LSD", "token").ok_or(PageError::Missing("LSD.token"))?,
        dtsg: field("DTSGInitData", "token").ok_or(PageError::Missing("DTSGInitData.token"))?,
        spin: site_field("__spin_r")
            .zip(site_field("__spin_b"))
            .zip(site_field("__spin_t"))
            .map(|((r, b), t)| (r, b, t)),
        comet_req: comet_req(html).unwrap_or_else(|| "7".into()),
    })
}
