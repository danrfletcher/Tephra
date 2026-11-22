//! What keeps a remote source current. PR-5's GitHub source (`add`, list and
//! the first index) is in `crate::remote_github`; this is what runs after it.

pub mod fetch;
pub mod poll;
pub mod safe_path;
