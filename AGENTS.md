# Working in this repo

## Gates — all clean before every commit

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`cargo test` starts a `postgres:18-alpine` testcontainer, named `instagram-sync-test-pg`
and marked for reuse, so a run attaches to the one the last run left rather than
starting a second. `docker rm -f instagram-sync-test-pg` reclaims it. Tests that need it
skip with a printed reason when Docker is unreachable, so run the suite where Docker
works (on the Windows workstation that is WSL). **Never point tests at
`homelab-postgres`**: nothing in the test support reads a DSN from the environment.

## Commits

- Two lines maximum. No co-author trailers. No emojis.
- Subject line says what changed; the second line says why, if it is not obvious.

## Releases and deployment

A release is a version bump: change `version` under `[workspace.package]` in
`Cargo.toml` and merge to main. `.github/workflows/tag.yml` tags it and starts
`image.yml`, which builds `deploy/Dockerfile` and pushes `ghcr.io/rgryta/instagram-sync`
as `X.Y.Z` and `latest`. The live stack follows the version tags through wud. `ci.yml`
(fmt, clippy, test, release build) runs on every pull request.

`/homelab_stack/*` is the live stack and is **read-only to agents** unless the owner
asks for a deploy.

## Secrets

- `.env` files committed to this repo carry variable **names with empty values**.
- The DSN is a `Secret` (`crates/core/src/config.rs`) whose `Debug` is redacted, so a
  stray log line cannot print it. The client and the ingester log counts and type
  names only, never a message, a name or an id.
- The session (the cookie jar) lives in `ingest_session`, which migration 0005 revokes
  from the MCP role.
- Never commit a captured payload. Fixtures are synthetic: call-sign words for names
  and text, `example.com` for URLs, made-up ids.

## Invariants worth not breaking

- **The MCP binary never links `instagram-client` or `instagram-ingest`**: the process
  that serves the archive cannot reach Instagram, because the code to do so is not in
  it. The root package carrying `src/bin/instagram-mcp.rs` depends on `instagram-core`
  and `instagram-mcp-server` only, and `tests/archive/isolation.rs` keeps the manifests
  honest. The client and the ingester are AGPL-3.0; the rest is MIT.
- **All SQL lives in `crates/core/src/store/`.** Nothing else writes a query.
- **Read-only toward Instagram.** `crates/instagram/src/graphql.rs` names read queries
  only, and the live socket sends nothing but its sync request and keepalives. No
  sends, reactions, follows, `mark_seen` or receipts, ever.
- **The client never logs in.** A rejected session is recorded and retried after
  `INSTAGRAM_AUTH_RETRY`; fresh cookies come from a human in a real browser.
- Migrations are forward-only and never edited once applied.
