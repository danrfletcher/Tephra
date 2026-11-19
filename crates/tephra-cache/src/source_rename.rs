//! Renaming a source (G13): the one transaction that changes a source's
//! name everywhere the cache holds it, and writes the fix-log entry and
//! the pending list of link rewrites in the same breath. A crash after it
//! leaves everything the link rewrites need (`pending_fixes`), exactly as
//! a promote does.
//!
//! The cache refers to a source by id almost everywhere (`units`,
//! `scan_state`), so those rows need no change. The durable tables that
//! hold a source by NAME (`promoted_roots`, `pending_fixes`) follow it.
//! `fix_log` is history and keeps the names as they were when each entry
//! was written.

use rusqlite::params;

use crate::fix_log::{self, NewFixLog};
use crate::pending_fixes::{self, NewPendingFix};
use crate::{Cache, CacheError};

/// What [`Cache::begin_source_rename`] writes.
#[derive(Debug, Clone)]
pub struct NewSourceRename<'a> {
    pub old_name: &'a str,
    pub new_name: &'a str,
    pub created_at: i64,
    pub log: &'a NewFixLog,
    pub pending: &'a [NewPendingFix],
}

/// What [`Cache::begin_source_rename`] created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRenamed {
    pub source_id: i64,
    pub fix_id: i64,
}

impl Cache {
    /// Renames the source `old_name` to `new_name` and records the fix, in
    /// ONE transaction: the registry row, the `promoted_roots` rows that
    /// name it (as parent or as promoted source), the pending rows of
    /// earlier fixes that name it, then the log entry and the pending list
    /// of this rename. [`CacheError::SourceNotFound`] when `old_name` is
    /// not registered and [`CacheError::DuplicateSource`] when another
    /// source has `new_name`, compared without regard to case (FR14);
    /// either leaves nothing written.
    pub fn begin_source_rename(
        &self,
        new: &NewSourceRename<'_>,
    ) -> Result<SourceRenamed, CacheError> {
        let tx = self.connection().unchecked_transaction()?;
        let source_id: i64 = tx
            .query_row(
                "SELECT id FROM sources WHERE name = ?1",
                [new.old_name],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    CacheError::SourceNotFound(new.old_name.to_string())
                }
                e => CacheError::from(e),
            })?;
        let taken: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM sources WHERE id <> ?1 AND lower(name) = lower(?2))",
            params![source_id, new.new_name],
            |r| r.get(0),
        )?;
        if taken {
            return Err(CacheError::DuplicateSource(new.new_name.to_string()));
        }
        tx.execute(
            "UPDATE sources SET name = ?1 WHERE id = ?2",
            params![new.new_name, source_id],
        )?;
        tx.execute(
            "UPDATE promoted_roots SET parent = ?1 WHERE parent = ?2",
            params![new.new_name, new.old_name],
        )?;
        tx.execute(
            "UPDATE promoted_roots SET source = ?1 WHERE source = ?2",
            params![new.new_name, new.old_name],
        )?;
        for column in ["source", "old_source", "new_source"] {
            tx.execute(
                &format!("UPDATE pending_fixes SET {column} = ?1 WHERE {column} = ?2"),
                params![new.new_name, new.old_name],
            )?;
        }
        let fix_id = fix_log::insert(&tx, new.log)?;
        for p in new.pending {
            pending_fixes::insert(&tx, fix_id, p, new.created_at)?;
        }
        tx.commit()?;
        Ok(SourceRenamed { source_id, fix_id })
    }
}
