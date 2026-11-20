#!/usr/bin/env python3
"""S5: does the Drive changes feed give Tephra renames by file id, and what
does polling it cost?

  python3 spikes/s5/run.py run        create scratch files, ask Dan to change four of them in the Drive UI, read the feed
  python3 spikes/s5/run.py setup      only the first half of `run`
  python3 spikes/s5/run.py list       only the second half (after Dan's changes)
  python3 spikes/s5/run.py resume     read the feed from the saved page token in a fresh process (no listing)
  python3 spikes/s5/run.py selftest   offline: check the classifier on a hand-made feed

This script only ever creates scratch files in a folder of its own. The
rename, move, bin and permanent delete are done by Dan in the Drive web UI,
because the spike scripts must never send any of those calls themselves.
Scratch files are left in Dan's Drive.

Credential: Keychain item `tephra-spike-google` (see spikes/s4/mint-token.py).
With no item the script prints `not run: no credential` and exits 0; with no
network (no answer from Google) `not run: no network` and exit 0.
"""
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "lib"))
import gapi  # noqa: E402

DRIVE = "https://www.googleapis.com/drive/v3"
FOLDER = "application/vnd.google-apps.folder"
SHORTCUT = "application/vnd.google-apps.shortcut"
POLL_WAIT = int(os.environ.get("TEPHRA_SPIKE_POLL_WAIT", "60"))


def state_path():
    return os.path.join(gapi.numbers_path()[1], "s5-state.json")


def classify(original, entry):
    """Describe one feed entry for a scratch file, given the file as it was
    created (`original`, a Drive file dict). Pure function; see selftest."""
    if entry.get("removed"):
        return "removed from the feed (no file body in the entry)"
    f = entry.get("file") or {}
    notes = []
    if f.get("name") != original.get("name"):
        notes.append(f"name {original.get('name')!r} -> {f.get('name')!r}, same fileId")
    if sorted(f.get("parents") or []) != sorted(original.get("parents") or []):
        notes.append("parents changed")
    if not notes:
        differing = sorted(
            k for k in f
            if k in original and f[k] != original[k] and k not in ("modifiedTime", "version", "viewedByMeTime")
        )
        notes.append("file entry present, name and parents unchanged; keys that differ from creation: " + ", ".join(differing))
    return "; ".join(notes)


def selftest():
    orig = {"id": "a", "name": "one.txt", "parents": ["p"], "flag": False}
    assert "removed" in classify(orig, {"fileId": "a", "removed": True})
    got = classify(orig, {"fileId": "a", "file": {"id": "a", "name": "two.txt", "parents": ["p"], "flag": False}})
    assert got == "name 'one.txt' -> 'two.txt', same fileId", got
    got = classify(orig, {"fileId": "a", "file": {"id": "a", "name": "one.txt", "parents": ["q"], "flag": False}})
    assert got == "parents changed", got
    got = classify(orig, {"fileId": "a", "file": {"id": "a", "name": "one.txt", "parents": ["p"], "flag": True}})
    assert got.endswith("flag"), got
    print("selftest ok")
    return 0


def connect(n):
    cred = gapi.load_credential()
    if cred is None:
        print("not run: no credential (Keychain item tephra-spike-google is missing; run spikes/s4/mint-token.py)")
        n.put("status", "not run: no credential")
        return None
    try:
        return gapi.Api(gapi.access_token(cred))
    except gapi.TokenError as err:
        n.put("status", f"not run: token refused ({err.code})")
        return None


def create(api, name, parents=None, mime=None, extra=None):
    body = {"name": name}
    if parents:
        body["parents"] = parents
    if mime:
        body["mimeType"] = mime
    if extra:
        body.update(extra)
    status, resp = api.call("POST", f"{DRIVE}/files", "files.create", body=body, params={"fields": "*"})
    if status != 200:
        raise RuntimeError(f"files.create returned HTTP {status}")
    return resp


def setup(n):
    """Create the scratch files and take the page token. True when it did."""
    api = connect(n)
    if api is None:
        return False
    stamp = time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime())
    root = create(api, f"tephra-spike-S5 {stamp}", mime=FOLDER)
    other = create(api, "other", parents=[root["id"]], mime=FOLDER)
    files = {}
    for name in ("rename-me.txt", "move-me.txt", "bin-me.txt", "delete-me.txt", "same-name.txt"):
        files[name] = create(api, name, parents=[root["id"]], mime="text/plain")
    # a second file with the very same name: Drive allows it
    files["same-name-2"] = create(api, "same-name.txt", parents=[root["id"]], mime="text/plain")
    files["shortcut"] = create(
        api, "shortcut-to-rename-me", parents=[root["id"]], mime=SHORTCUT,
        extra={"shortcutDetails": {"targetId": files["rename-me.txt"]["id"]}},
    )
    n.put("created_files", len(files) + 2)
    n.put("same_name_ids_differ", "yes" if files["same-name.txt"]["id"] != files["same-name-2"]["id"] else "no")
    n.put("shortcut_mime_type", files["shortcut"].get("mimeType"))
    n.put("shortcut_target_id_recorded", "yes" if files["shortcut"].get("shortcutDetails", {}).get("targetId") else "no")
    status, tok = api.call("GET", f"{DRIVE}/changes/startPageToken", "changes.getStartPageToken")
    n.put("start_page_token_status", status)
    with open(state_path(), "w") as fh:
        json.dump(
            {
                "page_token": tok["startPageToken"],
                "root_id": root["id"],
                "folder_ids": [root["id"], other["id"]],
                "files": files,
            },
            fh,
        )
    print()
    print("Now, in the Drive web UI, in the folder " + repr(root["name"]) + ":")
    print("  1. rename 'rename-me.txt' to 'renamed.txt'")
    print("  2. move 'move-me.txt' into the folder 'other'")
    print("  3. send 'bin-me.txt' to the bin")
    print("  4. delete 'delete-me.txt' for good (bin, then 'Delete forever')")
    print("Then run: python3 spikes/s5/run.py list")
    return True


def read_feed(api, token, label, fields):
    """All pages of the feed from `token`. Returns (changes, new_start_token)."""
    changes, new_token = [], None
    while token:
        status, resp = api.call(
            "GET", f"{DRIVE}/changes", label,
            params={"pageToken": token, "pageSize": 100, "includeRemoved": "true", "fields": fields},
        )
        if status != 200:
            raise RuntimeError(f"changes.list returned HTTP {status}")
        changes.extend(resp.get("changes", []))
        new_token = resp.get("newStartPageToken", new_token)
        token = resp.get("nextPageToken")
    return changes, new_token


def list_changes(n):
    api = connect(n)
    if api is None:
        return 0
    with open(state_path()) as fh:
        st = json.load(fh)
    files = st["files"]
    ids = {f["id"] for f in files.values()}
    changes, new_token = read_feed(
        api, st["page_token"], "changes.list",
        "nextPageToken,newStartPageToken,changes(changeType,fileId,removed,time,file(*))",
    )
    with open(os.path.join(gapi.numbers_path()[1], "s5-changes.json"), "w") as fh:
        json.dump(changes, fh, indent=1)
    n.put("feed.entries_total", len(changes))
    scratch_ids = ids | set(st["folder_ids"])
    mine = [c for c in changes if c.get("fileId") in scratch_ids]
    n.put("feed.entries_for_scratch_files", len(mine))
    n.put("feed.entries_for_other_files_in_the_account", len(changes) - len(mine))
    seen = {c["fileId"]: c for c in changes}
    for name, f in files.items():
        key = name.replace(".txt", "").replace("-", "_")
        entry = seen.get(f["id"])
        n.put(f"file.{key}.in_feed", "yes" if entry else "no")
        if entry:
            n.put(f"file.{key}.description", classify(f, entry))
    # Narrowing to one picked folder is done on our side: the feed has no folder filter.
    kept = [
        c for c in changes
        if c.get("fileId") in scratch_ids
        or set((c.get("file") or {}).get("parents") or []) & set(st["folder_ids"])
    ]
    n.put("feed.entries_kept_by_id_and_parent_filter", len(kept))
    n.put("feed.removed_entries_not_ours_or_unknown_parent", sum(1 for c in changes if c.get("removed") and c.get("fileId") not in scratch_ids))

    # Two files with one name: listed through files.list on the scratch folder.
    status, resp = api.call(
        "GET", f"{DRIVE}/files", "files.list",
        params={"q": f"'{st['root_id']}' in parents", "fields": "files(id,name,mimeType)", "pageSize": 100},
    )
    names = [f["name"] for f in resp.get("files", [])]
    n.put("listing.same_name_entries", names.count("same-name.txt"))

    st["page_token"] = new_token
    with open(state_path(), "w") as fh:
        json.dump(st, fh)
    n.put("page_token_saved_to_disk", "yes (spikes/out/s5-state.json holds the page token string only)")

    # One idle poll after POLL_WAIT seconds.
    time.sleep(POLL_WAIT)
    t0 = time.time()
    idle, _ = read_feed(api, new_token, "idle_poll", "nextPageToken,newStartPageToken,changes(fileId,removed)")
    s = api.stats["idle_poll"]
    n.put("idle_poll.wait_seconds", POLL_WAIT)
    n.put("idle_poll.requests", s["requests"])
    n.put("idle_poll.body_bytes", s["body_bytes"])
    n.put("idle_poll.entries", len(idle))
    n.put("idle_poll.seconds", f"{time.time() - t0:.2f}")
    n.put("idle_poll.quota_units", "not measured: Drive returns no per-request units")

    # Restart: a fresh process reads the feed from the saved token, no listing.
    proc = subprocess.run([sys.executable, os.path.abspath(__file__), "resume"], capture_output=True, text=True)
    for line in proc.stdout.splitlines():
        print(line)
    for label, st_ in sorted(api.stats.items()):
        n.put(f"requests.{label}", st_["requests"])
    return 0


def resume(n):
    api = connect(n)
    if api is None:
        return 0
    with open(state_path()) as fh:
        st = json.load(fh)
    changes, _ = read_feed(api, st["page_token"], "changes.list", "nextPageToken,newStartPageToken,changes(fileId,removed)")
    n.put("restart.requests", api.stats["changes.list"]["requests"])
    n.put("restart.used_files_list", "yes" if "files.list" in api.stats else "no")
    n.put("restart.entries", len(changes))
    return 0


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    if mode == "selftest":
        sys.exit(selftest())
    numbers = gapi.Numbers("s5")
    try:
        if mode == "setup":
            setup(numbers)
            sys.exit(0)
        if mode == "list":
            sys.exit(list_changes(numbers))
        if mode == "resume":
            sys.exit(resume(numbers))
        if mode == "run":
            if setup(numbers):
                if sys.stdin.isatty():
                    input("Press Enter when the four changes are done... ")
                    sys.exit(list_changes(numbers))
                print("(no terminal: run `python3 spikes/s5/run.py list` after the changes)")
            sys.exit(0)
    except gapi.NetworkError as err:
        # No network is `not run`, never a traceback; numbers already written stay.
        gapi.not_run_no_network(numbers, err)
        sys.exit(0)
    print(__doc__)
    sys.exit(2)
