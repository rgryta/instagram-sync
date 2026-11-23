//! Voice messages to text, as messenger-sync and whatsapp-sync transcribe theirs.
//!
//! The [`Worker`] takes the waiting voice messages newest first, fetches each one's audio
//! by the link it carries (or the fresh one a re-read stored), and sends it to the
//! whisper.cpp server, which converts any format. Only the words are stored.
//!
//! The link expires after a few days, so most of the archive's voice messages arrive at
//! the worker with a dead one. [`relink_once`], a step of the full pass, re-reads the page
//! that holds such a message and stores the fresh link it carries; the worker then tries
//! again. A fresh link the CDN refuses too is given up.
//!
//! How a step fails decides what happens:
//!
//! - whisper refuses the audio, or a fresh link is refused: `failed`, never retried;
//! - the link the message arrived with is refused: `expired`, for [`relink_once`];
//! - the CDN or whisper cannot be reached: left waiting, and the rest of the batch waits
//!   for the next pass, which would only fail the same way.

use std::collections::HashMap;
use std::time::Duration;

use instagram_core::store::{self, ExpiredVoice, WaitingVoice};
use serde_json::Value;
use sqlx::PgPool;

use crate::IngestError;
use crate::backfill::BACKFILL_RESERVE;
use crate::source::Source;

const BATCH: i64 = 20;
pub const INTERVAL: Duration = Duration::from_secs(120);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// A long voice message on the iGPU takes a while, and a slow answer is still an answer.
const WHISPER_TIMEOUT: Duration = Duration::from_secs(300);

/// The audio link a message node carries: GraphQL's `content.audio_attachments`, or the
/// REST era's `voice_media`.
pub fn audio_url(node: &Value) -> Option<String> {
    [
        "/content/audio_attachments/0/attachment_cdn_url",
        "/voice_media/media/audio/audio_src",
    ]
    .iter()
    .find_map(|path| node.pointer(path).and_then(Value::as_str))
    .filter(|url| url.starts_with("http"))
    .map(str::to_owned)
}

enum Outcome {
    Done(String),
    Failed(String),
    Expired(String),
    /// Try again next pass.
    Later(String),
}

pub struct Worker {
    pool: PgPool,
    http: reqwest::Client,
    whisper: String,
}

impl Worker {
    pub fn new(pool: PgPool, whisper_url: &str) -> Self {
        Self {
            pool,
            http: reqwest::Client::new(),
            whisper: format!("{}/inference", whisper_url.trim_end_matches('/')),
        }
    }

    pub async fn run(self, interval: Duration) {
        loop {
            if let Err(e) = self.pass().await {
                tracing::warn!("transcription pass failed: {e}");
            }
            tokio::time::sleep(interval).await;
        }
    }

    async fn pass(&self) -> sqlx::Result<()> {
        for voice in store::waiting_voice(&self.pool, BATCH).await? {
            match self.transcribe(&voice).await {
                Outcome::Done(words) => {
                    store::set_transcript(&self.pool, &voice, Some(&words), "done").await?;
                    tracing::info!("transcribed a voice message");
                }
                Outcome::Failed(why) => {
                    store::set_transcript(&self.pool, &voice, None, "failed").await?;
                    tracing::warn!("a voice message will not be transcribed: {why}");
                }
                Outcome::Expired(why) => {
                    store::set_transcript(&self.pool, &voice, None, "expired").await?;
                    tracing::info!("a voice message's link has expired: {why}");
                }
                Outcome::Later(why) => {
                    tracing::warn!("transcription waits for the next pass: {why}");
                    break;
                }
            }
        }
        Ok(())
    }

    async fn transcribe(&self, voice: &WaitingVoice) -> Outcome {
        let relinked = voice.voice_url.is_some();
        let Some(url) = voice.voice_url.clone().or_else(|| audio_url(&voice.raw)) else {
            return Outcome::Failed("no audio link".into());
        };
        let response = match self.http.get(url).timeout(DOWNLOAD_TIMEOUT).send().await {
            Ok(response) => response,
            Err(e) => return Outcome::Later(format!("CDN: {}", e.without_url())),
        };
        let status = response.status();
        if status.is_client_error() {
            let why = format!("CDN answered {status}");
            return if relinked {
                Outcome::Failed(why)
            } else {
                Outcome::Expired(why)
            };
        }
        if !status.is_success() {
            return Outcome::Later(format!("CDN answered {status}"));
        }
        let audio = match response.bytes().await {
            Ok(audio) => audio.to_vec(),
            Err(e) => return Outcome::Later(format!("CDN: {}", e.without_url())),
        };
        self.whisper(audio).await
    }

    /// The whisper.cpp server's `/inference`, read as whatsapp-sync reads it: an `error`
    /// in the answer, even on a 500, is audio it cannot read; a 5xx without one is a server
    /// that fell over.
    async fn whisper(&self, audio: Vec<u8>) -> Outcome {
        let file = reqwest::multipart::Part::bytes(audio)
            .file_name("voice.ogg")
            .mime_str("audio/ogg")
            .expect("a valid mime type");
        let form = reqwest::multipart::Form::new()
            .part("file", file)
            .text("response_format", "json")
            .text("temperature", "0.0");
        let response = match self
            .http
            .post(&self.whisper)
            .multipart(form)
            .timeout(WHISPER_TIMEOUT)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => return Outcome::Later(format!("whisper: {e}")),
        };
        let status = response.status();
        let body: Value = match response.bytes().await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            Err(e) => return Outcome::Later(format!("whisper: {e}")),
        };
        if let Some(error) = body
            .get("error")
            .filter(|e| !matches!(e, Value::Null | Value::Bool(false)) && e.as_str() != Some(""))
        {
            return Outcome::Failed(format!("whisper refused the audio: {error}"));
        }
        if status.is_server_error() {
            return Outcome::Later(format!("whisper answered {status}"));
        }
        if !status.is_success() {
            return Outcome::Failed(format!("whisper answered {status}"));
        }
        let words = body.get("text").and_then(Value::as_str).unwrap_or_default();
        Outcome::Done(words.trim().to_owned())
    }
}

/// A string or numeric id.
fn id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The link the page carries for `item_id`, if the page holds it.
fn link_on(page: &Value, item_id: &str) -> Option<String> {
    page.pointer("/slide_messages/edges")?
        .as_array()?
        .iter()
        .filter_map(|edge| edge.get("node"))
        .find(|node| id(node.get("message_id")).as_deref() == Some(item_id))
        .and_then(audio_url)
}

/// Spend at most `request_budget` requests fetching fresh links for voice messages whose
/// link expired, one page each, newest first. Returns the requests spent.
pub async fn relink_once(
    pool: &PgPool,
    source: &mut impl Source,
    account_id: &str,
    request_budget: i64,
) -> Result<u64, IngestError> {
    if request_budget <= 0 {
        return Ok(0);
    }
    let expired = store::expired_voice(pool, account_id, request_budget).await?;
    if expired.is_empty() {
        return Ok(0);
    }
    // The inbox, cached for the pass, maps the archive's thread ids to the fbids a
    // request addresses a conversation by.
    let handles: HashMap<String, String> = source
        .inbox()
        .await?
        .iter()
        .filter_map(|raw| Some((id(raw.get("thread_id"))?, id(raw.get("thread_fbid"))?)))
        .collect();
    let mut requests = 0;
    for voice in &expired {
        let Some(handle) = handles.get(&voice.thread_id) else {
            give_up(pool, account_id, voice, "its thread is not in the inbox").await?;
            continue;
        };
        // The page of what is older than the next message starts with this one; the
        // newest message is on the thread's newest page.
        let pages = match &voice.next_item_id {
            Some(next) => {
                source
                    .older(handle, &voice.thread_id, next, 1, BACKFILL_RESERVE)
                    .await?
            }
            None => source.thread(handle, None, 0, BACKFILL_RESERVE).await?.0,
        };
        requests += 1;
        match pages.iter().find_map(|page| link_on(page, &voice.item_id)) {
            Some(url) => store::relink_voice(pool, account_id, voice, &url).await?,
            None => give_up(pool, account_id, voice, "its page did not carry it").await?,
        }
    }
    Ok(requests)
}

async fn give_up(
    pool: &PgPool,
    account_id: &str,
    voice: &ExpiredVoice,
    why: &str,
) -> Result<(), IngestError> {
    tracing::warn!("a voice message's link cannot be refreshed: {why}");
    store::give_up_voice(pool, account_id, voice).await?;
    Ok(())
}
