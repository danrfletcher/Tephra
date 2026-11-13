//! pptx: the text of every slide, in `ppt/presentation.xml`'s
//! `sldIdLst` order (not zip entry order), numbered from 1.
//!
//! A shape whose placeholder (`p:ph`) is `title` or `ctrTitle` gives
//! the slide's title: a level-1 heading at that slide. Text in body
//! boxes, tables and the title is read. Speaker notes
//! (`ppt/notesSlides/`) are never opened, nor is anything under
//! `ppt/media/`.

use std::collections::HashMap;

use quick_xml::events::Event;

use super::xml::{self, attr, corrupt, xml_err};
use super::{DocHeading, DocLocation, DocParagraph, DocumentError, Extracted};

const PRESENTATION: &str = "ppt/presentation.xml";
const PRESENTATION_RELS: &str = "ppt/_rels/presentation.xml.rels";

pub(crate) fn extract(bytes: &[u8]) -> Result<Extracted, DocumentError> {
    let mut archive = xml::open_archive(bytes)?;
    let presentation = xml::require_part(&mut archive, PRESENTATION)?;
    let rels = xml::require_part(&mut archive, PRESENTATION_RELS)?;

    let targets = relationship_targets(&rels)?;
    let slide_parts: Vec<String> = slide_rids(&presentation)?
        .into_iter()
        .map(|rid| {
            targets
                .get(&rid)
                .map(|t| resolve_target(t))
                .ok_or_else(|| corrupt(format!("slide relationship {rid} has no target")))
        })
        .collect::<Result<_, _>>()?;

    let mut headings = Vec::new();
    let mut paragraphs = Vec::new();
    for (i, part) in slide_parts.iter().enumerate() {
        let slide = i + 1;
        let xml = xml::require_part(&mut archive, part)?;
        read_slide(part, &xml, slide, &mut headings, &mut paragraphs)?;
    }
    Ok(Extracted {
        headings,
        paragraphs,
        diagnostics: Vec::new(),
    })
}

/// relationship Id -> Target of `ppt/_rels/presentation.xml.rels`.
fn relationship_targets(rels: &[u8]) -> Result<HashMap<String, String>, DocumentError> {
    let mut out = HashMap::new();
    let mut r = xml::reader(rels);
    loop {
        match r.read_event().map_err(|e| xml_err(PRESENTATION_RELS, e))? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "Relationship" => {
                if let (Some(id), Some(target)) = (attr(&e, "Id", false), attr(&e, "Target", false))
                {
                    out.insert(id, target);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The `r:id` of every `p:sldId`, in list order.
fn slide_rids(presentation: &[u8]) -> Result<Vec<String>, DocumentError> {
    let mut out = Vec::new();
    let mut r = xml::reader(presentation);
    let mut depth = 0usize;
    loop {
        match r.read_event().map_err(|e| xml_err(PRESENTATION, e))? {
            Event::Eof => break,
            Event::Start(e) => {
                depth += 1;
                if e.local_name().as_ref() == "sldId" {
                    out.extend(attr(&e, "id", true));
                }
            }
            Event::Empty(e) => {
                if e.local_name().as_ref() == "sldId" {
                    out.extend(attr(&e, "id", true));
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if depth != 0 {
        return Err(corrupt(format!(
            "malformed XML in {PRESENTATION}: ends inside an open element"
        )));
    }
    Ok(out)
}

/// A relationship target as a zip part name: relative to `ppt/`, or
/// absolute from the package root.
fn resolve_target(target: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let base: &str = match target.strip_prefix('/') {
        Some(rest) => rest,
        None => {
            parts.push("ppt");
            target
        }
    };
    for seg in base.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn read_slide(
    part: &str,
    xml: &[u8],
    slide: usize,
    headings: &mut Vec<DocHeading>,
    paragraphs: &mut Vec<DocParagraph>,
) -> Result<(), DocumentError> {
    struct Shape {
        title: bool,
        texts: Vec<String>,
    }
    let mut shapes: Vec<Shape> = Vec::new();
    // Open `a:p` paragraphs (index, text so far); a table cell or text
    // box can nest.
    let mut open: Vec<(usize, String)> = Vec::new();
    let mut index = 0usize;
    let mut depth = 0usize;
    let mut in_t = false;

    let mut r = xml::reader(xml);
    loop {
        let ev = r.read_event().map_err(|e| xml_err(part, e))?;
        let (e, empty) = match ev {
            Event::Eof => break,
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                match e.local_name().as_ref() {
                    "t" => in_t = false,
                    "p" => {
                        if let Some((number, text)) = open.pop() {
                            let text = text.trim().to_string();
                            if !text.is_empty() {
                                if let Some(shape) = shapes.last_mut() {
                                    shape.texts.push(text.clone());
                                }
                                paragraphs.push(DocParagraph {
                                    location: DocLocation::Slide(slide),
                                    index: number,
                                    text,
                                });
                            }
                        }
                    }
                    "sp" => {
                        if let Some(shape) = shapes.pop() {
                            let title = shape.texts.join(" ");
                            if shape.title && !title.is_empty() {
                                headings.push(DocHeading {
                                    level: 1,
                                    text: title,
                                    location: DocLocation::Slide(slide),
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
                    if let Some((_, top)) = open.last_mut() {
                        xml::push_text(top, &t, part)?;
                    }
                }
                continue;
            }
            Event::GeneralRef(rf) => {
                if in_t {
                    if let Some((_, top)) = open.last_mut() {
                        xml::push_ref(top, &rf, part)?;
                    }
                }
                continue;
            }
            Event::CData(c) => {
                if in_t {
                    if let Some((_, top)) = open.last_mut() {
                        top.push_str(&c.into_inner());
                    }
                }
                continue;
            }
            _ => continue,
        };
        if e.local_name().as_ref() == "Fallback" {
            if !empty {
                r.read_to_end(e.name()).map_err(|err| xml_err(part, err))?;
            }
            continue;
        }
        if !empty {
            depth += 1;
        }
        match e.local_name().as_ref() {
            "sp" => {
                if !empty {
                    shapes.push(Shape {
                        title: false,
                        texts: Vec::new(),
                    });
                }
            }
            "ph" => {
                if let (Some(shape), Some(kind)) = (shapes.last_mut(), attr(&e, "type", false)) {
                    if kind == "title" || kind == "ctrTitle" {
                        shape.title = true;
                    }
                }
            }
            "p" => {
                index += 1;
                if !empty {
                    open.push((index, String::new()));
                }
            }
            "t" => in_t = !empty,
            "br" => {
                if let Some((_, top)) = open.last_mut() {
                    top.push('\n');
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(corrupt(format!(
            "malformed XML in {part}: ends inside an open element"
        )));
    }
    Ok(())
}
