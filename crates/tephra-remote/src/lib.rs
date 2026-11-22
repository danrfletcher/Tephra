//! Typed, read-side access to GitHub and Google Drive, and the fakes to
//! test it against.
//!
//! - [`credentials`]: the credential-store trait, its Keychain store and
//!   the test-only file store. Adapters read the token from it on every
//!   request.
//! - [`github`] / [`drive`]: the [`github::GithubAdapter`] and
//!   [`drive::DriveAdapter`] traits (read side only: nothing that writes,
//!   deletes, moves or renames, and nothing that lists every repo or Drive
//!   of an account) and their real HTTP implementations.
//! - [`error`], [`rate_limit`], [`health`], [`clock`]: the failure
//!   vocabulary (rate limit E1, rejected token E2, unreachable, moved) and
//!   the pure rules around it. Adapters never sleep.
//! - [`testing`] (feature `test-support`): in-process fake servers.
//!
//! This crate does not depend on `tephra-cache`, so a failure here cannot
//! touch listings, backlinks, cached files or queues by construction. The
//! real adapters open no listener and no webhook (F10).

use std::future::Future;
use std::pin::Pin;

pub mod api_url;
pub mod base_url;
pub mod clock;
pub mod credentials;
pub mod drive;
pub mod error;
pub mod github;
pub mod health;
pub mod rate_limit;
mod transport;

#[cfg(feature = "test-support")]
pub mod testing;

pub use clock::{Clock, FakeClock, SystemClock};
pub use error::{Conditional, RemoteError, UnreachableCause};
pub use health::{health_for_error, SourceHealth};
pub use transport::{AdapterOptions, BodyStream};

/// The future an adapter method returns. Boxed so the adapter traits are
/// object safe (`Arc<dyn GithubAdapter>`).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What every adapter call returns.
pub type RemoteResult<T> = Result<T, RemoteError>;
