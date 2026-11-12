//! Shared helpers for the OOXML (docx/pptx) extractors: bounded zip
//! part reads and XML text-event handling.

use std::io::{Cursor, Read};

use quick_xml::events::{BytesRef, BytesText};
use quick_xml::Reader;
use zip::ZipArchive;

use super::{DocumentError, MAX_PART_BYTES};

pub(crate) type Archive<'a> = ZipArchive<Cursor<&'a [u8]>>;

pub(crate) fn corrupt(reason: impl Into<String>) -> DocumentError {
    DocumentError::Corrupt(reason.into())
}

pub(crate) fn open_archive(bytes: &[u8]) -> Result<Archive<'_>, DocumentError> {
    ZipArchive::new(Cursor::new(bytes)).map_err(|e| corrupt(format!("not a readable zip: {e}")))
}

/// Reads the part `name`, refusing one that inflates past
/// [`MAX_PART_BYTES`] (a zip bomb). `Ok(None)` when the zip has no such
/// part.
pub(crate) fn read_part(
    archive: &mut Archive<'_>,
    name: &str,
) -> Result<Option<Vec<u8>>, DocumentError> {
    let entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(corrupt(format!("cannot open {name}: {e}"))),
    };
    let mut out = Vec::new();
    entry
        .take(MAX_PART_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|e| corrupt(format!("cannot read {name}: {e}")))?;
    if out.len() as u64 > MAX_PART_BYTES {
        return Err(corrupt(format!(
            "{name} inflates past {MAX_PART_BYTES} bytes"
        )));
    }
    Ok(Some(out))
}

/// Like [`read_part`] but a missing part is a [`DocumentError::Corrupt`].
pub(crate) fn require_part(
    archive: &mut Archive<'_>,
    name: &str,
) -> Result<Vec<u8>, DocumentError> {
    read_part(archive, name)?.ok_or_else(|| corrupt(format!("missing {name}")))
}

pub(crate) fn reader(xml: &[u8]) -> Reader<&[u8]> {
    // Defaults: whitespace text is kept (it is real text inside a
    // `w:t` / `a:t`), and mismatched end tags are an error.
    Reader::from_reader(xml)
}

pub(crate) fn xml_err(part: &str, e: impl std::fmt::Display) -> DocumentError {
    corrupt(format!("malformed XML in {part}: {e}"))
}

/// The text of a text event, with predefined entities already split off
/// by the reader as [`BytesRef`]s.
pub(crate) fn push_text(
    out: &mut String,
    t: &BytesText<'_>,
    part: &str,
) -> Result<(), DocumentError> {
    let _ = part;
    out.push_str(&t.xml10_content());
    Ok(())
}

/// Appends what an `&name;` / `&#N;` reference stands for. An unknown
/// named entity is dropped (XML parts here never declare any; nothing
/// external is ever resolved).
pub(crate) fn push_ref(
    out: &mut String,
    r: &BytesRef<'_>,
    part: &str,
) -> Result<(), DocumentError> {
    if let Some(c) = r.resolve_char_ref().map_err(|e| xml_err(part, e))? {
        out.push(c);
        return Ok(());
    }
    match &**r {
        "lt" => out.push('<'),
        "gt" => out.push('>'),
        "amp" => out.push('&'),
        "quot" => out.push('"'),
        "apos" => out.push('\''),
        _ => {}
    }
    Ok(())
}

/// The value of the attribute whose local name is `local` (and whose
/// prefix presence matches `prefixed`), unescaped.
pub(crate) fn attr(
    e: &quick_xml::events::BytesStart<'_>,
    local: &str,
    prefixed: bool,
) -> Option<String> {
    for a in e.attributes().with_checks(false).flatten() {
        let key = a.key;
        if key.local_name().as_ref() == local && key.prefix().is_some() == prefixed {
            return a
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned());
        }
    }
    None
}
