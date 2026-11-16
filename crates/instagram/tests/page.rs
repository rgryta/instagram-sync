//! The inbox page's `define` blocks, in the shape the live page carried on 2026-11-16 with
//! synthetic values: an Instagram-only account, so every Facebook id is "0".

use instagram_client::page::{PageConfig, PageError, define, scrape};

fn page(viewer: &str, extra: &str) -> String {
    format!(
        r#"<html><script>requireLazy(["ScheduledServerJS"],function(s){{s.handle({{"define":[
["CurrentUserInitialData",[],{{"ACCOUNT_ID":"0","APP_ID":"936619743392459","IG_USER_EIMU":"170000000000001","NON_FACEBOOK_USER_ID":"17841400000000001","USER_ID":"0","NAME":"x"}},270],
["DGWWebConfig",[],{{"appId":"936619743392459","appVersion":"0","authType":"","dgwVersion":"2","endpoint":"","fbId":"0"}},1],
["MqttWebDeviceID",[],{{"clientID":"aaaaaaaa-0000-4000-8000-000000000001"}},2],
["IGDMqttWebDeviceID",[],{{"clientId":"bbbbbbbb-0000-4000-8000-000000000002"}},3],
["LSD",[],{{"token":"lsd-token-xxxxxxxxxxxx"}},4],
["DTSGInitialData",[],{{"token":"dtsg-token"}},5],
["DTSGInitData",[],{{"token":"dtsg-token","async_get_token":"x"}},6],
["SiteData",[],{{"__spin_r":1030000000,"__spin_b":"trunk","__spin_t":1763251200,"client_revision":1030000000}},7],
["PolarisViewer",[],{{"data":{{"fbid":"17841400000000001","username":"synthetic.handle"}},"id":{viewer}}},8]{extra}
]}})}});</script><script>{{"__comet_req":7}}</script></html>"#
    )
}

#[test]
fn an_instagram_only_session_is_read_from_the_viewer_not_the_facebook_ids() {
    let config = scrape(&page(r#""293400001""#, "")).unwrap();
    assert_eq!(
        config,
        PageConfig {
            viewer_id: "293400001".into(),
            viewer_username: Some("synthetic.handle".into()),
            viewer_fbid: Some("170000000000001".into()),
            non_facebook_user_id: Some("17841400000000001".into()),
            app_id: "936619743392459".into(),
            device_id: "bbbbbbbb-0000-4000-8000-000000000002".into(),
            lsd: "lsd-token-xxxxxxxxxxxx".into(),
            dtsg: "dtsg-token".into(),
            spin: Some(("1030000000".into(), "trunk".into(), "1763251200".into())),
            comet_req: "7".into(),
        }
    );
}

#[test]
fn a_page_with_no_viewer_is_logged_out() {
    assert_eq!(scrape(&page("null", "")), Err(PageError::LoggedOut));
    assert_eq!(scrape(&page(r#""0""#, "")), Err(PageError::LoggedOut));
    assert_eq!(scrape("<html>login</html>"), Err(PageError::LoggedOut));
}

#[test]
fn a_missing_token_is_named() {
    let html = page(r#""293400001""#, "").replace("DTSGInitData", "Renamed");
    assert_eq!(scrape(&html), Err(PageError::Missing("DTSGInitData.token")));
}

#[test]
fn a_define_is_read_whole() {
    let html = page(r#""293400001""#, "");
    assert_eq!(
        define(&html, "LSD").unwrap()["token"],
        "lsd-token-xxxxxxxxxxxx"
    );
    assert!(define(&html, "Absent").is_none());
}
