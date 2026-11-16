# instagram-sync

A permanent archive of Instagram direct messages in PostgreSQL, and a read-only MCP
server over it. All Rust.

Instagram's official API cannot be a DM archive: it is for Business and Creator
accounts, returns about the last 20 messages of a thread, and has no group chats at all.
So `instagram-ingest` reads DMs the way the web client does, without a browser: the
inbox page and four persisted GraphQL read queries over HTTPS, and the gateway socket
that pushes a signal when the inbox changes.

```
instagram-ingest ──> www.instagram.com (read queries) ──> PostgreSQL
        ↑                                                   │
        └── gateway socket: "something changed"             │
instagram-mcp ──────────────────────────────────────────────┘  (read-only)
```

## Layout

| path | what |
| ---- | ---- |
| `crates/instagram` | the `instagram-client` library: the cookie jar, the inbox page, the read queries and the live socket (AGPL-3.0, after mautrix-meta's `instameow`) |
| `crates/ingest` | the `instagram-ingest` binary: the poll, backfill and live loops (AGPL-3.0, it links the client) |
| `crates/core` | models, the pool, the migrations runner, and every SQL statement (`store/`) |
| `crates/mcp`, `src/bin/instagram-mcp.rs` | the read-only MCP server on `/mcp` |
| `db-migrations/pg` | the archive's schema, applied by `instagram-ingest` at boot |
| `deploy/` | the image, the compose stack, the role provisioning SQL |

## How it reads

A full pass every ~15 minutes walks the inbox, opens only the threads whose activity
moved past what is stored, and checks the last 7 days for edits and unsends. Between
passes, a change the socket reports starts a quick pass about 30 seconds later: the
inbox's first page and the threads on it that moved. Backfill reaches older history at
most 400 requests a UTC day, and stops between pages when a change is waiting.

The session is a cookie jar in the `ingest_session` table, seeded once from a browser
export and kept current as Instagram rotates it. A rejected session is recorded and
retried after 6 hours, never re-logged in: a process that signs itself in is the
loudest signal there is.

## What it never does

Nothing here sends, reacts, follows or marks anything seen. The client knows only read
queries, so it has no way to name a mutation, and the socket sends nothing but its sync
request and keepalives. That is the risk control as much as an MCP invariant: what
Instagram fingerprints is account state, and this never changes any. It is still an
unofficial client of a personal account, and no one should call that risk zero.

## Running

One image, two services (`deploy/compose.yaml`): `instagram-ingest` (the only container
with the session and DDL rights) and `instagram-mcp` (reads the archive, nothing else).
`.env.example` lists the three variables the stack needs.

See `AGENTS.md` for the rules of working in this repo.
