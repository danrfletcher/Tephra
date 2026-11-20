//! The source-health value a later PR shows in `source list`, derived from
//! an adapter error. Pure: nothing here reads a clock or a store, and
//! nothing is wired into `source list` yet.

use std::time::SystemTime;

use crate::error::RemoteError;

/// The part of a source's status an adapter failure decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceHealth {
    /// Backing off a rate limit; syncing resumes at `retry_at`.
    Syncing { retry_at: SystemTime },
    /// The token is missing, unreadable or rejected.
    AuthRequired,
}

/// `Syncing { retry_at }` for a rate limit, `AuthRequired` for a rejected
/// or missing token, `None` for everything else (a permission failure, a
/// missing item or a transient error does not change the source's health
/// by itself).
pub fn health_for_error(error: &RemoteError) -> Option<SourceHealth> {
    match error {
        RemoteError::RateLimited { retry_at } => Some(SourceHealth::Syncing {
            retry_at: *retry_at,
        }),
        RemoteError::AuthRejected => Some(SourceHealth::AuthRequired),
        _ => None,
    }
}
