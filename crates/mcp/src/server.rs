//! The MCP server: streamable HTTP at `/mcp`, eight tools, no Instagram credential.
//!
//! The pool is opened before the listener binds and the shipped migrations are asserted
//! present — never applied. This process runs as a role with no DDL rights, and a schema
//! it cannot create is a schema it must not pretend to have.
//!
//! Seven of the eight tools are reads. The eighth writes the two alias tables and
//! nothing else, which is a database fact here rather than a convention: the grant in
//! `0001_init.sql` is all the write authority `instagram_mcp` has.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use instagram_core::config::McpSettings;
use instagram_core::db::{DbError, assert_migrations, create_pool};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use sqlx::postgres::PgPool;

use crate::tools::{self, ToolError};

/// Byte-identical to the Python server's, hard wrap and trailing newline included: the
/// two are the same server to a client for as long as both are shipped, and a
/// description that drifts is a description one of them is wrong about.
pub const INSTRUCTIONS: &str = "\
Instagram direct messages, from a permanent archive in PostgreSQL written by a separate
ingester. Timestamps are ISO 8601 UTC. Nothing here can send, like, follow or mark
anything seen, and nothing here reads Instagram live — only what has already been
archived.

Roughly half this corpus is not typed text but a reel, post, story or link somebody
forwarded, so `shares` is the tool for \"what did anyone ever send me about X\" and is
the one question no Instagram client can answer. `share_owner` is whoever AUTHORED the
thing; the sender is whoever forwarded it, and they are rarely the same person.

A DM has no title, so conversations are named after the people in them. Names come from
an alias if one is set, else the display name, else the handle. Use `people` when a name
is ambiguous, and `set-alias` to record what someone is actually called.

Thread and item ids are printed in brackets on every line and are what `messages` takes.
`status` says how much history is still being backfilled — an empty answer can mean
\"not archived yet\" rather than \"never happened\".
";

/// The name the server reports to clients, which MCPHub joins to each tool name with a
/// `-`: `instagram-threads`.
pub const SERVER_NAME: &str = "instagram";

#[derive(Clone)]
pub struct InstagramMcp {
    pool: PgPool,
}

/// Turn a tool's answer into a protocol response.
///
/// A usage error is the caller's and goes back as `invalid_params` with its own text. A
/// SQL error is ours: it is logged in full and the caller is told only that the read
/// failed, because a Postgres error string can carry the DSN and the data.
fn finish(result: Result<String, ToolError>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(body) => Ok(CallToolResult::success(vec![ContentBlock::text(body)])),
        Err(ToolError::Usage(message)) => Err(ErrorData::invalid_params(message, None)),
        Err(ToolError::Sql(error)) => {
            tracing::error!(%error, "the archive could not be read");
            Err(ErrorData::internal_error(
                "the archive could not be read".to_string(),
                None,
            ))
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ThreadsArgs {
    pub since: Option<String>,
    pub group: Option<bool>,
    pub account: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MessagesArgs {
    pub thread_id: String,
    pub after: Option<String>,
    pub before: Option<String>,
    pub account: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RevisionsArgs {
    pub thread_id: String,
    pub item_id: String,
    pub account: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    pub person: Option<String>,
    pub thread_id: Option<String>,
    pub share_kind: Option<String>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub account: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct PeopleArgs {
    pub name: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct SharesArgs {
    pub owner: Option<String>,
    pub share_kind: Option<String>,
    pub thread_id: Option<String>,
    pub after: Option<String>,
    pub before: Option<String>,
    pub account: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetAliasArgs {
    pub alias: String,
    /// Omit, or pass an empty list, to delete the alias.
    #[serde(default)]
    pub members: Vec<String>,
    pub note: Option<String>,
}

// Tool names are hyphenated and declared explicitly: MCPHub joins them to the server
// name with `-`, so these read `instagram-threads` rather than
// `instagram-instagram_threads`, which would stutter and mix both separators. The Rust
// method names below are incidental.
//
// The annotations are declared so a client can tell a read from a write without parsing
// a description. A tool with no hints is assumed able to change things, so an
// unannotated read looks no safer than a write — and ChatGPT blocked read calls before
// they left.
#[tool_router]
impl InstagramMcp {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    #[tool(
        name = "threads",
        description = "List archived conversations with message counts and last activity, newest \
                       first. `since` (ISO 8601 or a Unix epoch) narrows to threads active since \
                       then and counts only what arrived after it - the way to catch up. `group` \
                       selects group chats or one-to-one DMs.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn threads(
        &self,
        Parameters(args): Parameters<ThreadsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::threads(
                &self.pool,
                args.account.as_deref(),
                args.since.as_deref(),
                args.group,
                args.limit,
            )
            .await,
        )
    }

    #[tool(
        name = "messages",
        description = "Read one thread's archived messages, oldest first. With `after` you get the \
                       oldest `limit` messages after it, so you read a period forward from its \
                       start; without `after` you get the newest `limit`, and `before` walks back \
                       from there. `after`/`before` accept ISO 8601 or a Unix epoch. When the \
                       period holds more than `limit`, the reply ends with how many more there are \
                       and the exact `after` (or `before`) to pass for the next page. Shared \
                       reels, posts and links are rendered as their content, not as ids.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn messages(
        &self,
        Parameters(args): Parameters<MessagesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::messages(
                &self.pool,
                &args.thread_id,
                args.account.as_deref(),
                args.after.as_deref(),
                args.before.as_deref(),
                args.limit,
            )
            .await,
        )
    }

    #[tool(
        name = "revisions",
        description = "The edit history of one message, oldest first, and the deleted text if it \
                       was deleted. Other tools mark a message (edited) or [deleted] and withhold \
                       the earlier and deleted text; this is where to read it. Revision 1 is the \
                       earliest text this archive saw, not necessarily what was originally sent.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn revisions(
        &self,
        Parameters(args): Parameters<RevisionsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::revisions(
                &self.pool,
                &args.thread_id,
                &args.item_id,
                args.account.as_deref(),
            )
            .await,
        )
    }

    #[tool(
        name = "search",
        description = "Full-text search the archive, best match first. The caption and author \
                       handle of a shared reel or post are indexed alongside typed text, so both \
                       are searchable. `person` narrows by sender and takes an alias, a handle, a \
                       whole display name or a raw id, and `after`/`before` (ISO 8601 or a Unix \
                       epoch) narrow it to a period - which is what to use when the question is \
                       about a particular month, since ranking by relevance otherwise lets the \
                       best matches of all time crowd that month out.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::search(
                &self.pool,
                &args.query,
                args.account.as_deref(),
                args.thread_id.as_deref(),
                args.person.as_deref(),
                args.share_kind.as_deref(),
                args.after.as_deref(),
                args.before.as_deref(),
                args.limit,
            )
            .await,
        )
    }

    #[tool(
        name = "people",
        description = "Resolve part of a handle, display name or alias to the people it matches, \
                       showing every name form held for each so an ambiguous match is visible \
                       rather than silently picked. Each result carries the DM thread id to pass \
                       to `messages`. The aliases in force are listed underneath.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn people(
        &self,
        Parameters(args): Parameters<PeopleArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(tools::people(&self.pool, args.name.as_deref(), args.limit).await)
    }

    #[tool(
        name = "shares",
        description = "Every reel, post, story and link ever sent or received, newest first, with \
                       permalinks. `owner` is the handle of whoever AUTHORED it, not whoever \
                       forwarded it. `share_kind` is one of reel, post, story, link. With no \
                       `owner` the most-shared authors are listed underneath.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn shares(
        &self,
        Parameters(args): Parameters<SharesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(
            tools::shares(
                &self.pool,
                args.owner.as_deref(),
                args.share_kind.as_deref(),
                args.thread_id.as_deref(),
                args.account.as_deref(),
                args.after.as_deref(),
                args.before.as_deref(),
                args.limit,
            )
            .await,
        )
    }

    #[tool(
        name = "status",
        description = "Ingest health: what each loop last did, whether the Instagram session is \
                       still alive, how much is stored and how many threads still owe backfill.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn status(&self, Parameters(_): Parameters<NoArgs>) -> Result<CallToolResult, ErrorData> {
        finish(tools::status(&self.pool).await)
    }

    #[tool(
        name = "set-alias",
        description = "Name a set of Instagram identities so they can be found and rendered as one \
                       person. An alias beats their Instagram name everywhere. Members are written \
                       `account:ig_user_id`, as printed by `people`; omit them to delete the alias.",
        annotations(
            read_only_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_alias(
        &self,
        Parameters(args): Parameters<SetAliasArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        finish(tools::set_alias(&self.pool, &args.alias, &args.members, args.note.as_deref()).await)
    }
}

#[tool_handler]
impl ServerHandler for InstagramMcp {
    /// Written out rather than left to `#[tool_handler(instructions = "…")]`: that
    /// argument takes a literal, and a literal folded to fit this file would drop the
    /// hard wrap the Python server sends. This is what keeps the two byte-identical.
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS.to_string())
    }
}

/// Own the pool and refuse an unmigrated database.
///
/// Separate from [`run`] so a test can call it without a listener.
pub async fn open_pool(settings: &McpSettings) -> Result<PgPool, DbError> {
    let pool = create_pool(settings.dsn.expose()).await?;
    if let Err(error) = assert_migrations(&pool).await {
        // This process runs as a role with no DDL rights. A schema it cannot create is
        // one it must not pretend to have: failing at startup names the problem, where
        // serving on regardless would fail one query at a time and blame the caller.
        pool.close().await;
        return Err(error);
    }
    Ok(pool)
}

/// The container healthcheck's endpoint: a live query, not a liveness constant.
///
/// The pool is opened before the listener binds, so by the time this can answer at all
/// there is a pool — the Python's "starting" case is a refused connection here, which
/// Docker reads as unhealthy just the same. What is left worth checking is the database,
/// because `compose.yaml` has `autoheal=true` on this container and a server that cannot
/// reach Postgres must not report itself well.
async fn healthz(pool: PgPool) -> Response {
    match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&pool)
        .await
    {
        Ok(_) => "ok".into_response(),
        Err(error) => {
            tracing::error!(%error, "the healthcheck could not reach the database");
            (StatusCode::SERVICE_UNAVAILABLE, "database unreachable").into_response()
        }
    }
}

/// `/mcp` plus `/healthz`.
///
/// `/healthz` is a plain route on this router and only `/mcp` is nested onto the MCP
/// service, so the `Host` allowlist below does not apply to it and `--health-check`
/// needs no entry of its own.
/// `tests/archive/server.rs::healthz_answers_a_host_the_mcp_endpoint_rejects` holds
/// that: if it ever stops being true the healthcheck starts failing for a reason
/// nothing about the container would explain.
pub fn build_router(pool: PgPool, allowed_hosts: Vec<String>) -> Router {
    let probe = pool.clone();
    let service = StreamableHttpService::new(
        move || Ok(InstagramMcp::new(pool.clone())),
        Arc::new(NeverSessionManager::default()),
        stateless_config(allowed_hosts),
    );
    Router::new()
        .route("/healthz", get(move || healthz(probe.clone())))
        .nest_service("/mcp", service)
}

fn stateless_config(allowed_hosts: Vec<String>) -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default();
    // There is no per-client state to keep, and statelessness takes session affinity off
    // the table behind MCPHub — what `stateless_http=True` bought the Python server.
    config.legacy_session_mode = false;
    // rmcp's guard against DNS rebinding. An empty list would mean "answer any `Host`",
    // so the list handed in here is never empty — see `DEFAULT_ALLOWED_HOSTS`. MCPHub
    // addresses this server by container name, so production sets `instagram-mcp`.
    config.allowed_hosts = allowed_hosts;
    config
}

pub async fn run(settings: &McpSettings) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pool = open_pool(settings).await?;
    let address: SocketAddr = format!("{}:{}", settings.host, settings.port).parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    // Logged because the failure mode is a server that boots clean and then refuses
    // every tool call with a 403: one line here turns a baffling outage into an obvious
    // one. `/healthz` is outside this list, so a container can look healthy while every
    // call is rejected.
    tracing::info!(
        allowed_hosts = %settings.allowed_hosts.join(", "),
        "answering /mcp only for these Host headers"
    );
    tracing::info!(%address, "serving the instagram archive over MCP");
    axum::serve(listener, build_router(pool, settings.allowed_hosts.clone()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Probe our own `/healthz`, for the container healthcheck.
///
/// The image is `scratch`: no shell, no curl, no interpreter, so the healthcheck in
/// `deploy/compose.yaml` runs this binary with `--health-check` instead.
///
/// Hand-rolled over TCP rather than through an HTTP client crate: it reads one status
/// line from a loopback socket, and a dependency for that is a dependency to keep
/// patched for no gain. It is also the one thing in this binary that opens a socket at
/// all, which is why it speaks to loopback and nothing else.
pub async fn health_check(host: &str, port: u16) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // The bind address may be the wildcard, which is not a destination.
    let host = match host {
        "0.0.0.0" | "::" | "" => "127.0.0.1",
        host => host,
    };
    let target = format!("{host}:{port}");
    let mut stream = tokio::net::TcpStream::connect(&target)
        .await
        .map_err(|error| format!("could not connect to {target}: {error}"))?;
    stream
        .write_all(
            format!("GET /healthz HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .map_err(|error| format!("could not send the probe: {error}"))?;

    let mut status = String::new();
    BufReader::new(stream)
        .read_line(&mut status)
        .await
        .map_err(|error| format!("no answer from {target}: {error}"))?;
    if status.contains(" 200 ") {
        return Ok(());
    }
    Err(format!("/healthz answered {}", status.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names() -> Vec<String> {
        InstagramMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    #[test]
    fn every_tool_is_registered() {
        let mut names = tool_names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "messages",
                "people",
                "revisions",
                "search",
                "set-alias",
                "shares",
                "status",
                "threads",
            ]
        );
    }

    #[test]
    fn names_are_hyphenated_not_mixed() {
        // MCPHub joins server and tool with `-`; an underscore would read as
        // `instagram-set_alias`, mixing both separators in one identifier.
        for name in tool_names() {
            assert!(!name.contains('_'), "{name}");
        }
    }

    #[test]
    fn tools_describe_themselves() {
        // MCPHub shows these descriptions; an unlabelled tool is an unusable one.
        for tool in InstagramMcp::tool_router().list_all() {
            assert!(tool.description.is_some(), "{}", tool.name);
        }
    }

    #[test]
    fn a_client_can_tell_which_tools_only_read() {
        // With no hints every tool looks able to write, so a read was no safer on its
        // face than `set-alias` beside it.
        for tool in InstagramMcp::tool_router().list_all() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", tool.name));
            assert_eq!(
                annotations.read_only_hint,
                Some(tool.name != "set-alias"),
                "{}",
                tool.name
            );
        }
    }

    #[test]
    fn the_instructions_say_what_the_archive_is_and_is_not() {
        // This is the one server whose read-only-ness is a risk control, not a nicety.
        assert!(INSTRUCTIONS.contains("shares"));
        assert!(INSTRUCTIONS.contains("Nothing here can send, like, follow or mark"));
        assert!(INSTRUCTIONS.ends_with("\"never happened\".\n"));
    }

    #[tokio::test]
    async fn the_health_check_target_is_never_the_wildcard_address() {
        // Connecting to 0.0.0.0 is not a thing the healthcheck can do; port 1 is closed,
        // so the error names the address it actually tried.
        let error = health_check("0.0.0.0", 1).await.unwrap_err();
        assert!(error.contains("127.0.0.1:1"), "{error}");
    }
}
