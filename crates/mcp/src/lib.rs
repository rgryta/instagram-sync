//! The read surface.
//!
//! This crate must never depend on `instagram-client`: the MCP process holds no
//! Instagram credential and must be unable to reach Instagram even by accident. In
//! Python that needed a test that spawns a fresh interpreter and inspects `sys.modules`;
//! here the absence of the dependency edge is what the linker enforces, and
//! `tests/archive/isolation.rs` checks the manifests still say so.

pub mod render;
pub mod server;
pub mod tools;
