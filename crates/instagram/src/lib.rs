//! Instagram's direct-message surface as the web page uses it, read-only and with no
//! browser: the page for the session's tokens, then four persisted GraphQL queries, and the
//! gateway socket that pushes the inbox's changes live (`live`). Nothing here can write to
//! the account — the only documents it knows are queries, and the socket only listens.

pub mod cookies;
pub mod dgw;
pub mod graphql;
pub mod live;
pub mod page;
pub mod web;
