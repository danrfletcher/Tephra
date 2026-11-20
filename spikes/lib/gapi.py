"""Shared helpers for the Google spikes (S4, S5). Not a runnable script.

Credential: the Keychain item `tephra-spike-google` holds a base64 string of
a JSON object {client_id, client_secret, refresh_token, issued_at, ...}. It is
read with `security find-generic-password -s tephra-spike-google -w`; this
module never prints it, never writes it to a file and never puts it on a
command line. Only `mint-token.py` writes the item, and it does so through
`security -i` reading its commands from stdin.
"""
import base64
import http.client
import json
import os
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request

SERVICE = "tephra-spike-google"
TOKEN_URL = "https://oauth2.googleapis.com/token"


def load_credential():
    """The stored credential as a dict, or None when the item is missing."""
    if os.environ.get("TEPHRA_SPIKE_NO_KEYCHAIN") == "1":
        return None
    try:
        out = subprocess.run(
            ["security", "find-generic-password", "-s", SERVICE, "-w"],
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        return None
    if out.returncode != 0:
        return None
    raw = out.stdout.strip()
    if not raw:
        return None
    try:
        return json.loads(base64.b64decode(raw))
    except (ValueError, json.JSONDecodeError):
        return None


class NetworkError(Exception):
    """No answer from the server: refused or dropped connection, DNS failure,
    timeout. The spikes write `not run: no network` for it and exit 0, as
    spikes/s1/rest.py does."""

    def __init__(self, err):
        super().__init__(type(err).__name__)
        self.kind = type(err).__name__


# What urllib and http.client raise when there is no usable network.
NETWORK_ERRORS = (urllib.error.URLError, OSError, http.client.HTTPException)


def not_run_no_network(numbers, err, key="status"):
    """Record `<key>=not run: no network (<ErrorName>)` and print it."""
    msg = f"not run: no network ({err.kind})"
    print(msg)
    numbers.put(key, msg)


class TokenError(Exception):
    """The token endpoint refused; `code` is Google's short error name."""

    def __init__(self, code):
        super().__init__(code)
        self.code = code


def access_token(cred):
    """Exchange the stored long-lived token for a short-lived access token."""
    form = urllib.parse.urlencode(
        {
            "client_id": cred["client_id"],
            "client_secret": cred["client_secret"],
            "refresh_token": cred["refresh_token"],
            "grant_type": "refresh_token",
        }
    ).encode()
    req = urllib.request.Request(TOKEN_URL, data=form, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.loads(resp.read())["access_token"]
    except urllib.error.HTTPError as err:
        try:
            code = json.loads(err.read()).get("error", f"http_{err.code}")
        except ValueError:
            code = f"http_{err.code}"
        raise TokenError(code)
    except NETWORK_ERRORS as err:
        raise NetworkError(err)


class Api:
    """Tiny JSON client that counts requests and bytes per endpoint label."""

    def __init__(self, token):
        self.token = token
        self.stats = {}

    def call(self, method, url, label, body=None, params=None):
        if params:
            url = url + "?" + urllib.parse.urlencode(params)
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Authorization", "Bearer " + self.token)
        if data is not None:
            req.add_header("Content-Type", "application/json")
        started = time.time()
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                status, raw = resp.status, resp.read()
        except urllib.error.HTTPError as err:
            status, raw = err.code, err.read()
        except NETWORK_ERRORS as err:
            raise NetworkError(err)
        s = self.stats.setdefault(label, {"requests": 0, "body_bytes": 0, "seconds": 0.0})
        s["requests"] += 1
        s["body_bytes"] += len(raw)
        s["seconds"] += time.time() - started
        try:
            return status, json.loads(raw) if raw else {}
        except ValueError:
            return status, {"_unparsed_bytes": len(raw)}


def numbers_path():
    here = os.path.dirname(os.path.abspath(__file__))
    out_dir = os.environ.get("TEPHRA_SPIKE_OUT") or os.path.join(here, "..", "out")
    os.makedirs(out_dir, exist_ok=True)
    return os.environ.get("NUMBERS") or os.path.join(out_dir, "numbers-live.txt"), out_dir


class Numbers:
    """key=value lines, printed as they are recorded."""

    def __init__(self, prefix):
        self.prefix = prefix
        self.path, self.out_dir = numbers_path()

    def put(self, key, value):
        line = f"{self.prefix}.{key}={value}"
        with open(self.path, "a") as fh:
            fh.write(line + "\n")
        print(f"  {self.prefix}.{key:<48} {value}")
