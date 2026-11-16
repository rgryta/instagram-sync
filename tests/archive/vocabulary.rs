//! The canary for "Instagram changed the message vocabulary".
//!
//! `item_type` is GraphQL's `content_type`, and the classification in
//! `instagram_core::store` keys off it: a value nobody has measured is either a new kind
//! of message counted as an ordinary one, or a new system line counted as something a
//! person sent. Both of those shipped once already, because the constants still held
//! `direct_v2`'s names.
//!
//! `tests/test_normalize.py::test_the_live_capture_holds_no_content_type_we_have_not_measured`
//! asserts this through the normaliser, which is the ingester's and is phase 2's. The
//! fixture is the same, so this reads the capture directly: when it fails, measure the
//! new value and decide which group it belongs to.

use std::collections::BTreeSet;

use instagram_core::store::MEASURED_ITEM_TYPES;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/instagram/inbox.graphql.json"
);

/// Every `content_type` value anywhere in the payload, however deeply nested.
fn content_types(value: &serde_json::Value, found: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "content_type"
                    && let Some(name) = child.as_str()
                {
                    found.insert(name.to_owned());
                }
                content_types(child, found);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                content_types(item, found);
            }
        }
        _ => {}
    }
}

#[test]
fn the_live_capture_holds_no_content_type_we_have_not_measured() {
    let raw = std::fs::read_to_string(FIXTURE).expect("the live capture is in the repo");
    let payload: serde_json::Value = serde_json::from_str(&raw).expect("the capture is JSON");
    let mut seen = BTreeSet::new();
    content_types(&payload, &mut seen);

    assert!(
        !seen.is_empty(),
        "the fixture carries no content_type at all"
    );
    let unmeasured: Vec<&String> = seen
        .iter()
        .filter(|name| !MEASURED_ITEM_TYPES.contains(&name.as_str()))
        .collect();
    assert!(
        unmeasured.is_empty(),
        "unmeasured content_type(s): {unmeasured:?}"
    );
}

#[test]
fn every_measured_type_is_upper_case() {
    // The whole bug class in one line: the constants held `direct_v2`'s lower-case REST
    // names (`action_log`, `video_call_event`) for months, matched nothing, and so every
    // reaction line counted as a message somebody sent.
    for name in MEASURED_ITEM_TYPES {
        assert_eq!(
            name,
            name.to_uppercase(),
            "{name} is not GraphQL's spelling"
        );
    }
}
