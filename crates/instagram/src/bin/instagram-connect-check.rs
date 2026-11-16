//! Read-only proof that the browser is not needed for the inbox: load the inbox page with
//! an exported cookie jar, then run each read query once — the inbox, its second page, the
//! newest thread, and that thread's next page of messages — and say what came back as
//! counts only, never a name, message or id.
//!
//!   instagram-connect-check <cookies.json>
//!
//! The cookie file is only read: rotations Instagram sends are applied in memory and
//! dropped. It sends nothing that changes the account (see `graphql.rs`).

use std::process::ExitCode;

use instagram_client::cookies::Jar;
use instagram_client::graphql::{self, Doc};
use instagram_client::web::Web;
use serde_json::Value;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: instagram-connect-check <cookies.json>");
        return ExitCode::from(2);
    };
    match run(&path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("FAILED: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `{edges, page_info}` of a connection: how many edges, whether more follow, and the
/// cursor to the next page.
fn connection(value: Option<&Value>) -> (usize, bool, Option<String>) {
    let edges = value
        .and_then(|c| c.get("edges"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let info = value.and_then(|c| c.get("page_info"));
    let more = info
        .and_then(|i| i.get("has_next_page"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cursor = info
        .and_then(|i| i.get("end_cursor"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    (edges, more, cursor)
}

async fn run(path: &str) -> Result<(), String> {
    let jar = Jar::from_json(&std::fs::read_to_string(path).map_err(|e| e.to_string())?)
        .map_err(|e| format!("cookie file: {e}"))?;
    if jar.get("sessionid").is_none() {
        return Err("the cookie file holds no session (sessionid)".into());
    }
    let mut web = Web::new(jar).map_err(|e| e.to_string())?;
    let config = web.page().await.map_err(|e| e.to_string())?;
    println!(
        "page: logged in, app id {}, eimu {}, spin {}, comet_req {}",
        config.app_id,
        config.viewer_fbid.is_some(),
        config.spin.is_some(),
        config.comet_req
    );

    let ask = async |web: &mut Web, doc: &Doc, field: &str, variables: Value| {
        web.query(&config, doc, field, &variables)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{}: data.{field} is null", doc.name))
    };

    let inbox = ask(
        &mut web,
        &graphql::INBOX,
        graphql::INBOX_FIELD,
        graphql::inbox_variables(&config),
    )
    .await?;
    let (threads, more, cursor) = connection(inbox.get("threads_by_folder"));
    println!("inbox: {threads} thread(s), more {more}");
    let first = inbox
        .pointer("/threads_by_folder/edges/0/node/as_ig_direct_thread/thread_fbid")
        .and_then(Value::as_str)
        .map(str::to_owned);

    if let (Some(cursor), Some(fbid)) = (cursor, config.viewer_fbid.as_deref()) {
        let page = ask(
            &mut web,
            &graphql::THREAD_LIST,
            graphql::THREAD_LIST_FIELD,
            graphql::thread_list_variables(fbid, &cursor),
        )
        .await?;
        let (threads, more, _) = connection(page.get("threads_by_folder"));
        println!("inbox page 2: {threads} thread(s), more {more}");
    }

    let fbid = first.ok_or("the inbox listed no thread")?;
    let thread = ask(
        &mut web,
        &graphql::THREAD,
        graphql::THREAD_FIELD,
        graphql::thread_variables(&fbid),
    )
    .await?;
    let (messages, more, cursor) =
        connection(thread.pointer("/as_ig_direct_thread/slide_messages"));
    println!("thread: {messages} message(s), more {more}");

    if let Some(cursor) = cursor.filter(|_| more) {
        let older = ask(
            &mut web,
            &graphql::MESSAGES,
            graphql::MESSAGES_FIELD,
            graphql::messages_variables(&fbid, &cursor),
        )
        .await?;
        let (messages, more, _) = connection(older.pointer("/as_ig_direct_thread/slide_messages"));
        println!("older messages: {messages} message(s), more {more}");
    }
    println!("cookies rotated: {} (not saved)", web.take_rotated());
    Ok(())
}
