//! `sources` table rows, the `source_status` enum, and the status
//! transition function (G5: "documented transition function, no
//! silent state-skipping").

use std::fmt;

use rusqlite::{Connection, OptionalExtension};

use crate::CacheError;

/// A source's lifecycle state.
///
/// - `Active`: mounted/reachable, last scan succeeded.
/// - `Stale`: known to need a rescan — either it just remounted after
///   being offline (E2) and hasn't been rescanned yet, or a scan is
///   otherwise known to be out of date.
/// - `Offline`: its root path was missing/unmounted at startup or
///   rescan time (E2). Its cached `units`/`links`/`tags`/`blocks` rows
///   are preserved untouched while offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStatus {
    Active,
    Stale,
    Offline,
}

impl SourceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceStatus::Active => "active",
            SourceStatus::Stale => "stale",
            SourceStatus::Offline => "offline",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(SourceStatus::Active),
            "stale" => Some(SourceStatus::Stale),
            "offline" => Some(SourceStatus::Offline),
            _ => None,
        }
    }
}

impl fmt::Display for SourceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A `sources` table row.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceRow {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub status: SourceStatus,
    pub last_scanned_at: Option<i64>,
}

/// An attempted status transition that is not one of the documented
/// legal edges below. Returned instead of silently applying (or
/// silently skipping a step in) the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: SourceStatus,
    pub to: SourceStatus,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal source status transition: {} -> {}",
            self.from, self.to
        )
    }
}

/// Validates a `from -> to` source-status transition. The legal edges,
/// documented so no transition is ever applied (or skipped) silently:
///
/// - `Active <-> Stale`: a rescan may find a source needs re-indexing
///   (`Active -> Stale`), and a completed rescan marks it current again
///   (`Stale -> Active`).
/// - `Active -> Offline` and `Stale -> Offline`: a source's root may
///   disappear at any time, whether it was previously current or
///   already known stale.
/// - `Offline -> Stale`: remounting an offline source (E2) is observed
///   as this transition — its cached rows become reachable again but
///   are not yet re-verified by a scan.
/// - `Offline -> Active` is deliberately **not** a legal direct edge:
///   E2 requires the remount path go `Offline -> Stale -> Active`, so a
///   rescan always happens before a remounted source is trusted as
///   current again. Skipping straight from `Offline` to `Active` is
///   rejected rather than silently allowed.
/// - Identity transitions (`X -> X`) are always legal and are a no-op.
pub fn validate_transition(from: SourceStatus, to: SourceStatus) -> Result<(), InvalidTransition> {
    use SourceStatus::*;

    let legal = from == to
        || matches!(
            (from, to),
            (Active, Stale)
                | (Stale, Active)
                | (Active, Offline)
                | (Stale, Offline)
                | (Offline, Stale)
        );

    if legal {
        Ok(())
    } else {
        Err(InvalidTransition { from, to })
    }
}

pub(crate) fn row_from_query(
    id: i64,
    name: String,
    path: String,
    status_raw: String,
    last_scanned_at: Option<i64>,
) -> Result<SourceRow, CacheError> {
    let status = SourceStatus::parse(&status_raw)
        .ok_or_else(|| CacheError::CorruptStatus(status_raw.clone()))?;
    Ok(SourceRow {
        id,
        name,
        path,
        status,
        last_scanned_at,
    })
}

pub(crate) fn get_by_name(conn: &Connection, name: &str) -> Result<Option<SourceRow>, CacheError> {
    let row = conn
        .query_row(
            "SELECT id, name, path, status, last_scanned_at FROM sources WHERE name = ?1",
            [name],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()?;

    match row {
        None => Ok(None),
        Some((id, name, path, status_raw, last_scanned_at)) => Ok(Some(row_from_query(
            id,
            name,
            path,
            status_raw,
            last_scanned_at,
        )?)),
    }
}
