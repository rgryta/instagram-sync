//! One test binary, so one testcontainer is started for the whole suite.
//!
//! Rust has no session-scoped fixture, and a separate `tests/*.rs` file would mean a
//! separate process and a separate Postgres container each. The modules below share
//! one, taking turns through a mutex because they share its database.
//!
//! The modules named after a `tests/test_*.py` are ports of the read-side half of it.
//! The Python suite is still in the tree and still passing: this port adds a binary
//! beside it rather than replacing it.

mod ingest;
mod isolation;
mod loops;
mod server;
mod store;
mod support;
mod tools;
mod vocabulary;
