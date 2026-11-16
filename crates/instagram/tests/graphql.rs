//! The read queries' requests and how their answers are taken apart.

use instagram_client::graphql::{
    self, GraphqlError, INBOX, MESSAGES, THREAD, THREAD_LIST, data_field, form,
};
use instagram_client::page::PageConfig;
use serde_json::json;

fn config() -> PageConfig {
    PageConfig {
        viewer_id: "293400001".into(),
        viewer_username: None,
        viewer_fbid: Some("170000000000001".into()),
        non_facebook_user_id: Some("17841400000000001".into()),
        app_id: "936619743392459".into(),
        device_id: "bbbbbbbb-0000-4000-8000-000000000002".into(),
        lsd: "lsd".into(),
        dtsg: "AB".into(),
        spin: Some(("1".into(), "trunk".into(), "2".into())),
        comet_req: "7".into(),
    }
}

#[test]
fn only_queries_are_known() {
    for doc in [INBOX, THREAD_LIST, THREAD, MESSAGES] {
        assert!(doc.name.ends_with("Query"), "{} is not a query", doc.name);
        assert!(doc.doc_id.chars().all(|c| c.is_ascii_digit()));
    }
}

#[test]
fn the_form_names_the_document_and_checksums_the_token() {
    let variables = graphql::thread_variables("1100000000000001");
    let fields = form(&config(), &THREAD, &variables);
    let get = |name: &str| {
        fields
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
            .unwrap()
    };
    assert_eq!(get("doc_id"), THREAD.doc_id);
    assert_eq!(get("fb_api_req_friendly_name"), "IGDThreadDetailQuery");
    // "2" and the byte sum of "AB" (65 + 66).
    assert_eq!(get("jazoest"), "2131");
    assert_eq!(get("fb_dtsg"), "AB");
    assert_eq!(get("__spin_b"), "trunk");
    assert_eq!(get("__user"), "0");
    let sent: serde_json::Value = serde_json::from_str(get("variables")).unwrap();
    assert_eq!(sent["thread_fbid"], "1100000000000001");
    assert_eq!(
        sent["__relay_internal__pv__IGDInitialMessagePageCountrelayprovider"],
        20
    );
}

#[test]
fn paging_names_the_cursor_and_the_viewer() {
    let list = graphql::thread_list_variables("170000000000001", "c1");
    assert_eq!(list["id"], "170000000000001");
    assert_eq!(list["cursor"], "c1");
    assert_eq!(list["folder"], "INBOX");
    let older = graphql::messages_variables("1100000000000001", "c2");
    assert_eq!(older["id"], "1100000000000001");
    assert_eq!(older["after"], "c2");
    assert_eq!(older["first"], 20);
    assert_eq!(
        graphql::inbox_variables(&config())["device_id_for_iris_subscription"],
        "bbbbbbbb-0000-4000-8000-000000000002"
    );
}

#[test]
fn an_answer_is_its_data_field_with_or_without_the_hijacking_prefix() {
    let body = json!({"data": {"get_slide_thread_nullable": {"id": "1"}}}).to_string();
    assert_eq!(
        data_field(&body, graphql::THREAD_FIELD).unwrap(),
        Some(json!({"id": "1"}))
    );
    assert_eq!(
        data_field(&format!("for (;;);{body}"), graphql::THREAD_FIELD).unwrap(),
        Some(json!({"id": "1"}))
    );
    let gone = json!({"data": {"get_slide_thread_nullable": null}}).to_string();
    assert_eq!(data_field(&gone, graphql::THREAD_FIELD).unwrap(), None);
}

#[test]
fn errors_beside_usable_data_are_tolerated_and_without_it_are_not() {
    let partial = json!({
        "data": {"get_slide_thread_nullable": {"id": "1"}},
        "errors": [{"message": "a field failed"}],
    })
    .to_string();
    assert!(
        data_field(&partial, graphql::THREAD_FIELD)
            .unwrap()
            .is_some()
    );
    let failed = json!({"errors": [{"message": "Please reload"}]}).to_string();
    assert!(matches!(
        data_field(&failed, graphql::THREAD_FIELD),
        Err(GraphqlError::Errors(_))
    ));
    assert_eq!(
        data_field("{}", graphql::THREAD_FIELD),
        Err(GraphqlError::NoData("get_slide_thread_nullable".into()))
    );
    assert_eq!(
        data_field("<html>", graphql::THREAD_FIELD),
        Err(GraphqlError::NotJson)
    );
}

#[test]
fn a_logged_out_answer_is_its_own_error() {
    let body =
        json!({"message": "Please wait", "require_login": true, "status": "fail"}).to_string();
    assert!(matches!(
        data_field(&body, graphql::INBOX_FIELD),
        Err(GraphqlError::LoggedOut(_))
    ));
}
