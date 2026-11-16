//! The credential-isolation guard.
//!
//! The MCP process holds no Instagram credential and must not be able to reach
//! Instagram, and in Cargo that is a link-time fact: the binary's package does not
//! depend on `instagram-client`, so the code that could use a session cookie is not in
//! the process at all. In Python this needed a test that spawns a fresh interpreter and
//! inspects `sys.modules` (`tests/test_mcp_isolation.py`).
//!
//! What is left to check is that the manifests still say so, that the settings type
//! still has nowhere to put a credential, and that the tools layer still holds no SQL of
//! its own. All three are cheap and all three are the invariants `AGENTS.md` names.

use std::path::{Path, PathBuf};

use instagram_core::db::{create_pool, ensure_schema};

use crate::support::{self, db_test};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn manifest(path: &str) -> String {
    std::fs::read_to_string(repo_root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn source(path: &str) -> String {
    std::fs::read_to_string(repo_root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// One manifest section, ending where the next one starts.
///
/// `[dependencies]` is what reaches a binary target; `[dev-dependencies]` does not, and
/// reading to the end of the file would conflate the two.
fn section(path: &str, name: &str) -> String {
    let manifest = manifest(path);
    let after = manifest
        .split_once(&format!("\n[{name}]"))
        .unwrap_or_else(|| panic!("{path} has no [{name}] section"))
        .1;
    after
        .split_once("\n[")
        .map_or(after, |(section, _)| section)
        .to_owned()
}

#[test]
fn the_mcp_binary_does_not_depend_on_the_instagram_client() {
    // The root package is the one that carries `src/bin/instagram-mcp.rs`.
    let dependencies = section("Cargo.toml", "dependencies");
    assert!(
        !dependencies.contains("instagram-client"),
        "the MCP binary must not link instagram-client:\n{dependencies}"
    );
}

#[test]
fn the_mcp_binary_does_not_depend_on_the_ingester_either() {
    // `instagram-ingest` links `instagram-client`, so an edge to it is the same edge.
    let dependencies = section("Cargo.toml", "dependencies");
    assert!(
        !dependencies.contains("instagram-ingest"),
        "the MCP binary must not link instagram-ingest:\n{dependencies}"
    );
}

#[test]
fn neither_the_core_nor_the_mcp_crate_depends_on_the_instagram_client() {
    // A transitive edge would put the code back in the binary just as surely.
    for path in ["crates/core/Cargo.toml", "crates/mcp/Cargo.toml"] {
        assert!(
            !manifest(path).contains("instagram-client"),
            "{path} must not depend on instagram-client"
        );
    }
}

#[test]
fn nothing_in_the_read_process_links_an_http_client() {
    // Only the ingester's `instagram-client` speaks to Instagram; this process speaks to Postgres
    // and to its own callers and to nothing else. `axum` and `hyper` are the server it
    // is called through, not a way out.
    for path in [
        "Cargo.toml",
        "crates/core/Cargo.toml",
        "crates/mcp/Cargo.toml",
    ] {
        let dependencies = section(path, "dependencies");
        for forbidden in ["reqwest", "ureq", "isahc", "hyper-tls"] {
            assert!(
                !dependencies.contains(forbidden),
                "{path} must not depend on {forbidden}:\n{dependencies}"
            );
        }
    }
}

#[test]
fn the_settings_type_has_nowhere_to_put_a_credential() {
    // `tests/test_mcp_isolation.py::test_mcp_settings_have_no_session_field`, which
    // inspected `McpSettings.model_fields`. Here the type is the whole of it, so the
    // check is that the type still says so.
    let config = source("crates/core/src/config.rs");
    let settings = config
        .split_once("pub struct McpSettings {")
        .expect("McpSettings is declared")
        .1
        .split_once('}')
        .expect("the struct closes")
        .0;
    for forbidden in [
        "sessionid",
        "csrftoken",
        "token",
        "cookie",
        "user_agent",
        "account",
    ] {
        assert!(
            !settings.contains(forbidden),
            "McpSettings must have no {forbidden} field:\n{settings}"
        );
    }
}

#[test]
fn the_tools_layer_holds_no_sql_of_its_own() {
    // `AGENTS.md`: all SQL lives in the store. `set-alias` is the only write in the whole
    // read surface, and the MCP role is granted nothing else.
    let tools = source("crates/mcp/src/tools.rs").to_lowercase();
    for statement in ["insert into", "update ", "delete from", "select "] {
        assert!(
            !tools.contains(statement),
            "raw {statement:?} in the tools layer"
        );
    }
}

#[test]
fn the_renderer_reaches_no_database_either() {
    let render = source("crates/mcp/src/render.rs");
    assert!(
        !render.contains("sqlx") && !render.contains("PgPool"),
        "the renderer takes rows, it does not fetch them"
    );
}

#[test]
fn the_instagram_crate_reaches_no_database() {
    // The Instagram layer fetches and parses, and a store call there would let a fetch
    // decide what gets written.
    let dependencies: String = manifest("crates/instagram/Cargo.toml")
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect();
    assert!(
        !dependencies.contains("sqlx"),
        "the Instagram client must not depend on sqlx"
    );
    let mut dirs = vec![repo_root().join("crates/instagram/src")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("the crate") {
            let path = entry.expect("a source file").path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("a source file");
            assert!(
                !body.contains("store::") && !body.contains("store}"),
                "{} reaches into the store",
                path.display()
            );
        }
    }
}

#[test]
fn the_session_jar_migration_revokes_what_the_mcp_role_is_granted_by_default() {
    // `provision-instagram-db.sql` grants `instagram_mcp` SELECT on every table the
    // ingester creates, and this one holds a live login.
    let migration = source("db-migrations/pg/0005_session_jar.sql");
    assert!(
        migration.contains("REVOKE ALL ON ingest_session FROM instagram_mcp"),
        "{migration}"
    );
}

const GRANTS_DB: &str = "instagram_session_grants";

db_test!(the_mcp_role_cannot_read_the_session_jar, |f| {
    // The production grants on a database of its own: the MCP role, its schema usage,
    // and the default SELECT on every table the migrating role creates.
    let Some(dsn) = support::container_dsn().await else {
        return;
    };
    sqlx::query(
        "DO $$ BEGIN CREATE ROLE instagram_mcp; \
         EXCEPTION WHEN duplicate_object THEN NULL; END $$",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query("DROP DATABASE IF EXISTS instagram_session_grants WITH (FORCE)")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("CREATE DATABASE instagram_session_grants")
        .execute(&f.pool)
        .await
        .unwrap();
    let (base, _) = dsn.rsplit_once('/').expect("the dsn names a database");
    let pool = create_pool(&format!("{base}/{GRANTS_DB}")).await.unwrap();
    sqlx::query("GRANT USAGE ON SCHEMA public TO instagram_mcp")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO instagram_mcp",
    )
    .execute(&pool)
    .await
    .unwrap();
    ensure_schema(&pool).await.unwrap();

    // As the MCP role: the archive reads, and the jar does not.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL ROLE instagram_mcp")
        .execute(&mut *tx)
        .await
        .unwrap();
    let messages: i64 = sqlx::query_scalar("SELECT count(*) FROM message")
        .fetch_one(&mut *tx)
        .await
        .expect("the default grant reaches the archive");
    assert_eq!(messages, 0);
    let denied = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM ingest_session")
        .fetch_one(&mut *tx)
        .await
        .expect_err("the MCP role must not read the session jar");
    assert_eq!(
        denied.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42501"),
        "{denied}"
    );
    drop(tx);
    pool.close().await;
    sqlx::query("DROP DATABASE IF EXISTS instagram_session_grants WITH (FORCE)")
        .execute(&f.pool)
        .await
        .unwrap();
});
