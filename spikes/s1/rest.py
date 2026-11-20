#!/usr/bin/env python3
"""S1, REST half: the same listing and change detection as git-half.sh, but
through the GitHub REST API, with request counts, bytes and rate-limit units.

  python3 spikes/s1/rest.py --repo owner/name --base <sha> --head <sha> \
      --branch main --label s1.public [--unauthenticated] [--poll-wait 60]
  python3 spikes/s1/rest.py --selftest     offline check of the rate-limit path

The token, when there is one, is read from the Keychain item
`tephra-spike-github` with `security ... -w` and goes into an Authorization
header. It is never printed, written to a file or put on a command line.

Numbers are collected in memory and written to the numbers file only when the
whole run finished. A 403/429 from the rate limiter stops the run, writes the
status and reset time instead, and exits 0 (no partial numbers).
"""
import argparse
import datetime
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

API = "https://api.github.com"
TEXT_RE = re.compile(
    r"\.(md|markdown|txt|rst|rs|py|js|ts|tsx|jsx|json|ya?ml|toml|html|css|sh|c|h|cpp|go|java|swift|lua|cfg|ini|lock)$"
)


class RateLimited(Exception):
    def __init__(self, status, reset_epoch):
        super().__init__("rate limited")
        self.status = status
        self.reset_epoch = reset_epoch


def keychain_token(service="tephra-spike-github"):
    """The secret in the Keychain item, or None when it is missing."""
    if os.environ.get("TEPHRA_SPIKE_NO_KEYCHAIN") == "1":
        return None
    try:
        out = subprocess.run(
            ["security", "find-generic-password", "-s", service, "-w"],
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        return None
    if out.returncode != 0:
        return None
    # A token never contains whitespace; GNU base64 wraps long output, so
    # drop every whitespace character, not only the ends.
    return "".join(out.stdout.split()) or None


class Client:
    def __init__(self, token):
        self.token = token
        self.stats = {}  # category -> {"requests", "body_bytes", "header_bytes"}

    def get(self, path, category, extra_headers=None, counted=True):
        headers = {
            "Accept": "application/vnd.github+json",
            "User-Agent": "tephra-spike-s1",
            "X-GitHub-Api-Version": "2022-11-28",
        }
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        if extra_headers:
            headers.update(extra_headers)
        req = urllib.request.Request(API + path, headers=headers)
        try:
            resp = urllib.request.urlopen(req, timeout=60)
            status, hdrs, body = resp.status, resp.headers, resp.read()
        except urllib.error.HTTPError as err:
            status, hdrs, body = err.code, err.headers, err.read()
        header_bytes = sum(len(k) + len(v) + 4 for k, v in hdrs.items()) + 15
        if counted:
            s = self.stats.setdefault(
                category, {"requests": 0, "body_bytes": 0, "header_bytes": 0}
            )
            s["requests"] += 1
            s["body_bytes"] += len(body)
            s["header_bytes"] += header_bytes
        if status in (403, 429) and (
            hdrs.get("x-ratelimit-remaining") == "0"
            or b"rate limit" in body.lower()
        ):
            reset = hdrs.get("x-ratelimit-reset")
            raise RateLimited(status, int(reset) if reset else None)
        return status, hdrs, body


def core_remaining(client):
    """Remaining core units, read from /rate_limit (which costs nothing)."""
    _, _, body = client.get("/rate_limit", "rate_limit", counted=False)
    return json.loads(body)["resources"]["core"]["remaining"]


def run(args):
    token = None
    if not args.unauthenticated:
        token = keychain_token()
        if token is None:
            return {"status": "not run: no credential"}, {}
    client = Client(token)
    nums = {}
    renames = []
    repo = args.repo
    nums["auth"] = "token from Keychain" if token else "none (unauthenticated, 60 requests/hour)"

    units_start = core_remaining(client)

    # Listing: the whole tree in one request.
    status, _, body = client.get(
        f"/repos/{repo}/git/trees/{args.head}?recursive=1", "tree"
    )
    if status != 200:
        return {"status": f"not run: tree request returned HTTP {status}"}, {}
    tree = json.loads(body)
    entries = [e for e in tree["tree"] if e["type"] == "blob"]
    nums["tree_entries"] = len(tree["tree"])
    nums["tree_truncated"] = str(tree.get("truncated", False)).lower()
    text_files = [e for e in entries if TEXT_RE.search(e["path"])]
    nums["text_files"] = len(text_files)

    # Change detection with rename status.
    status, _, body = client.get(
        f"/repos/{repo}/compare/{args.base}...{args.head}?per_page=100", "compare"
    )
    if status != 200:
        return {"status": f"not run: compare request returned HTTP {status}"}, {}
    cmp = json.loads(body)
    files = cmp.get("files", [])
    nums["compare_files"] = len(files)
    nums["compare_total_commits"] = cmp.get("total_commits", "unknown")
    for f in files:
        if f["status"] == "renamed":
            renames.append(("renamed", f["previous_filename"], f["filename"], f.get("changes", 0)))
    nums["rename_count"] = len(renames)
    nums["rename_pure_found"] = sum(1 for r in renames if r[3] == 0)
    nums["rename_edited_found"] = sum(1 for r in renames if r[3] != 0)

    # Commits.
    status, _, body = client.get(
        f"/repos/{repo}/commits?sha={args.head}&per_page=30", "commits"
    )
    nums["commits_listed"] = len(json.loads(body)) if status == 200 else 0

    # First-index read through the API: one request per file. A sample is read
    # for the per-request size; the full count is the number of text files.
    sample = text_files[: args.blob_sample]
    for e in sample:
        client.get(
            f"/repos/{repo}/git/blobs/{e['sha']}",
            "blob_sample",
            {"Accept": "application/vnd.github.raw+json"},
        )
    nums["first_index_read_requests_needed"] = len(text_files)
    nums["first_index_read_sample_files"] = len(sample)

    # One idle conditional poll: an ETag request, a wait, then the same
    # request with If-None-Match. Units come from /rate_limit around the
    # second request and from the response headers.
    poll_path = f"/repos/{repo}/commits?sha={args.branch}&per_page=1"
    status, hdrs, _ = client.get(poll_path, "poll_first")
    etag = hdrs.get("etag")
    if status == 200 and etag:
        hdr_remaining_1 = hdrs.get("x-ratelimit-remaining")
        time.sleep(args.poll_wait)
        before = core_remaining(client)
        status2, hdrs2, _ = client.get(poll_path, "poll_conditional", {"If-None-Match": etag})
        after = core_remaining(client)
        nums["poll_wait_seconds"] = args.poll_wait
        nums["poll_conditional_status"] = status2
        nums["poll_conditional_units_by_rate_limit_endpoint"] = before - after
        remaining_2 = hdrs2.get("x-ratelimit-remaining")
        if hdr_remaining_1 and remaining_2:
            nums["poll_conditional_units_by_headers"] = int(hdr_remaining_1) - int(remaining_2)
    else:
        nums["poll_conditional_status"] = f"not run: first poll returned HTTP {status} with no ETag"

    units_end = core_remaining(client)
    nums["units_used_whole_run"] = units_start - units_end

    for cat, s in sorted(client.stats.items()):
        nums[f"{cat}.requests"] = s["requests"]
        nums[f"{cat}.body_bytes"] = s["body_bytes"]
        nums[f"{cat}.header_bytes"] = s["header_bytes"]
    nums["total_requests"] = sum(s["requests"] for s in client.stats.values())
    nums["total_body_bytes"] = sum(s["body_bytes"] for s in client.stats.values())
    return nums, renames


def hhmm_utc(epoch):
    if epoch is None:
        return "an unknown time"
    return datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc).strftime("%H:%M UTC")


def selftest():
    """Offline: drive the real `main()` with a stand-in for the network call
    (a real 403 cannot be forced). The first requests succeed, the compare
    request is refused by the rate limiter: the run must record the 403 and the
    reset time and write no partial numbers."""
    import email.message
    import io
    import tempfile

    reset = 1798000000  # 2026-12-23 04:26:40 UTC

    class Resp:
        def __init__(self, payload, headers=None):
            self.status = 200
            self.headers = headers or {}
            self._payload = payload

        def read(self):
            return json.dumps(self._payload).encode()

    def fake_urlopen(req, timeout=None):
        url = req.full_url
        if url.endswith("/rate_limit"):
            return Resp({"resources": {"core": {"remaining": 59}}})
        if "/git/trees/" in url:
            return Resp({"tree": [{"type": "blob", "path": "a.md", "sha": "1"}], "truncated": False})
        hdrs = email.message.Message()
        hdrs["x-ratelimit-remaining"] = "0"
        hdrs["x-ratelimit-reset"] = str(reset)
        raise urllib.error.HTTPError(url, 403, "rate limit exceeded", hdrs, io.BytesIO(b'{"message":"API rate limit exceeded"}'))

    real = urllib.request.urlopen
    urllib.request.urlopen = fake_urlopen
    try:
        with tempfile.TemporaryDirectory() as tmp:
            os.environ["TEPHRA_SPIKE_OUT"] = tmp
            os.environ.pop("NUMBERS", None)
            sys.argv = ["rest.py", "--repo", "a/b", "--base", "x", "--head", "y", "--label", "s1.t", "--unauthenticated"]
            assert main() == 0
            with open(os.path.join(tmp, "numbers-live.txt")) as fh:
                lines = fh.read().splitlines()
    finally:
        urllib.request.urlopen = real
    expected = f"s1.t.rest.status=not run: rate limited (HTTP 403), resets {hhmm_utc(reset)}"
    assert lines == [expected], lines
    assert hhmm_utc(reset) == "04:26 UTC", hhmm_utc(reset)
    print("selftest ok")
    return 0


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--repo", required=True)
    p.add_argument("--base", required=True)
    p.add_argument("--head", required=True)
    p.add_argument("--branch", default="main")
    p.add_argument("--label", required=True)
    p.add_argument("--unauthenticated", action="store_true")
    p.add_argument("--poll-wait", type=int, default=60)
    p.add_argument("--blob-sample", type=int, default=10)
    args = p.parse_args()

    here = os.path.dirname(os.path.abspath(__file__))
    out_dir = os.environ.get("TEPHRA_SPIKE_OUT") or os.path.join(here, "..", "out")
    os.makedirs(out_dir, exist_ok=True)
    numbers_path = os.environ.get("NUMBERS") or os.path.join(out_dir, "numbers-live.txt")

    print(f"== S1 REST half: {args.label} ({args.repo})")
    try:
        nums, renames = run(args)
    except RateLimited as rl:
        msg = f"not run: rate limited (HTTP {rl.status}), resets {hhmm_utc(rl.reset_epoch)}"
        print(msg)
        with open(numbers_path, "a") as fh:
            fh.write(f"{args.label}.rest.status={msg}\n")
        return 0
    except (urllib.error.URLError, OSError) as err:
        msg = f"not run: no network ({type(err).__name__})"
        print(msg)
        with open(numbers_path, "a") as fh:
            fh.write(f"{args.label}.rest.status={msg}\n")
        return 0

    if "status" in nums and len(nums) == 1:
        print(nums["status"])
        with open(numbers_path, "a") as fh:
            fh.write(f"{args.label}.rest.status={nums['status']}\n")
        return 0

    with open(numbers_path, "a") as fh:
        for k, v in nums.items():
            fh.write(f"{args.label}.rest.{k}={v}\n")
            print(f"  {args.label}.rest.{k:<40} {v}")
    with open(os.path.join(out_dir, f"{args.label}-rest-renames.txt"), "w") as fh:
        for kind, old, new, _ in sorted(renames, key=lambda r: (r[1], r[2])):
            fh.write(f"{kind} {old} {new}\n")
    return 0


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "--selftest":
        sys.exit(selftest())
    sys.exit(main())
