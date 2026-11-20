# S4: Docs API range-targeted edit and a 7-day token

Question, method and results: `docs/spikes/S4-docs-api-and-token.md`.

Dan runs these; nothing here runs under `cargo test`.

1. `python3 spikes/s4/mint-token.py --client-json <client json> --client-is-in-production`
   signs in (loopback and PKCE, 5 minutes at most) and stores the token and its
   issue time in the Keychain item `tephra-spike-google`.
2. `python3 spikes/s4/run.py run` builds a scratch Doc (bold, italic, a link, a
   heading, a list, a table, code-styled text), edits one paragraph by index
   range under `requiredRevisionId`, checks a stale revision writes nothing,
   inserts the ` ^abc` marker, and records global `replaceAllText` as unusable.
   The scratch Doc is left in Drive for you to open.
3. After at least 168 hours: `python3 spikes/s4/run.py recheck`.

`python3 spikes/s4/run.py selftest` checks the helper functions offline.
