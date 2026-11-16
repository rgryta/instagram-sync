//! The ingester: Instagram payloads to archive rows, and the loops that fetch them.

pub mod backfill;
pub mod config;
pub mod listen;
pub mod normalize;
pub mod poll;
pub mod ratelimit;
pub mod runner;
pub mod source;

use instagram_client::web::WebError;

/// Why a pass failed. Every variant ends the pass, never the process.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error(transparent)]
    Web(#[from] WebError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Normalize(#[from] normalize::NormalizeError),
}
