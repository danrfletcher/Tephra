# S4: Docs API range-targeted edit, paragraph marker, 7-day token

Run date: 2026-11-19 (offline parts only). Tools: python3 3.9.6 with urllib, git 2.50.1
(Apple Git-155). Raw numbers: `numbers/live.txt` (`s4.*`).

## Question

Can Tephra change one paragraph of a native Google Doc through
`documents.batchUpdate` on an index range alone, guarded by
`writeControl.requiredRevisionId`, without disturbing the formatting around it?
Does a stale revision id write nothing? Does a ` ^abc` marker at a paragraph end
inherit a link or bold? Is global `replaceAllText` usable? Does a refresh token
minted for an In-production client still work after 168 hours?

## Method

`spikes/s4/run.py run` (rerun command; Dan runs it after signing in with
`spikes/s4/mint-token.py`). It creates a scratch Doc (`documents.create`) with a
heading, bold, italic, a link, a bold linked phrase, a list, a table and
code-styled text, reads it with `documents.get`, finds one paragraph by its
`startIndex` and `endIndex`, then sends:

    documents:batchUpdate  [deleteContentRange, insertText] on that range only,
                           writeControl.requiredRevisionId = revision read before
    documents:batchUpdate  insertText at index 1 with the OLD revision id (must write nothing)
    documents:batchUpdate  insertText " ^abc" at the end of two paragraphs that end in
                           bold linked text, once plain and once with updateTextStyle
                           clearing bold and link
    documents:batchUpdate  replaceAllText (recorded on the scratch Doc only)

and compares a snapshot of every paragraph's runs and styles before and after.
`python3 spikes/s4/run.py recheck` after at least 168 hours uses the stored
token again. The token is read only from the Keychain item
`tephra-spike-google` (`security ... -w`) and never printed or written to a file.
`python3 spikes/s4/run.py selftest` checks the snapshot, locate and compare
functions offline.

## Measurements

not run: no credential. The Keychain item `tephra-spike-google` was not found
(`security find-generic-password -s tephra-spike-google` exit 44), so no Doc was
created, no request reached Google, and no formatting, stale-revision, marker,
`replaceAllText` or token-age number exists. The offline `selftest` passes (it
checks the helper functions against a hand-made Doc, not Google's behaviour).

## Verdict

Verdict: no-go

This is not a measured no-go: it is the verdict for something not run
(not run: no credential), and an unrun spike is never `go`. It stays `no-go` until
Dan signs in and runs `spikes/run-live.sh`, opens the scratch Doc to check bold,
link and heading look unchanged, and the 168-hour recheck is run.

## What changes in the plan

PR-16 (Docs link fixes and paragraph block IDs) stays gated: with this verdict
there is no code for it. Per Dan's Q7 decision, a real no-go comes back to him.
Once the live run exists, this file is rewritten with its numbers.
