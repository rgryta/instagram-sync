//! Live inbox updates: the websocket Instagram's web client keeps open on Meta's gateway,
//! `wss://gateway.instagram.com/ws/lightspeed`, as mautrix-meta's `instameow` opens it
//! (`socket.go`, over `messagix/dgw/socket.go`).
//!
//! One persistent DGW stream (id 0) carries one request: a sync of database 223 from the
//! mailbox's `iris_inactive_subscription_uq_seq_id`, which the INBOX query answers with.
//! The server then pushes data frames whose JSON body holds a base64 `LSResponse`
//! (protobuf, `mdCoreSync/MDCoreSync.proto`) of operations: `subscribe` once the stream is
//! live, `delta` with the slide deltas (new messages, edits, unsends, reactions, read
//! receipts…) each stamped with a `uq_seq_id`, `noop`, and `resnapshot` or `failure` when
//! the cursor cannot be served. After either of those mautrix reconnects (from the last
//! `uq_seq_id`, or from a fresh INBOX query after a resnapshot); so must a caller.
//!
//! Read-only: besides the stream's establish frame and its one init request, the client
//! sends only what keeps the socket alive, exactly as mautrix does — a DGW ping every 10 s,
//! a pong for each ping, and an ack for every data frame that asks for one. It sends no
//! typing, presence or read receipts, opens no `mqttbypass` or `streamcontroller` socket,
//! and makes no GraphQL call, mutation or otherwise.

use std::collections::VecDeque;
use std::time::Duration;

use base64::Engine;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::time::{Instant, Interval};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::cookies::Jar;
use crate::dgw::Frame;
use crate::page::PageConfig;
use crate::web::{BASE, USER_AGENT};

pub const URL: &str = "wss://gateway.instagram.com/ws/lightspeed";
const HOST: &str = "gateway.instagram.com";
const PATH: &str = "/ws/lightspeed";
/// The inbox database the web client syncs.
pub const DATABASE: i64 = 223;
/// `useragent.IGDUserAgent`: what the web client names itself in `sync_params`.
pub const SYNC_USER_AGENT: &str = "WMI Web";
/// `graphql.IGDSlideDeltaProcessorQuery`: the document the server renders deltas with.
pub const DELTA_DOC_ID: &str = "27931342966497053";
const STREAM: u16 = 0;
const PING_INTERVAL: Duration = Duration::from_secs(10);
const PONG_TIMEOUT: Duration = Duration::from_secs(30);
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// The socket URL. Go's `url.Values.Encode` sorts the keys, so these are in that order.
pub fn url(config: &PageConfig) -> Result<String, LiveError> {
    let uuid = config
        .non_facebook_user_id
        .as_deref()
        .ok_or(LiveError::NoUserId)?;
    let url = reqwest::Url::parse_with_params(
        URL,
        [
            ("x-dgw-appid", config.app_id.as_str()),
            ("x-dgw-appversion", "0"),
            ("x-dgw-authtype", "6:0"),
            ("x-dgw-deviceid", config.device_id.as_str()),
            ("x-dgw-tier", "prod"),
            ("x-dgw-uuid", uuid),
            ("x-dgw-version", "5"),
        ],
    )
    .expect("the gateway URL parses");
    Ok(url.into())
}

#[derive(Serialize)]
struct SyncParams<'a> {
    user_agent: &'a str,
    snapshot_at_ms: i64,
    prevalidated_graphql_doc_id: &'a str,
}

#[derive(Serialize)]
struct Cursor {
    seq_id: i64,
}

/// `socket.DatabaseQuery`, field for field in Go's order.
#[derive(Serialize)]
struct DatabaseQuery {
    database: i64,
    last_applied_cursor: String,
    sync_params: String,
    epoch_id: i64,
    version: i64,
    failure_count: u32,
}

#[derive(Serialize)]
struct Connect<'a> {
    app_id: &'a str,
    device_id: &'a str,
    payload: String,
    request_id: u32,
    r#type: u8,
}

/// The stream's init request (`makeStreamInitPayload`): sync database 223 from `seq_id`.
/// `snapshot_at_ms` is when that seq id was read, and `retries` how many connections came
/// before this one (mautrix's `socketRetries`).
pub fn init_payload(
    app_id: &str,
    device_id: &str,
    seq_id: i64,
    snapshot_at_ms: i64,
    retries: u32,
) -> String {
    let query = DatabaseQuery {
        database: DATABASE,
        last_applied_cursor: json(&Cursor { seq_id }),
        sync_params: json(&SyncParams {
            user_agent: SYNC_USER_AGENT,
            snapshot_at_ms,
            prevalidated_graphql_doc_id: DELTA_DOC_ID,
        }),
        epoch_id: 0,
        version: -3,
        failure_count: retries,
    };
    json(&Connect {
        app_id,
        device_id,
        payload: json(&query),
        request_id: 4 + retries,
        r#type: 2,
    })
}

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("plain structs serialize")
}

/// `iris_inactive_subscription_uq_seq_id` of the INBOX answer: where the stream starts.
pub fn mailbox_seq_id(inbox: &Value) -> Option<i64> {
    seq_id(inbox.get("iris_inactive_subscription_uq_seq_id")?).ok()
}

/// A `,string` int64 as Go reads it; a bare number is taken too.
fn seq_id(value: &Value) -> Result<i64, String> {
    match value {
        Value::Null => Ok(0),
        Value::String(s) => s.parse().map_err(|_| format!("uq_seq_id {s:?}")),
        Value::Number(n) => n.as_i64().ok_or_else(|| format!("uq_seq_id {n}")),
        _ => Err("uq_seq_id is not a number".into()),
    }
}

/// `mdCoreSync.LSResponse`, the fields read here. `controlHeader` (1) and
/// `transactionParams` (3) are skipped as unknown fields.
#[derive(Clone, PartialEq, prost::Message)]
pub struct LsResponse {
    #[prost(message, repeated, tag = "2")]
    pub operations: Vec<Operation>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Operation {
    #[prost(oneof = "operation::Kind", tags = "1, 2, 3, 4, 5")]
    pub kind: Option<operation::Kind>,
}

pub mod operation {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "1")]
        Subscribe(super::OnIrisSubscribe),
        #[prost(message, tag = "2")]
        Resnapshot(super::OnResnapshot),
        #[prost(message, tag = "3")]
        Delta(super::OnMailboxDelta),
        #[prost(message, tag = "4")]
        Noop(super::Noop),
        #[prost(message, tag = "5")]
        Failure(super::HandleSyncFailure),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OnIrisSubscribe {
    #[prost(int32, optional, tag = "1")]
    pub group_id: Option<i32>,
    #[prost(int64, optional, tag = "2")]
    pub subscribed_seq_id: Option<i64>,
    #[prost(int64, optional, tag = "3")]
    pub latest_seq_id: Option<i64>,
}

/// `snapshotPayload` (4) is skipped.
#[derive(Clone, PartialEq, prost::Message)]
pub struct OnResnapshot {
    #[prost(int32, optional, tag = "1")]
    pub group_id: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub error_message: Option<String>,
    #[prost(int32, optional, tag = "3")]
    pub error_type: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OnMailboxDelta {
    #[prost(int32, optional, tag = "1")]
    pub group_id: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub delta: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Noop {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HandleSyncFailure {
    #[prost(int32, required, tag = "1")]
    pub group_id: i32,
    #[prost(string, required, tag = "2")]
    pub error_message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `subscribe`: the stream is live from `subscribed_seq_id`; the server is at
    /// `latest_seq_id`.
    Connected {
        subscribed_seq_id: Option<i64>,
        latest_seq_id: Option<i64>,
    },
    /// One slide delta, its JSON kept whole.
    Delta {
        typename: String,
        uq_seq_id: i64,
        raw: Value,
    },
    Noop,
    /// The cursor is too old to replay: start again from a fresh INBOX query.
    Resnapshot(String),
    /// The sync failed: mautrix reconnects from the last `uq_seq_id`.
    Failure(String),
}

#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("the page carries no PolarisViewer.data.fbid")]
    NoUserId,
    #[error("a header value is not text")]
    Header,
    #[error("live socket: {0}")]
    Socket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("the socket closed (code {code:?}): {reason}")]
    Closed { code: Option<u16>, reason: String },
    #[error("the stream did not open: {0}")]
    Establish(String),
    #[error("a live frame does not parse: {0}")]
    Decode(String),
}

/// `[{"data":{"slide_delta_processor":[Delta…]}}]`, the string a `delta` operation carries.
pub fn deltas(json: &str) -> Result<Vec<Event>, LiveError> {
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        data: Data,
    }
    #[derive(Deserialize, Default)]
    struct Data {
        #[serde(default)]
        slide_delta_processor: Vec<Value>,
    }
    let wrappers: Vec<Wrapper> =
        serde_json::from_str(json).map_err(|e| LiveError::Decode(format!("delta: {e}")))?;
    wrappers
        .into_iter()
        .flat_map(|w| w.data.slide_delta_processor)
        .map(|raw| {
            Ok(Event::Delta {
                typename: raw
                    .get("__typename")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                uq_seq_id: seq_id(raw.get("uq_seq_id").unwrap_or(&Value::Null))
                    .map_err(LiveError::Decode)?,
                raw,
            })
        })
        .collect()
}

/// One data frame's payload: `{"request_id":…,"payload":"<base64 LSResponse>"}`.
pub fn decode_payload(payload: &[u8]) -> Result<Vec<Event>, LiveError> {
    #[derive(Deserialize)]
    struct Envelope {
        payload: String,
    }
    let envelope: Envelope =
        serde_json::from_slice(payload).map_err(|e| LiveError::Decode(format!("envelope: {e}")))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(envelope.payload)
        .map_err(|e| LiveError::Decode(format!("base64: {e}")))?;
    let response = LsResponse::decode(bytes.as_slice())
        .map_err(|e| LiveError::Decode(format!("protobuf: {e}")))?;
    let mut events = Vec::new();
    for op in response.operations {
        use operation::Kind;
        match op.kind {
            Some(Kind::Subscribe(s)) => events.push(Event::Connected {
                subscribed_seq_id: s.subscribed_seq_id,
                latest_seq_id: s.latest_seq_id,
            }),
            Some(Kind::Resnapshot(r)) => events.push(Event::Resnapshot(format!(
                "({}) {}",
                r.error_type.unwrap_or_default(),
                r.error_message.unwrap_or_default()
            ))),
            Some(Kind::Delta(d)) => events.extend(deltas(d.delta.as_deref().unwrap_or_default())?),
            Some(Kind::Noop(_)) => events.push(Event::Noop),
            Some(Kind::Failure(f)) => events.push(Event::Failure(format!(
                "group {}: {}",
                f.group_id, f.error_message
            ))),
            None => return Err(LiveError::Decode("an unrecognised operation".into())),
        }
    }
    Ok(events)
}

/// mautrix's `receiveEstablish`: the answer is `200` or `{"code":200,…}`.
fn establish_code(parameters: &[u8]) -> Result<(), String> {
    let value: Value =
        serde_json::from_slice(parameters).map_err(|_| "no response status code".to_owned())?;
    let code = if value.is_object() {
        value.get("code").cloned().unwrap_or(Value::Null)
    } else {
        value
    };
    match code.as_f64() {
        Some(200.0) => Ok(()),
        Some(code) => Err(format!("code {code}")),
        None => Err("no response status code".into()),
    }
}

/// The protocol half of [`Live`], with no socket: what the main stream has said, and what
/// each incoming message needs sent back.
pub struct MainStream {
    established: Option<Result<(), String>>,
    init_acked: bool,
    /// The last ping or pong heard: mautrix's pong timer resets on either, and only those.
    heard: Instant,
    seq_id: i64,
    events: VecDeque<Event>,
}

impl MainStream {
    pub fn new(seq_id: i64) -> Self {
        MainStream {
            established: None,
            init_acked: false,
            heard: Instant::now(),
            seq_id,
            events: VecDeque::new(),
        }
    }

    /// The cursor: the highest `uq_seq_id` seen, starting from the one connected from.
    pub fn seq_id(&self) -> i64 {
        self.seq_id
    }

    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// One websocket message; returns the frames to send back (pongs and acks). A data
    /// frame that does not parse is an error and is not acknowledged, as in mautrix.
    pub fn handle(&mut self, message: &[u8]) -> Result<Vec<Frame>, LiveError> {
        let frames = match Frame::decode_all(message) {
            Ok(frames) => frames,
            Err(e) => {
                tracing::warn!("dropped a gateway message that does not frame: {e}");
                return Ok(Vec::new());
            }
        };
        let mut reply = Vec::new();
        for frame in frames {
            match frame {
                Frame::Ping => {
                    self.heard = Instant::now();
                    reply.push(Frame::Pong);
                }
                Frame::Pong => self.heard = Instant::now(),
                Frame::Establish { stream, parameters } if stream == STREAM => {
                    self.established = Some(establish_code(&parameters));
                }
                Frame::Ack { stream, ack } if stream == STREAM && ack == 0 => {
                    self.init_acked = true;
                }
                Frame::Data {
                    stream,
                    ack_id,
                    requires_ack,
                    payload,
                    ..
                } => {
                    if stream == STREAM {
                        for event in decode_payload(&payload)? {
                            if let Event::Delta { uq_seq_id, .. } = event {
                                self.seq_id = self.seq_id.max(uq_seq_id);
                            }
                            self.events.push_back(event);
                        }
                    }
                    // Unknown streams are acknowledged too.
                    if requires_ack {
                        reply.push(Frame::Ack {
                            stream,
                            ack: ack_id,
                        });
                    }
                }
                Frame::EndOfData { stream, .. } if stream == STREAM => {
                    return Err(LiveError::Closed {
                        code: None,
                        reason: "the main stream ended".into(),
                    });
                }
                // mautrix only logs a drain, and a deauth on a websocket (it ends only its
                // HTTP stream transport).
                other => tracing::debug!("ignored gateway frame {other:?}"),
            }
        }
        Ok(reply)
    }
}

pub struct Live {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ping: Interval,
    main: MainStream,
}

impl Live {
    /// Opens the socket and its stream from `seq_id` (see [`init_payload`]), and waits, as
    /// mautrix does, up to 5 s for the stream to open and 5 s more for the init request's
    /// ack. Cookies the upgrade sets go into `jar`.
    pub async fn connect(
        config: &PageConfig,
        jar: &mut Jar,
        seq_id: i64,
        snapshot_at_ms: i64,
        retries: u32,
    ) -> Result<Self, LiveError> {
        let mut request = url(config)?.into_client_request()?;
        let headers = request.headers_mut();
        let header = |v: &str| HeaderValue::from_str(v).map_err(|_| LiveError::Header);
        headers.insert("cookie", header(&jar.header(HOST, PATH, Utc::now()))?);
        headers.insert("user-agent", header(USER_AGENT)?);
        headers.insert("origin", header(BASE)?);
        headers.insert("sec-fetch-dest", header("empty")?);
        headers.insert("sec-fetch-mode", header("websocket")?);
        headers.insert("sec-fetch-site", header("same-site")?);
        let (ws, upgrade) = tokio_tungstenite::connect_async(request).await?;
        for value in upgrade.headers().get_all("set-cookie") {
            if let Ok(v) = value.to_str() {
                jar.set(v, HOST, Utc::now());
            }
        }
        let mut live = Live {
            ws,
            ping: tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL),
            main: MainStream::new(seq_id),
        };
        let init = init_payload(
            &config.app_id,
            &config.device_id,
            seq_id,
            snapshot_at_ms,
            retries,
        );
        live.send(&[
            Frame::Establish {
                stream: STREAM,
                parameters: Vec::new(),
            },
            Frame::Data {
                stream: STREAM,
                ack_id: 0,
                requires_ack: true,
                content_type: None,
                payload: init.into_bytes(),
            },
        ])
        .await?;
        let deadline = Instant::now() + ACK_TIMEOUT;
        while live.main.established.is_none() {
            if !live.read(deadline).await? {
                return Err(LiveError::Establish("no answer in 5 s".into()));
            }
        }
        if let Some(Err(e)) = &live.main.established {
            return Err(LiveError::Establish(e.clone()));
        }
        let deadline = Instant::now() + ACK_TIMEOUT;
        while !live.main.init_acked {
            if !live.read(deadline).await? {
                return Err(LiveError::Establish(
                    "the sync request was not acked".into(),
                ));
            }
        }
        Ok(live)
    }

    /// The cursor to resume from: see [`MainStream::seq_id`].
    pub fn seq_id(&self) -> i64 {
        self.main.seq_id()
    }

    /// The next event, or `None` once `deadline` has passed.
    pub async fn next_event(&mut self, deadline: Instant) -> Result<Option<Event>, LiveError> {
        loop {
            if let Some(event) = self.main.next_event() {
                return Ok(Some(event));
            }
            if !self.read(deadline).await? {
                return Ok(None);
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    async fn send(&mut self, frames: &[Frame]) -> Result<(), LiveError> {
        let mut out = Vec::new();
        for frame in frames {
            frame.encode(&mut out);
        }
        self.ws.send(Message::binary(out)).await?;
        Ok(())
    }

    /// One websocket message or one ping tick; `false` once `deadline` has passed.
    async fn read(&mut self, deadline: Instant) -> Result<bool, LiveError> {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => Ok(false),
            _ = self.ping.tick() => {
                if self.main.heard.elapsed() > PONG_TIMEOUT {
                    return Err(LiveError::Closed {
                        code: None,
                        reason: "no ping or pong for 30 s".into(),
                    });
                }
                self.send(&[Frame::Ping]).await?;
                Ok(true)
            }
            message = self.ws.next() => match message {
                None => Err(LiveError::Closed { code: None, reason: "end of stream".into() }),
                Some(Err(e)) => Err(e.into()),
                Some(Ok(Message::Binary(bytes))) => {
                    let reply = self.main.handle(&bytes)?;
                    if !reply.is_empty() {
                        self.send(&reply).await?;
                    }
                    Ok(true)
                }
                Some(Ok(Message::Close(frame))) => Err(LiveError::Closed {
                    code: frame.as_ref().map(|f| u16::from(f.code)),
                    reason: frame.map(|f| f.reason.to_string()).unwrap_or_default(),
                }),
                // mautrix drops text messages; websocket pings are answered by tungstenite.
                Some(Ok(_)) => Ok(true),
            },
        }
    }
}
