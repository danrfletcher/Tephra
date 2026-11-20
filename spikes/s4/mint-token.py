#!/usr/bin/env python3
"""Sign in to Google once and store a long-lived token in the Keychain for the
S4 and S5 spikes. Dan runs this; nothing here runs under `cargo test`.

  python3 spikes/s4/mint-token.py --client-json <path to the downloaded client JSON> \
      --client-is-in-production

What it does: loopback OAuth with PKCE on an ephemeral 127.0.0.1 port, open
for at most 5 minutes; the code is exchanged for a long-lived token; the
result (with the issue time) is written to the Keychain item
`tephra-spike-google` through `security -i`, which reads its command from
stdin so the token is never on a command line. Nothing is written to disk and
nothing secret is printed.

`--client-is-in-production` is a statement from you that the Google Cloud
OAuth client is published "In production": a Testing-mode client expires its
tokens after 7 days and would make the 168-hour check meaningless.
"""
import argparse
import base64
import hashlib
import http.server
import json
import os
import secrets
import subprocess
import sys
import threading
import time
import urllib.parse
import urllib.request
import webbrowser

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "lib"))
import gapi  # noqa: E402

SCOPES = "https://www.googleapis.com/auth/drive https://www.googleapis.com/auth/documents"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--client-json", required=True)
    ap.add_argument("--client-is-in-production", action="store_true")
    args = ap.parse_args()
    if not args.client_is_in_production:
        print("refused: pass --client-is-in-production once the OAuth client is published In production")
        return 2

    with open(args.client_json) as fh:
        client = json.load(fh)
    client = client.get("installed") or client.get("web") or client
    client_id, client_secret = client["client_id"], client["client_secret"]

    verifier = secrets.token_urlsafe(64)
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
    state = secrets.token_urlsafe(16)
    result = {}

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            if q.get("state", [""])[0] == state and "code" in q:
                result["code"] = q["code"][0]
            else:
                result["error"] = q.get("error", ["unexpected request"])[0]
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"You can close this tab and return to the terminal.\n")

        def log_message(self, *_):
            pass

    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    redirect = f"http://127.0.0.1:{port}/"
    url = "https://accounts.google.com/o/oauth2/v2/auth?" + urllib.parse.urlencode(
        {
            "client_id": client_id,
            "redirect_uri": redirect,
            "response_type": "code",
            "scope": SCOPES,
            "code_challenge": challenge,
            "code_challenge_method": "S256",
            "state": state,
            "access_type": "offline",
            "prompt": "consent",
        }
    )
    print("Open this URL if a browser does not open (listening on 127.0.0.1 for up to 5 minutes):")
    print(url)
    try:
        webbrowser.open(url)
    except Exception:
        pass
    server.timeout = 1
    deadline = time.time() + 300
    thread_done = threading.Event()

    def serve():
        while time.time() < deadline and not result:
            server.handle_request()
        thread_done.set()

    threading.Thread(target=serve, daemon=True).start()
    thread_done.wait(timeout=305)
    server.server_close()
    if "code" not in result:
        print("not signed in: " + result.get("error", "timed out after 5 minutes"))
        return 1

    form = urllib.parse.urlencode(
        {
            "code": result["code"],
            "client_id": client_id,
            "client_secret": client_secret,
            "redirect_uri": redirect,
            "grant_type": "authorization_code",
            "code_verifier": verifier,
        }
    ).encode()
    with urllib.request.urlopen(urllib.request.Request(gapi.TOKEN_URL, data=form, method="POST"), timeout=60) as resp:
        tokens = json.loads(resp.read())
    long_lived = tokens.get("refresh_token")
    if not long_lived:
        print("Google returned no long-lived token (revoke the app's access at myaccount.google.com and retry)")
        return 1

    record = {
        "client_id": client_id,
        "client_secret": client_secret,
        "refresh_token": long_lived,
        "issued_at": int(time.time()),
        "client_publishing_status_asserted_by_operator": "in-production",
    }
    blob = base64.b64encode(json.dumps(record).encode()).decode()
    # `security -i` reads its commands from stdin: the secret never appears in argv.
    cmd = f"add-generic-password -U -s {gapi.SERVICE} -a tephra-spike -w {blob}\n"
    proc = subprocess.run(["security", "-i"], input=cmd, text=True, capture_output=True)
    if proc.returncode != 0:
        print("could not write the Keychain item (exit code " + str(proc.returncode) + ")")
        return 1
    issued = time.strftime("%Y-%m-%d %H:%M:%S UTC", time.gmtime(record["issued_at"]))
    print(f"signed in; Keychain item {gapi.SERVICE} written; token issued {issued}")
    print("Run `python3 spikes/s4/run.py recheck` after at least 168 hours.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
