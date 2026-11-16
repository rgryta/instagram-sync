//! Read-only proof of the live socket: load the inbox page with an exported cookie jar, ask
//! the INBOX query for the mailbox's seq id, open the gateway socket from it, listen for
//! `seconds`, and say what came: operations by kind, and each delta type's count and field
//! names — shapes only, never a name, message, url or id.
//!
//!   instagram-live-check <cookies.json> [seconds=60]
//!
//! The cookie file is only read: rotations Instagram sends are applied in memory and
//! dropped. The socket sends nothing but its sync request and keep-alives (see `live.rs`).

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::time::Duration;

use chrono::Utc;
use instagram_client::cookies::Jar;
use instagram_client::graphql;
use instagram_client::live::{self, Event, Live};
use instagram_client::web::Web;
use serde_json::Value;
use tokio::time::Instant;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(path), seconds) = (args.next(), args.next()) else {
        eprintln!("usage: instagram-live-check <cookies.json> [seconds=60]");
        return ExitCode::from(2);
    };
    let Ok(seconds) = seconds.as_deref().unwrap_or("60").parse::<u64>() else {
        eprintln!("seconds must be a whole number");
        return ExitCode::from(2);
    };
    match run(&path, seconds).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("FAILED: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A field name as it is, but a key that is data (an id or an emoji used as a map key) as
/// `<key>`.
fn key(name: &str) -> String {
    let word = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if word {
        name.to_owned()
    } else {
        "<key>".into()
    }
}

/// Top-level field names, and one level into nested objects as `outer.inner`.
fn shape(raw: &Value, into: &mut BTreeSet<String>) {
    let Some(object) = raw.as_object() else {
        return;
    };
    for (name, value) in object {
        let name = key(name);
        if let Some(inner) = value.as_object() {
            into.extend(inner.keys().map(|k| format!("{name}.{}", key(k))));
        }
        into.insert(name);
    }
}

async fn run(path: &str, seconds: u64) -> Result<(), String> {
    let jar = Jar::from_json(&std::fs::read_to_string(path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("cookie file: {e}"))?;
    if jar.get("sessionid").is_none() {
        return Err("the cookie file holds no session (sessionid)".into());
    }
    let mut web = Web::new(jar).map_err(|e| e.to_string())?;
    let config = web.page().await.map_err(|e| e.to_string())?;
    println!(
        "page: logged in, app id {}, gateway user id {}",
        config.app_id,
        config.non_facebook_user_id.is_some()
    );

    let inbox = web
        .query(
            &config,
            &graphql::INBOX,
            graphql::INBOX_FIELD,
            &graphql::inbox_variables(&config),
        )
        .await
        .map_err(|e| e.to_string())?
        .ok_or("inbox: data is null")?;
    let start = live::mailbox_seq_id(&inbox).ok_or("the inbox carries no seq id")?;
    let snapshot_at_ms = Utc::now().timestamp_millis();
    println!("inbox: seq id present");

    let mut jar = web.jar().clone();
    let mut socket = Live::connect(&config, &mut jar, start, snapshot_at_ms, 0)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    println!("socket: stream open, listening {seconds} s");

    let mut operations: BTreeMap<&str, usize> = BTreeMap::new();
    let mut deltas: BTreeMap<String, (usize, BTreeSet<String>)> = BTreeMap::new();
    let mut subscribed = false;
    let mut problems = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let ended = loop {
        match socket.next_event(deadline).await {
            Ok(None) => break None,
            Err(e) => break Some(e),
            Ok(Some(event)) => match event {
                Event::Connected {
                    subscribed_seq_id,
                    latest_seq_id,
                } => {
                    subscribed = true;
                    *operations.entry("subscribe").or_default() += 1;
                    if let (Some(from), Some(latest)) = (subscribed_seq_id, latest_seq_id) {
                        println!("subscribed: {} behind the latest", latest - from);
                    }
                }
                Event::Delta { typename, raw, .. } => {
                    let entry = deltas.entry(typename).or_default();
                    entry.0 += 1;
                    shape(&raw, &mut entry.1);
                }
                Event::Noop => *operations.entry("noop").or_default() += 1,
                Event::Resnapshot(why) => {
                    *operations.entry("resnapshot").or_default() += 1;
                    problems.push(format!("resnapshot {why}"));
                }
                Event::Failure(why) => {
                    *operations.entry("failure").or_default() += 1;
                    problems.push(format!("failure {why}"));
                }
            },
        }
    };
    let advanced = socket.seq_id() - start;
    if ended.is_none() {
        socket.close().await;
    }

    println!("connected (subscribe seen): {subscribed}");
    println!("operations: {operations:?}");
    println!(
        "deltas: {} in {} type(s), seq id advanced by {advanced}",
        deltas.values().map(|(n, _)| n).sum::<usize>(),
        deltas.len()
    );
    for (typename, (count, fields)) in &deltas {
        println!("  {typename} x{count}: {fields:?}");
    }
    for problem in &problems {
        println!("{problem}");
    }
    match ended {
        None => println!("socket: still open after {seconds} s, closed by us"),
        Some(e) => println!("socket ended early: {e}"),
    }
    println!("cookies rotated: {} (not saved)", web.take_rotated());
    Ok(())
}
