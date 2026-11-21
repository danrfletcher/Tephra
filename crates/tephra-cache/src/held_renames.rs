//! The `held_renames` table: files the rename detector holds while it
//! waits to see whether a vanished file and a new one are a rename
//! (PR-9 R3, R8). See `schema` for why it is durable.

use rusqlite::{params, Connection};

use crate::CacheError;

/// Which side of a possible rename a held file is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HeldKind {
    /// A tracked file that is gone.
    Vanished,
    /// A file new to the index.
    Appeared,
}

impl HeldKind {
    fn as_str(self) -> &'static str {
        match self {
            HeldKind::Vanished => "vanished",
            HeldKind::Appeared => "appeared",
        }
    }
}

/// One held file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HeldFile {
    pub kind: HeldKind,
    pub source: String,
    /// Relative to the source's root.
    pub path: String,
    /// `None` for a file that was never hashed (over the size cap).
    pub hash: Option<String>,
}

/// Records `files` in one transaction; a file held already is replaced.
pub(crate) fn hold(conn: &Connection, files: &[HeldFile], now: i64) -> Result<(), CacheError> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO held_renames (kind, source, path, hash, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for f in files {
            stmt.execute(params![f.kind.as_str(), f.source, f.path, f.hash, now])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Removes `files` (settled: fixed, logged or no longer true).
pub(crate) fn release(conn: &Connection, files: &[HeldFile]) -> Result<(), CacheError> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt =
            tx.prepare("DELETE FROM held_renames WHERE kind = ?1 AND source = ?2 AND path = ?3")?;
        for f in files {
            stmt.execute(params![f.kind.as_str(), f.source, f.path])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Every held file, oldest first.
pub(crate) fn all(conn: &Connection) -> Result<Vec<HeldFile>, CacheError> {
    let mut stmt = conn
        .prepare("SELECT kind, source, path, hash FROM held_renames ORDER BY created_at, rowid")?;
    let rows = stmt.query_map([], |r| {
        let kind: String = r.get(0)?;
        Ok(HeldFile {
            kind: if kind == "vanished" {
                HeldKind::Vanished
            } else {
                HeldKind::Appeared
            },
            source: r.get(1)?,
            path: r.get(2)?,
            hash: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
