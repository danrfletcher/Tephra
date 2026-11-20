#!/usr/bin/env python3
"""S4: can Tephra edit one paragraph of a native Google Doc through the Docs
API without disturbing formatting, and does a long-lived token survive 7 days?

  python3 spikes/s4/run.py run        live: scratch Doc + range-targeted edit (Dan runs this)
  python3 spikes/s4/run.py recheck    live: use the stored token again after >= 168 hours
  python3 spikes/s4/run.py selftest   offline: check the helper functions on a hand-made Doc

`run` and `recheck` read the Keychain item `tephra-spike-google` (written by
mint-token.py). With no item they print `not run: no credential` and exit 0;
with no network (no answer from Google) `not run: no network` and exit 0.
Neither ever prints a token. Only a scratch Doc created by this script is
written to; it is left in Dan's Drive for him to open and inspect.
"""
import copy
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "lib"))
import gapi  # noqa: E402

DOCS = "https://docs.googleapis.com/v1/documents"
LINK = "https://example.com/plan"


# ---------------------------------------------------------------- helpers

def text_style_view(style):
    """The parts of a text style the spike compares."""
    style = style or {}
    return {
        "bold": bool(style.get("bold")),
        "italic": bool(style.get("italic")),
        "link": (style.get("link") or {}).get("url"),
        "font": (style.get("weightedFontFamily") or {}).get("fontFamily"),
    }


def snapshot(doc):
    """One entry per structural element: paragraphs as their runs and styles,
    tables as their cell text. Positions are left out on purpose, so a
    snapshot taken before an edit can be compared with one taken after."""
    out = []
    for el in doc.get("body", {}).get("content", []):
        if "paragraph" in el:
            p = el["paragraph"]
            runs = [
                (r["textRun"]["content"], text_style_view(r["textRun"].get("textStyle")))
                for r in p.get("elements", [])
                if "textRun" in r
            ]
            out.append(
                {
                    "kind": "paragraph",
                    "named_style": p.get("paragraphStyle", {}).get("namedStyleType"),
                    "bullet": "bullet" in p,
                    "runs": runs,
                    "text": "".join(c for c, _ in runs),
                }
            )
        elif "table" in el:
            t = el["table"]
            cells = []
            for row in t.get("tableRows", []):
                for cell in row.get("tableCells", []):
                    cells.append(
                        "".join(
                            r["textRun"]["content"]
                            for c in cell.get("content", [])
                            for r in c.get("paragraph", {}).get("elements", [])
                            if "textRun" in r
                        )
                    )
            out.append({"kind": "table", "rows": t.get("rows"), "columns": t.get("columns"), "cells": cells})
    return out


def locate_paragraph(doc, prefix):
    """(startIndex, endIndex, text) of the one paragraph whose text starts with
    `prefix`; raises when there is no match or more than one."""
    found = []
    for el in doc.get("body", {}).get("content", []):
        p = el.get("paragraph")
        if not p:
            continue
        text = "".join(r["textRun"]["content"] for r in p.get("elements", []) if "textRun" in r)
        if text.startswith(prefix):
            found.append((el["startIndex"], el["endIndex"], text))
    if len(found) != 1:
        raise LookupError(f"{len(found)} paragraphs start with {prefix!r}")
    return found[0]


def paragraph_diffs(before, after, skip_prefixes=()):
    """Entries that differ between two snapshots, ignoring paragraphs whose text
    starts with one of `skip_prefixes` (the ones an edit meant to change)."""
    def keep(entries):
        return [
            e for e in entries
            if not (e["kind"] == "paragraph" and e["text"].startswith(tuple(skip_prefixes)))
        ]
    b, a = keep(before), keep(after)
    diffs = []
    for i in range(max(len(a), len(b))):
        x = b[i] if i < len(b) else None
        y = a[i] if i < len(a) else None
        if x != y:
            diffs.append((x, y))
    return diffs


def run_styles_for(doc, needle):
    """Style views of every run whose text contains `needle`."""
    views = []
    for e in snapshot(doc):
        if e["kind"] == "paragraph":
            for content, style in e["runs"]:
                if needle in content:
                    views.append(style)
    return views


def plain_text(doc):
    return "".join(e["text"] if e["kind"] == "paragraph" else "|".join(e["cells"]) for e in snapshot(doc))


# ---------------------------------------------------------------- selftest

def selftest():
    def run(content, **style):
        return {"textRun": {"content": content, "textStyle": style}}

    def para(start, runs):
        end = start + sum(len(r["textRun"]["content"]) for r in runs)
        return {"startIndex": start, "endIndex": end, "paragraph": {"elements": runs, "paragraphStyle": {"namedStyleType": "NORMAL_TEXT"}}}

    doc = {"body": {"content": [
        para(1, [run("EDIT one "), run("the plan\n", bold=True, link={"url": LINK})]),
        para(19, [run("Other paragraph\n")]),
    ]}}
    assert locate_paragraph(doc, "EDIT") == (1, 19, "EDIT one the plan\n"), locate_paragraph(doc, "EDIT")
    try:
        locate_paragraph(doc, "nothing")
        raise AssertionError("expected LookupError")
    except LookupError:
        pass
    before = snapshot(doc)
    assert paragraph_diffs(before, snapshot(doc)) == []
    changed = copy.deepcopy(doc)
    changed["body"]["content"][1]["paragraph"]["elements"][0]["textRun"]["textStyle"] = {"bold": True}
    assert len(paragraph_diffs(before, snapshot(changed))) == 1
    assert paragraph_diffs(before, snapshot(changed), skip_prefixes=("Other",)) == []
    styles = run_styles_for(doc, "the plan")
    assert styles == [{"bold": True, "italic": False, "link": LINK, "font": None}], styles
    print("selftest ok")
    return 0


# ---------------------------------------------------------------- live run

def build_document_requests():
    """Insert the test body at index 1, then style it. Returns (requests,
    paragraph prefixes). All offsets are for ASCII text (1 char = 1 index)."""
    paras = [
        ("Spike heading", [], "HEADING_1"),
        ("Plain with bold, italic and a link inside.", [("bold", "bold"), ("italic", "italic"), ("link", "link")], None),
        ("EDIT paragraph: swap the old text here, see the plan", [("linkbold", "the plan")], None),
        ("MARKA paragraph ends in a linked bold plan", [("linkbold", "linked bold plan")], None),
        ("MARKB paragraph ends in a linked bold plan", [("linkbold", "linked bold plan")], None),
        ("List item one", [], "BULLET"),
        ("List item two", [], "BULLET"),
        ("Run cargo test for code style.", [("code", "cargo test")], None),
        ("Plan.md is mentioned here and Plan.md again here.", [], None),
    ]
    text = "".join(p[0] + "\n" for p in paras)
    reqs = [{"insertText": {"location": {"index": 1}, "text": text}}]
    pos = 1
    bullet_start = bullet_end = None
    for body, spans, kind in paras:
        for what, needle in spans:
            s = pos + body.index(needle)
            rng = {"startIndex": s, "endIndex": s + len(needle)}
            if what == "bold":
                reqs.append({"updateTextStyle": {"range": rng, "textStyle": {"bold": True}, "fields": "bold"}})
            elif what == "italic":
                reqs.append({"updateTextStyle": {"range": rng, "textStyle": {"italic": True}, "fields": "italic"}})
            elif what == "link":
                reqs.append({"updateTextStyle": {"range": rng, "textStyle": {"link": {"url": LINK}}, "fields": "link"}})
            elif what == "linkbold":
                reqs.append({"updateTextStyle": {"range": rng, "textStyle": {"bold": True, "link": {"url": LINK}}, "fields": "bold,link"}})
            elif what == "code":
                reqs.append({"updateTextStyle": {"range": rng, "textStyle": {"weightedFontFamily": {"fontFamily": "Courier New"}}, "fields": "weightedFontFamily"}})
        rng = {"startIndex": pos, "endIndex": pos + len(body) + 1}
        if kind and kind.startswith("HEADING"):
            reqs.append({"updateParagraphStyle": {"range": rng, "paragraphStyle": {"namedStyleType": kind}, "fields": "namedStyleType"}})
        if kind == "BULLET":
            bullet_start = bullet_start or pos
            bullet_end = pos + len(body) + 1
        pos += len(body) + 1
    reqs.append({"createParagraphBullets": {"range": {"startIndex": bullet_start, "endIndex": bullet_end}, "bulletPreset": "BULLET_DISC_CIRCLE_SQUARE"}})
    return reqs


def get_doc(api, doc_id, label="documents.get"):
    status, doc = api.call("GET", f"{DOCS}/{doc_id}", label)
    if status != 200:
        raise RuntimeError(f"documents.get returned HTTP {status}")
    return doc


def batch(api, doc_id, requests, revision=None, label="documents.batchUpdate"):
    body = {"requests": requests}
    if revision:
        body["writeControl"] = {"requiredRevisionId": revision}
    return api.call("POST", f"{DOCS}/{doc_id}:batchUpdate", label, body=body)


def live_run():
    n = gapi.Numbers("s4")
    cred = gapi.load_credential()
    if cred is None:
        print("not run: no credential (Keychain item tephra-spike-google is missing; run spikes/s4/mint-token.py)")
        n.put("status", "not run: no credential")
        return 0
    try:
        api = gapi.Api(gapi.access_token(cred))
    except gapi.TokenError as err:
        n.put("status", f"not run: token refused ({err.code})")
        return 0

    n.put("token_issued_at_epoch", cred.get("issued_at", "unknown"))
    title = "tephra-spike-S4 " + time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime())
    status, created = api.call("POST", DOCS, "documents.create", body={"title": title})
    if status != 200:
        n.put("status", f"not run: documents.create returned HTTP {status}")
        return 0
    doc_id = created["documentId"]
    n.put("scratch_doc_url", f"https://docs.google.com/document/d/{doc_id}/edit")

    status, resp = batch(api, doc_id, build_document_requests())
    n.put("build_body_status", status)
    # a table at the end of the body
    status, resp = batch(api, doc_id, [{"insertTable": {"rows": 2, "columns": 2, "endOfSegmentLocation": {"segmentId": ""}}}])
    n.put("insert_table_status", status)

    doc = get_doc(api, doc_id)
    before = snapshot(doc)
    n.put("snapshot_elements_before", len(before))
    n.put("revision_id_present", "yes" if doc.get("revisionId") else "no")
    start, end, text = locate_paragraph(doc, "EDIT paragraph")
    n.put("located_paragraph_range", f"{start}-{end}")

    # Range-targeted edit of one phrase inside that paragraph.
    old, new = "old text", "new words"
    s = start + text.index(old)
    revision = doc["revisionId"]
    status, _ = batch(
        api, doc_id,
        [
            {"deleteContentRange": {"range": {"startIndex": s, "endIndex": s + len(old)}}},
            {"insertText": {"location": {"index": s}, "text": new}},
        ],
        revision=revision,
    )
    n.put("range_edit_status", status)
    after_doc = get_doc(api, doc_id)
    after = snapshot(after_doc)
    expect = text.replace(old, new)
    n.put("range_edit_text_as_expected", "yes" if locate_paragraph(after_doc, "EDIT paragraph")[2] == expect else "no")
    n.put("formatting_diffs_outside_edited_paragraph", len(paragraph_diffs(before, after, ("EDIT paragraph",))))
    n.put("link_bold_run_after_edit", run_styles_for(after_doc, "the plan"))
    n.put("inserted_text_style", run_styles_for(after_doc, "new words"))

    # A stale revision id must write nothing.
    text_before_stale = plain_text(after_doc)
    status, resp = batch(api, doc_id, [{"insertText": {"location": {"index": 1}, "text": "STALE"}}], revision=revision)
    n.put("stale_revision_status", status)
    n.put("stale_revision_error", (resp.get("error", {}) or {}).get("status", "none"))
    n.put("stale_revision_wrote_nothing", "yes" if plain_text(get_doc(api, doc_id)) == text_before_stale else "no")

    # The ^abc marker at a paragraph end, with and without clearing the style.
    marker = " ^abc"
    for prefix, clear in (("MARKA", False), ("MARKB", True)):
        cur = get_doc(api, doc_id)
        ps, pe, _ = locate_paragraph(cur, prefix)
        at = pe - 1  # before the paragraph's newline
        reqs = [{"insertText": {"location": {"index": at}, "text": marker}}]
        if clear:
            reqs.append({"updateTextStyle": {"range": {"startIndex": at, "endIndex": at + len(marker)}, "textStyle": {}, "fields": "bold,link"}})
        status, _ = batch(api, doc_id, reqs, revision=cur["revisionId"])
        styles = run_styles_for(get_doc(api, doc_id), "^abc")
        which = "with_explicit_clear" if clear else "plain_insert"
        n.put(f"marker_{which}.status", status)
        n.put(f"marker_{which}.marker_run_styles", styles)
        n.put(f"marker_{which}.marker_inherits_link", "yes" if any(s["link"] for s in styles) else "no")
        n.put(f"marker_{which}.marker_inherits_bold", "yes" if any(s["bold"] for s in styles) else "no")

    # Global replace is recorded, on the scratch Doc only.
    status, resp = batch(api, doc_id, [{"replaceAllText": {"containsText": {"text": "Plan.md", "matchCase": True}, "replaceText": "Roadmap.md"}}])
    changed = (resp.get("replies", [{}])[0].get("replaceAllText", {}) or {}).get("occurrencesChanged", "unknown")
    n.put("replace_all_text.status", status)
    n.put("replace_all_text.occurrences_changed", changed)

    for label, s in sorted(api.stats.items()):
        n.put(f"requests.{label}", s["requests"])
    print("  scratch Doc left in your Drive for you to open: bold, link and heading should look unchanged.")
    return 0


def live_recheck():
    n = gapi.Numbers("s4")
    cred = gapi.load_credential()
    if cred is None:
        print("not run: no credential (Keychain item tephra-spike-google is missing)")
        n.put("recheck.status", "not run: no credential")
        return 0
    issued = cred.get("issued_at")
    if not isinstance(issued, int):
        n.put("recheck.status", "not run: the item has no issue time")
        return 0
    age_hours = (time.time() - issued) / 3600
    n.put("recheck.token_age_hours", f"{age_hours:.1f}")
    if age_hours < 168:
        n.put("recheck.status", f"not run: token is {age_hours:.1f} hours old, needs at least 168")
        return 0
    try:
        gapi.access_token(cred)
        n.put("recheck.result", "token still accepted after >= 168 hours")
    except gapi.TokenError as err:
        n.put("recheck.result", f"token refused after >= 168 hours: {err.code}")
    return 0


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    # No network is `not run`, never a traceback: Numbers lines already written
    # stay, the status line says where the run stopped being possible.
    if mode == "run":
        try:
            sys.exit(live_run())
        except gapi.NetworkError as err:
            gapi.not_run_no_network(gapi.Numbers("s4"), err)
            sys.exit(0)
    if mode == "recheck":
        try:
            sys.exit(live_recheck())
        except gapi.NetworkError as err:
            gapi.not_run_no_network(gapi.Numbers("s4"), err, "recheck.status")
            sys.exit(0)
    if mode == "selftest":
        sys.exit(selftest())
    print(__doc__)
    sys.exit(2)
