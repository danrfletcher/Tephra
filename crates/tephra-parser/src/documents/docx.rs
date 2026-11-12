//! docx: the paragraphs of `word/document.xml`.
//!
//! Word splits a paragraph's text into as many `w:r` runs as its edit
//! history left behind, so a link can arrive as `[[`, `@notes:Tephra`,
//! ` test]]` in three runs. Runs are joined per `w:p` before anything
//! is scanned for links.
//!
//! Only `w:t` text is visible text. Hyperlink targets (`w:hyperlink`'s
//! `r:id`, `HYPERLINK` field instructions in `w:instrText`) are never
//! read, and deleted text (`w:delText`) is not text. A `mc:Fallback`
//! branch is skipped so a text box is not read twice.

use quick_xml::events::Event;
use std::collections::HashMap;

use super::xml::{self, attr, corrupt, xml_err};
use super::{DocHeading, DocLocation, DocParagraph, DocumentError, Extracted};

const DOCUMENT_PART: &str = "word/document.xml";
const STYLES_PART: &str = "word/styles.xml";

pub(crate) fn extract(bytes: &[u8]) -> Result<Extracted, DocumentError> {
    let mut archive = xml::open_archive(bytes)?;
    let doc = xml::require_part(&mut archive, DOCUMENT_PART)?;
    // Style names are only a fallback for a heading whose style ID is
    // not `Heading1`..; a missing or broken styles part never fails the
    // document.
    let style_levels = match xml::read_part(&mut archive, STYLES_PART) {
        Ok(Some(styles)) => style_levels(&styles),
        _ => HashMap::new(),
    };

    let mut headings = Vec::new();
    let mut paragraphs = Vec::new();

    struct Open {
        number: usize,
        style: Option<String>,
        text: String,
    }
    let mut stack: Vec<Open> = Vec::new();
    let mut counter = 0usize;
    let mut depth = 0usize;
    let mut in_t = false;

    let mut r = xml::reader(&doc);
    loop {
        let ev = r.read_event().map_err(|e| xml_err(DOCUMENT_PART, e))?;
        let (e, empty) = match ev {
            Event::Eof => break,
            Event::Start(e) => (Some(e), false),
            Event::Empty(e) => (Some(e), true),
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                match e.local_name().as_ref() {
                    "t" => in_t = false,
                    "p" => {
                        if let Some(done) = stack.pop() {
                            let text = done.text.trim().to_string();
                            if !text.is_empty() {
                                let loc = DocLocation::Paragraph(done.number);
                                if let Some(level) = done
                                    .style
                                    .as_deref()
                                    .and_then(|s| heading_level(s, &style_levels))
                                {
                                    headings.push(DocHeading {
                                        level,
                                        text: text.clone(),
                                        location: loc,
                                    });
                                }
                                paragraphs.push(DocParagraph {
                                    location: loc,
                                    index: done.number,
                                    text,
                                });
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            Event::Text(t) => {
                if in_t {
                    if let Some(top) = stack.last_mut() {
                        xml::push_text(&mut top.text, &t, DOCUMENT_PART)?;
                    }
                }
                continue;
            }
            Event::GeneralRef(rf) => {
                if in_t {
                    if let Some(top) = stack.last_mut() {
                        xml::push_ref(&mut top.text, &rf, DOCUMENT_PART)?;
                    }
                }
                continue;
            }
            Event::CData(c) => {
                if in_t {
                    if let Some(top) = stack.last_mut() {
                        top.text.push_str(&c.into_inner());
                    }
                }
                continue;
            }
            _ => continue,
        };
        let Some(e) = e else { continue };
        if e.local_name().as_ref() == "Fallback" {
            if !empty {
                r.read_to_end(e.name())
                    .map_err(|err| xml_err(DOCUMENT_PART, err))?;
            }
            continue;
        }
        if !empty {
            depth += 1;
        }
        match e.local_name().as_ref() {
            "p" => {
                counter += 1;
                if empty {
                    continue;
                }
                stack.push(Open {
                    number: counter,
                    style: None,
                    text: String::new(),
                });
            }
            "pStyle" => {
                if let (Some(top), Some(v)) = (stack.last_mut(), attr(&e, "val", true)) {
                    top.style = Some(v);
                }
            }
            "t" => in_t = !empty,
            "tab" => {
                if let Some(top) = stack.last_mut() {
                    top.text.push('\t');
                }
            }
            "br" | "cr" => {
                if let Some(top) = stack.last_mut() {
                    top.text.push('\n');
                }
            }
            "noBreakHyphen" => {
                if let Some(top) = stack.last_mut() {
                    top.text.push('-');
                }
            }
            _ => {}
        }
    }
    if depth != 0 || !stack.is_empty() {
        return Err(corrupt(format!(
            "malformed XML in {DOCUMENT_PART}: ends inside an open element"
        )));
    }
    Ok(Extracted {
        headings,
        paragraphs,
        diagnostics: Vec::new(),
    })
}

/// `Title` is level 1, `Heading1`..`Heading6` their number, by style ID
/// (any case) or, failing that, by the style's name in `styles.xml`
/// (`heading 2`, `Title`).
fn heading_level(style_id: &str, by_name: &HashMap<String, u8>) -> Option<u8> {
    level_of(style_id).or_else(|| by_name.get(style_id).copied())
}

fn level_of(name: &str) -> Option<u8> {
    let lower = name.trim().to_ascii_lowercase();
    if lower == "title" {
        return Some(1);
    }
    let digits = lower.strip_prefix("heading")?.trim_start();
    match digits.parse::<u8>() {
        Ok(n @ 1..=6) => Some(n),
        _ => None,
    }
}

/// styleId -> heading level, for the styles `styles.xml` names as a
/// heading or title.
fn style_levels(styles: &[u8]) -> HashMap<String, u8> {
    let mut out = HashMap::new();
    let mut r = xml::reader(styles);
    let mut current: Option<String> = None;
    while let Ok(ev) = r.read_event() {
        match ev {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                "style" => current = attr(&e, "styleId", true),
                "name" => {
                    if let (Some(id), Some(name)) = (&current, attr(&e, "val", true)) {
                        if let Some(level) = level_of(&name) {
                            out.insert(id.clone(), level);
                        }
                    }
                }
                _ => {}
            },
            Event::End(e) if e.local_name().as_ref() == "style" => current = None,
            _ => {}
        }
    }
    out
}
