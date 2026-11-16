//! The live socket's requests and how its pushes are taken apart, on synthetic data.

use base64::Engine;
use instagram_client::dgw::Frame;
use instagram_client::live::{
    self, Event, HandleSyncFailure, LsResponse, MainStream, Noop, OnIrisSubscribe, OnMailboxDelta,
    OnResnapshot, Operation, operation::Kind,
};
use instagram_client::page::PageConfig;
use prost::Message;
use serde_json::{Value, json};

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
        spin: None,
        comet_req: "7".into(),
    }
}

#[test]
fn the_url_carries_the_query_mautrix_sends_in_go_s_sorted_order() {
    assert_eq!(
        live::url(&config()).unwrap(),
        "wss://gateway.instagram.com/ws/lightspeed?x-dgw-appid=936619743392459\
         &x-dgw-appversion=0&x-dgw-authtype=6%3A0\
         &x-dgw-deviceid=bbbbbbbb-0000-4000-8000-000000000002&x-dgw-tier=prod\
         &x-dgw-uuid=17841400000000001&x-dgw-version=5"
    );
    let mut config = config();
    config.non_facebook_user_id = None;
    assert!(live::url(&config).is_err());
}

#[test]
fn the_init_payload_is_byte_for_byte_what_instameow_marshals() {
    let payload = live::init_payload(
        "936619743392459",
        "bbbbbbbb-0000-4000-8000-000000000002",
        123456,
        1763251200000,
        0,
    );
    assert_eq!(
        payload,
        r#"{"app_id":"936619743392459","device_id":"bbbbbbbb-0000-4000-8000-000000000002","payload":"{\"database\":223,\"last_applied_cursor\":\"{\\\"seq_id\\\":123456}\",\"sync_params\":\"{\\\"user_agent\\\":\\\"WMI Web\\\",\\\"snapshot_at_ms\\\":1763251200000,\\\"prevalidated_graphql_doc_id\\\":\\\"27931342966497053\\\"}\",\"epoch_id\":0,\"version\":-3,\"failure_count\":0}","request_id":4,"type":2}"#
    );
}

#[test]
fn a_reconnect_counts_its_retries_into_the_request() {
    let outer: Value = serde_json::from_str(&live::init_payload("1", "d", 7, 8, 2)).unwrap();
    assert_eq!(outer["request_id"], 6);
    let query: Value = serde_json::from_str(outer["payload"].as_str().unwrap()).unwrap();
    assert_eq!(query["failure_count"], 2);
}

#[test]
fn the_mailbox_names_its_seq_id_as_a_string() {
    let inbox = json!({"iris_inactive_subscription_uq_seq_id": "9001", "threads_by_folder": {}});
    assert_eq!(live::mailbox_seq_id(&inbox), Some(9001));
    assert_eq!(live::mailbox_seq_id(&json!({})), None);
}

fn delta_json() -> String {
    json!([
        {"data": {"slide_delta_processor": [
            {"__typename": "SlideUQPPNewMessage", "uq_seq_id": "41", "message": {"text": "x"}},
            {"__typename": "SlideUQPPReadReceipt", "uq_seq_id": "43"},
        ]}},
        {"data": {"slide_delta_processor": [
            {"__typename": "SlideUQPPCreateReaction", "uq_seq_id": 42},
        ]}},
        {},
    ])
    .to_string()
}

#[test]
fn deltas_are_read_with_their_type_seq_id_and_whole_json() {
    let events = live::deltas(&delta_json()).unwrap();
    let seen: Vec<(&str, i64)> = events
        .iter()
        .map(|e| match e {
            Event::Delta {
                typename,
                uq_seq_id,
                ..
            } => (typename.as_str(), *uq_seq_id),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("SlideUQPPNewMessage", 41),
            ("SlideUQPPReadReceipt", 43),
            ("SlideUQPPCreateReaction", 42),
        ]
    );
    let Event::Delta { raw, .. } = &events[0] else {
        unreachable!()
    };
    assert_eq!(raw["message"]["text"], "x");
    assert!(live::deltas(r#"[{"data":{"slide_delta_processor":[{"uq_seq_id":"x"}]}}]"#).is_err());
    assert!(live::deltas("").is_err());
}

fn op(kind: Kind) -> Operation {
    Operation { kind: Some(kind) }
}

/// A data frame's body as the gateway sends it: JSON around a base64 `LSResponse`.
fn envelope(operations: Vec<Operation>) -> Vec<u8> {
    let bytes = LsResponse { operations }.encode_to_vec();
    let payload = base64::engine::general_purpose::STANDARD.encode(bytes);
    json!({"request_id": null, "payload": payload})
        .to_string()
        .into_bytes()
}

#[test]
fn every_operation_kind_decodes() {
    let body = envelope(vec![
        op(Kind::Subscribe(OnIrisSubscribe {
            group_id: Some(1),
            subscribed_seq_id: Some(40),
            latest_seq_id: Some(45),
        })),
        op(Kind::Delta(OnMailboxDelta {
            group_id: Some(1),
            delta: Some(delta_json()),
        })),
        op(Kind::Noop(Noop {})),
        op(Kind::Resnapshot(OnResnapshot {
            group_id: Some(1),
            error_message: Some("too old".into()),
            error_type: Some(3),
        })),
        op(Kind::Failure(HandleSyncFailure {
            group_id: 1,
            error_message: "nope".into(),
        })),
    ]);
    let events = live::decode_payload(&body).unwrap();
    assert_eq!(
        events[0],
        Event::Connected {
            subscribed_seq_id: Some(40),
            latest_seq_id: Some(45),
        }
    );
    assert!(matches!(&events[1], Event::Delta { uq_seq_id: 41, .. }));
    assert_eq!(events.len(), 7);
    assert_eq!(events[4], Event::Noop);
    assert_eq!(events[5], Event::Resnapshot("(3) too old".into()));
    assert_eq!(events[6], Event::Failure("group 1: nope".into()));

    assert!(live::decode_payload(&envelope(vec![Operation { kind: None }])).is_err());
    assert!(live::decode_payload(br#"{"payload":"!!"}"#).is_err());
}

fn message(frames: &[Frame]) -> Vec<u8> {
    let mut out = Vec::new();
    for frame in frames {
        frame.encode(&mut out);
    }
    out
}

fn data(stream: u16, ack_id: u16, payload: Vec<u8>) -> Frame {
    Frame::Data {
        stream,
        ack_id,
        requires_ack: true,
        content_type: None,
        payload,
    }
}

#[test]
fn the_stream_answers_pings_acks_what_asks_and_follows_the_seq_id() {
    let mut main = MainStream::new(40);
    let body = envelope(vec![op(Kind::Delta(OnMailboxDelta {
        group_id: None,
        delta: Some(delta_json()),
    }))]);
    let reply = main
        .handle(&message(&[
            Frame::Ping,
            Frame::Establish {
                stream: 0,
                parameters: br#"{"code":200}"#.to_vec(),
            },
            Frame::Ack { stream: 0, ack: 0 },
            data(0, 5, body),
            data(3, 9, b"not ours".to_vec()),
            Frame::Pong,
        ]))
        .unwrap();
    assert_eq!(
        reply,
        [
            Frame::Pong,
            Frame::Ack { stream: 0, ack: 5 },
            Frame::Ack { stream: 3, ack: 9 },
        ]
    );
    let mut deltas = 0;
    while let Some(event) = main.next_event() {
        assert!(matches!(event, Event::Delta { .. }));
        deltas += 1;
    }
    assert_eq!(deltas, 3);
    assert_eq!(main.seq_id(), 43);
}

#[test]
fn a_frame_that_does_not_parse_or_the_stream_ending_is_an_error() {
    let mut main = MainStream::new(1);
    assert!(
        main.handle(&message(&[data(0, 1, b"{}".to_vec())]))
            .is_err()
    );
    assert!(
        main.handle(&message(&[Frame::EndOfData {
            stream: 0,
            reason: None
        }]))
        .is_err()
    );
    // A seq id never moves back.
    let body = envelope(vec![op(Kind::Delta(OnMailboxDelta {
        group_id: None,
        delta: Some(delta_json()),
    }))]);
    let mut ahead = MainStream::new(100);
    ahead.handle(&message(&[data(0, 1, body)])).unwrap();
    assert_eq!(ahead.seq_id(), 100);
}
