#!/usr/bin/env python3
"""Yapfast's key server: the keys that unlock a build of Yapfast made with
YAPFAST_KEYS_URL, and the invite each key may make. Nothing but Python 3.10
or newer is needed:

    python yapfast_keys.py setup                  once: the database and the admin secret
    python yapfast_keys.py serve                  the API, on 127.0.0.1
    python yapfast_keys.py create --note Niko     a new key, shown once
    python yapfast_keys.py revoke k_1a2b3c        by its id, or by the key itself
    python yapfast_keys.py list                   every key, without the keys
    python yapfast_keys.py invites k_1a2b3c 1     how many invites a key may make

It listens on 127.0.0.1 only, twice:

- the public API (port 8787 by default), which Tailscale Funnel publishes
  over HTTPS: POST /v1/validate and POST /v1/invite, which Yapfast calls;
- the admin API (port 8788 by default), never published, which answers only
  requests that carry the admin secret: the commands above.

The server keeps each key's SHA-256 and the admin secret's SHA-256, never
the keys or the secret themselves. The admin secret lives in Windows
Credential Manager (YapfastKeys/admin), or in the YAPFAST_KEYS_ADMIN_TOKEN
environment variable elsewhere: never in Yapfast, in Git or in this folder.
"""

from __future__ import annotations

import argparse
import hashlib
import hmac
import http.server
import json
import os
import secrets
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

# A key: YF, then 25 characters of Crockford's base32 (125 random bits) in
# five groups, as YF-7K2QM-9XH4D-PL3VA-Q7MZ8-B4N8T.
ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
PREFIX = "YF"
GROUPS = 5
GROUP = 5
# Letters typed for the digits they look like, as Crockford's base32 reads them.
LOOKALIKES = str.maketrans({"I": "1", "L": "1", "O": "0"})

PUBLIC_PORT = 8787
ADMIN_PORT = 8788
# What a key made by an invite may itself invite: none, unless the admin says.
INVITED_QUOTA = 0
# A request's body is at most this many bytes.
MAX_BODY = 4096
CREDENTIAL = "YapfastKeys/admin"
TOKEN_ENV = "YAPFAST_KEYS_ADMIN_TOKEN"
DATA_ENV = "YAPFAST_KEYS_DATA"
LOG_LIMIT = 1_000_000


def data_dir(chosen: str | None = None) -> Path:
    """Where the database, the settings and the log are kept."""
    if chosen:
        return Path(chosen)
    if os.environ.get(DATA_ENV):
        return Path(os.environ[DATA_ENV])
    if os.name == "nt":
        return Path(os.environ.get("LOCALAPPDATA", str(Path.home()))) / "YapfastKeys"
    return Path.home() / ".local" / "share" / "yapfast-keys"


def sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def new_key() -> str:
    body = "".join(secrets.choice(ALPHABET) for _ in range(GROUPS * GROUP))
    return "-".join([PREFIX, *(body[at:at + GROUP] for at in range(0, len(body), GROUP))])


def canonical(key: str) -> str | None:
    """A key as typed, without spaces, dashes or case, its look-alike
    letters read as digits: None if it can't be a key."""
    text = "".join(str(key).split()).replace("-", "").upper().translate(LOOKALIKES)
    if not text.startswith(PREFIX):
        return None
    body = text[len(PREFIX):]
    if len(body) != GROUPS * GROUP or any(char not in ALPHABET for char in body):
        return None
    return PREFIX + body


def key_hash(key: str) -> str | None:
    text = canonical(key)
    return sha256_hex(text) if text else None


SCHEMA = """
CREATE TABLE IF NOT EXISTS keys (
    id TEXT PRIMARY KEY,
    key_hash TEXT NOT NULL UNIQUE,
    note TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    revoked_at INTEGER,
    revoked_why TEXT,
    invited_by TEXT REFERENCES keys(id),
    invite_quota INTEGER NOT NULL DEFAULT 0,
    first_seen_at INTEGER,
    last_seen_at INTEGER
);
CREATE INDEX IF NOT EXISTS keys_invited_by ON keys(invited_by);
"""


class Store:
    """The keys, in SQLite: each one's hash, never the key."""

    def __init__(self, path: Path):
        self.db = sqlite3.connect(str(path), check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.executescript(SCHEMA)
        self.db.commit()
        self.lock = threading.Lock()

    def close(self) -> None:
        with self.lock:
            self.db.close()

    def _insert(self, note: str, quota: int, invited_by: str | None) -> tuple[str, str]:
        while True:
            key, ident = new_key(), "k_" + secrets.token_hex(3)
            try:
                self.db.execute(
                    "INSERT INTO keys (id, key_hash, note, created_at, invited_by, invite_quota) "
                    "VALUES (?, ?, ?, ?, ?, ?)",
                    (ident, key_hash(key), note, int(time.time()), invited_by, quota),
                )
                return ident, key
            except sqlite3.IntegrityError:
                continue

    def create(self, note: str, quota: int) -> tuple[str, str]:
        with self.lock, self.db:
            return self._insert(note, max(0, quota), None)

    def _find(self, target: str) -> sqlite3.Row | None:
        """A key's row by its id, or by the key itself."""
        if target.startswith("k_"):
            return self.db.execute("SELECT * FROM keys WHERE id = ?", (target,)).fetchone()
        digest = key_hash(target)
        if not digest:
            return None
        return self.db.execute("SELECT * FROM keys WHERE key_hash = ?", (digest,)).fetchone()

    def _invites(self, row: sqlite3.Row) -> dict:
        used, pending = self.db.execute(
            "SELECT COUNT(first_seen_at), COUNT(*) - COUNT(first_seen_at) FROM keys "
            "WHERE invited_by = ? AND revoked_at IS NULL",
            (row["id"],),
        ).fetchone()
        quota = row["invite_quota"]
        return {"quota": quota, "used": used, "pending": pending,
                "left": max(0, quota - used - pending)}

    def _check(self, key: str) -> tuple[sqlite3.Row | None, dict | None]:
        """The row of a key that's valid, seen now; or why it isn't."""
        digest = key_hash(key) if isinstance(key, str) else None
        row = digest and self.db.execute(
            "SELECT * FROM keys WHERE key_hash = ?", (digest,)).fetchone()
        if not row:
            return None, {"valid": False, "reason": "invalid"}
        if row["revoked_at"] is not None:
            return None, {"valid": False, "reason": "revoked"}
        now = int(time.time())
        self.db.execute(
            "UPDATE keys SET first_seen_at = COALESCE(first_seen_at, ?), last_seen_at = ? WHERE id = ?",
            (now, now, row["id"]),
        )
        return row, None

    def validate(self, key: str) -> dict:
        with self.lock, self.db:
            row, refused = self._check(key)
            if refused:
                return refused
            return {"valid": True, "id": row["id"], "invites": self._invites(row)}

    def invite(self, key: str) -> tuple[int, dict]:
        """A new invite code from a valid key: it replaces the key's last
        code while that hasn't been used, else takes one of its invites."""
        with self.lock, self.db:
            row, refused = self._check(key)
            if refused:
                return 403, refused
            invites = self._invites(row)
            if invites["pending"] == 0 and invites["left"] == 0:
                return 409, {"error": "This key has no invites left.", "reason": "no_invites",
                             "invites": invites}
            now = int(time.time())
            self.db.execute(
                "UPDATE keys SET revoked_at = ?, revoked_why = 'replaced' "
                "WHERE invited_by = ? AND revoked_at IS NULL AND first_seen_at IS NULL",
                (now, row["id"]),
            )
            ident, code = self._insert("invited by " + (row["note"] or row["id"]), INVITED_QUOTA,
                                       row["id"])
            return 200, {"code": code, "id": ident, "invites": self._invites(row)}

    def revoke(self, target: str, cascade: bool) -> list[str] | None:
        with self.lock, self.db:
            row = self._find(target)
            if not row:
                return None
            revoked, queue, now = [], [row["id"]], int(time.time())
            while queue:
                ident = queue.pop()
                changed = self.db.execute(
                    "UPDATE keys SET revoked_at = ?, revoked_why = 'revoked' "
                    "WHERE id = ? AND revoked_at IS NULL", (now, ident)).rowcount
                if changed:
                    revoked.append(ident)
                if cascade:
                    queue += [child["id"] for child in self.db.execute(
                        "SELECT id FROM keys WHERE invited_by = ?", (ident,))]
            return revoked

    def set_quota(self, target: str, quota: int) -> dict | None:
        with self.lock, self.db:
            row = self._find(target)
            if not row:
                return None
            self.db.execute("UPDATE keys SET invite_quota = ? WHERE id = ?",
                            (max(0, quota), row["id"]))
            row = self._find(row["id"])
            return {"id": row["id"], "invites": self._invites(row)}

    def all(self) -> list[dict]:
        with self.lock:
            rows = self.db.execute("SELECT * FROM keys ORDER BY created_at, id").fetchall()
            return [{
                "id": row["id"], "note": row["note"], "created_at": row["created_at"],
                "revoked_at": row["revoked_at"], "revoked_why": row["revoked_why"],
                "invited_by": row["invited_by"], "first_seen_at": row["first_seen_at"],
                "last_seen_at": row["last_seen_at"], "invites": self._invites(row),
            } for row in rows]


class Log:
    """What the server did, without keys or secrets: to the console and
    to server.log, which starts again past a megabyte."""

    def __init__(self, path: Path | None):
        self.path, self.lock = path, threading.Lock()

    def __call__(self, line: str) -> None:
        stamped = time.strftime("%Y-%m-%d %H:%M:%S ") + line
        with self.lock:
            if sys.stdout:
                print(stamped, flush=True)
            if self.path:
                if self.path.exists() and self.path.stat().st_size > LOG_LIMIT:
                    self.path.replace(self.path.with_suffix(".old.log"))
                with self.path.open("a", encoding="utf-8") as log:
                    log.write(stamped + "\n")


class Handler(http.server.BaseHTTPRequestHandler):
    server_version = "YapfastKeys/1"
    sys_version = ""
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):  # noqa: A002 - the base class's name
        pass

    def reply(self, status: int, body: dict) -> None:
        data = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    def body(self) -> dict | None:
        try:
            length = int(self.headers.get("Content-Length") or "0")
        except ValueError:
            return None
        if length <= 0 or length > MAX_BODY:
            return None
        try:
            parsed = json.loads(self.rfile.read(length))
        except (ValueError, UnicodeDecodeError):
            return None
        return parsed if isinstance(parsed, dict) else None

    def do_GET(self):
        self.route("GET")

    def do_POST(self):
        self.route("POST")

    def route(self, method: str) -> None:
        try:
            self.handle_route(method)
        except Exception as error:  # noqa: BLE001 - one request never takes the server down
            self.server.log(f"{method} {self.path}: {type(error).__name__}")
            self.reply(500, {"error": "The key server failed."})


class PublicHandler(Handler):
    """Yapfast's side: checking a key and making its invite."""

    def handle_route(self, method: str) -> None:
        store, log = self.server.store, self.server.log
        if method == "GET" and self.path == "/v1/health":
            return self.reply(200, {"ok": True})
        if method != "POST" or self.path not in ("/v1/validate", "/v1/invite"):
            return self.reply(404, {"error": "Not found."})
        body = self.body()
        if body is None or not isinstance(body.get("key"), str):
            return self.reply(400, {"error": "Send JSON with the key."})
        if self.path == "/v1/validate":
            answer = store.validate(body["key"])
            log(f"validate {answer.get('id', '-')}: {'valid' if answer['valid'] else answer['reason']}")
            answer.pop("id", None)
            return self.reply(200, answer)
        status, answer = store.invite(body["key"])
        log(f"invite: {status} {answer.get('id', answer.get('reason', ''))}")
        answer.pop("id", None)
        return self.reply(status, answer)


class AdminHandler(Handler):
    """The admin's side, on this PC only, with the admin secret."""

    def handle_route(self, method: str) -> None:
        header = self.headers.get("Authorization", "")
        given = header[len("Bearer "):].strip() if header.startswith("Bearer ") else ""
        if not given or not hmac.compare_digest(sha256_hex(given), self.server.admin_hash):
            self.server.log(f"admin {self.path}: refused, no or wrong secret")
            return self.reply(401, {"error": "The admin secret is missing or wrong."})
        store, log = self.server.store, self.server.log
        if method == "GET" and self.path == "/admin/keys":
            return self.reply(200, {"keys": store.all()})
        if method != "POST":
            return self.reply(404, {"error": "Not found."})
        body = self.body()
        if body is None:
            return self.reply(400, {"error": "Send JSON."})
        if self.path == "/admin/keys":
            note = str(body.get("note") or "")[:200]
            ident, key = store.create(note, int(body.get("invites", 1)))
            log(f"admin: created {ident} ({note})")
            return self.reply(200, {"id": ident, "key": key, "note": note})
        if self.path == "/admin/revoke":
            revoked = store.revoke(str(body.get("target", "")), bool(body.get("cascade")))
            if revoked is None:
                return self.reply(404, {"error": "No such key."})
            log(f"admin: revoked {', '.join(revoked) or 'nothing new'}")
            return self.reply(200, {"revoked": revoked})
        if self.path == "/admin/invites":
            changed = store.set_quota(str(body.get("target", "")), int(body.get("quota", 0)))
            if changed is None:
                return self.reply(404, {"error": "No such key."})
            log(f"admin: {changed['id']} may invite {changed['invites']['quota']}")
            return self.reply(200, changed)
        return self.reply(404, {"error": "Not found."})


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True
    # On Windows the address's reuse lets a second server take the same
    # port; there, a second `serve` fails instead.
    allow_reuse_address = os.name != "nt"

    def __init__(self, port: int, handler, store: Store, admin_hash: str, log: Log):
        super().__init__(("127.0.0.1", port), handler)
        self.store, self.admin_hash, self.log = store, admin_hash, log


def start_servers(database: Path, admin_hash: str, public_port: int, admin_port: int,
                  log: Log | None = None) -> tuple[Server, Server, Store]:
    """Both listeners, answering on threads of their own."""
    log = log or Log(None)
    store = Store(database)
    public = Server(public_port, PublicHandler, store, admin_hash, log)
    admin = Server(admin_port, AdminHandler, store, admin_hash, log)
    for server in (public, admin):
        threading.Thread(target=server.serve_forever, daemon=True).start()
    return public, admin, store


# The admin secret in Windows Credential Manager, as a generic credential.
if os.name == "nt":
    import ctypes
    from ctypes import wintypes

    class WinCredential(ctypes.Structure):
        _fields_ = [
            ("Flags", wintypes.DWORD), ("Type", wintypes.DWORD),
            ("TargetName", wintypes.LPWSTR), ("Comment", wintypes.LPWSTR),
            ("LastWritten", wintypes.FILETIME), ("CredentialBlobSize", wintypes.DWORD),
            ("CredentialBlob", ctypes.POINTER(ctypes.c_ubyte)), ("Persist", wintypes.DWORD),
            ("AttributeCount", wintypes.DWORD), ("Attributes", ctypes.c_void_p),
            ("TargetAlias", wintypes.LPWSTR), ("UserName", wintypes.LPWSTR),
        ]

    _advapi = ctypes.WinDLL("advapi32", use_last_error=True)
    _advapi.CredWriteW.argtypes = [ctypes.POINTER(WinCredential), wintypes.DWORD]
    _advapi.CredWriteW.restype = wintypes.BOOL
    _advapi.CredReadW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                  ctypes.POINTER(ctypes.POINTER(WinCredential))]
    _advapi.CredReadW.restype = wintypes.BOOL
    _advapi.CredFree.argtypes = [ctypes.c_void_p]
    _advapi.CredFree.restype = None


def save_secret(secret: str) -> bool:
    if os.name != "nt":
        return False
    blob = secret.encode("utf-8")
    buffer = (ctypes.c_ubyte * len(blob)).from_buffer_copy(blob)
    credential = WinCredential()
    credential.Type = 1  # CRED_TYPE_GENERIC
    credential.TargetName = CREDENTIAL_NAME()
    credential.UserName = "admin"
    credential.CredentialBlobSize = len(blob)
    credential.CredentialBlob = ctypes.cast(buffer, ctypes.POINTER(ctypes.c_ubyte))
    credential.Persist = 2  # CRED_PERSIST_LOCAL_MACHINE: this Windows user, kept
    return bool(_advapi.CredWriteW(ctypes.byref(credential), 0))


def read_secret() -> str | None:
    if os.environ.get(TOKEN_ENV):
        return os.environ[TOKEN_ENV]
    if os.name != "nt":
        return None
    found = ctypes.POINTER(WinCredential)()
    if not _advapi.CredReadW(CREDENTIAL_NAME(), 1, 0, ctypes.byref(found)):
        return None
    try:
        record = found.contents
        return ctypes.string_at(record.CredentialBlob, record.CredentialBlobSize).decode("utf-8")
    finally:
        _advapi.CredFree(found)


def CREDENTIAL_NAME() -> str:  # noqa: N802 - a constant, overridable for tests
    return os.environ.get("YAPFAST_KEYS_CREDENTIAL", CREDENTIAL)


def read_config(folder: Path) -> dict:
    path = folder / "config.json"
    if not path.exists():
        sys.exit(f"No key server is set up in {folder}: run `python {Path(__file__).name} setup` first.")
    return json.loads(path.read_text(encoding="utf-8"))


def setup(args) -> int:
    folder = data_dir(args.data)
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / "config.json"
    config = json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}
    if config and not args.new_secret:
        print(f"Already set up in {folder}. `setup --new-secret` makes a new admin secret.")
        return 0
    secret = "yka_" + secrets.token_urlsafe(32)
    config.update({
        "admin_token_sha256": sha256_hex(secret),
        "public_port": args.port or config.get("public_port", PUBLIC_PORT),
        "admin_port": args.admin_port or config.get("admin_port", ADMIN_PORT),
    })
    Store(folder / "keys.sqlite3").close()
    if save_secret(secret):
        where = f"Windows Credential Manager ({CREDENTIAL_NAME()})"
    else:
        print("This system has no credential store Yapfast's key server knows. Keep this admin")
        print(f"secret in the {TOKEN_ENV} environment variable; it's shown only now:\n\n    {secret}\n")
        where = f"the {TOKEN_ENV} environment variable"
    path.write_text(json.dumps(config, indent=2) + "\n", encoding="utf-8")
    print(f"Set up in {folder}: the database, and the admin secret in {where}.")
    print(f"Start the API with `python {Path(__file__).name} serve`.")
    return 0


def serve(args) -> int:
    folder = data_dir(args.data)
    config = read_config(folder)
    log = Log(folder / "server.log")
    public, admin, _ = start_servers(folder / "keys.sqlite3", config["admin_token_sha256"],
                                     config["public_port"], config["admin_port"], log)
    log(f"public API on http://127.0.0.1:{public.server_address[1]} "
        f"(publish it: tailscale funnel --bg {public.server_address[1]})")
    log(f"admin API on http://127.0.0.1:{admin.server_address[1]} (this PC only)")
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        log("stopped")
    return 0


def admin(args, method: str, path: str, body: dict | None = None) -> dict:
    folder = data_dir(args.data)
    config = read_config(folder)
    secret = read_secret()
    if not secret:
        sys.exit(f"No admin secret: run setup, or set {TOKEN_ENV}.")
    request = urllib.request.Request(
        f"http://127.0.0.1:{config['admin_port']}{path}", method=method,
        data=None if body is None else json.dumps(body).encode("utf-8"),
        headers={"Authorization": f"Bearer {secret}", "Content-Type": "application/json"},
    )
    # Straight to this PC, whatever proxy the environment names.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(request, timeout=15) as response:
            return json.loads(response.read())
    except urllib.error.HTTPError as error:
        try:
            message = json.loads(error.read()).get("error", "")
        except ValueError:
            message = ""
        sys.exit(f"The key server refused: {message or error.code}")
    except urllib.error.URLError:
        sys.exit(f"The key server isn't running: start it with `python {Path(__file__).name} serve`.")


def when(stamp: int | None) -> str:
    return time.strftime("%Y-%m-%d %H:%M", time.localtime(stamp)) if stamp else "never"


def create(args) -> int:
    answer = admin(args, "POST", "/admin/keys", {"note": args.note, "invites": args.invites})
    invites = "1 invite" if args.invites == 1 else f"{args.invites} invites"
    print(f"Key {answer['id']}{' for ' + answer['note'] if answer['note'] else ''}, {invites}:\n")
    print(f"    {answer['key']}\n")
    print("Shown only now: the server keeps only its hash.")
    return 0


def revoke(args) -> int:
    answer = admin(args, "POST", "/admin/revoke", {"target": args.target, "cascade": args.cascade})
    print("Revoked: " + (", ".join(answer["revoked"]) or "nothing new, it was revoked already") + ".")
    print("Yapfast locks on that key within a minute.")
    return 0


def invites(args) -> int:
    answer = admin(args, "POST", "/admin/invites", {"target": args.target, "quota": args.count})
    left = answer["invites"]
    print(f"{answer['id']} may invite {left['quota']}: {left['used']} used, "
          f"{left['pending']} waiting, {left['left']} left.")
    return 0


def listing(args) -> int:
    keys = admin(args, "GET", "/admin/keys")["keys"]
    if not keys:
        print("No keys yet: `create --note NAME` makes one.")
        return 0
    print(f"{'id':10} {'state':9} {'invites':8} {'invited by':10} {'created':16} {'last seen':16} note")
    for key in keys:
        if key["revoked_at"]:
            state = key["revoked_why"] or "revoked"
        else:
            state = "active" if key["first_seen_at"] else "unused"
        invited = key["invites"]
        print(f"{key['id']:10} {state:9} {str(invited['used']) + '/' + str(invited['quota']):8} "
              f"{key['invited_by'] or '-':10} {when(key['created_at']):16} "
              f"{when(key['last_seen_at']):16} {key['note']}")
    return 0


def autostart(args) -> int:
    """Starts the server, hidden, as this Windows user signs in: a shortcut
    in the Startup folder."""
    if os.name != "nt":
        sys.exit("Autostart is for Windows; elsewhere, start `serve` as a service.")
    startup = Path(os.environ["APPDATA"]) / "Microsoft/Windows/Start Menu/Programs/Startup"
    shortcut = startup / "Yapfast key server.lnk"
    if args.state == "off":
        shortcut.unlink(missing_ok=True)
        print("The key server no longer starts with Windows.")
        return 0
    windowless = Path(sys.executable).with_name("pythonw.exe")
    program = windowless if windowless.exists() else Path(sys.executable)
    script = Path(__file__).resolve()
    command = (
        "$s = (New-Object -ComObject WScript.Shell).CreateShortcut($env:YK_LINK);"
        "$s.TargetPath = $env:YK_PROGRAM; $s.Arguments = $env:YK_ARGUMENTS;"
        "$s.WorkingDirectory = $env:YK_FOLDER; $s.WindowStyle = 7; $s.Save()"
    )
    arguments = f'"{script}" serve' + (f' --data "{args.data}"' if args.data else "")
    environment = dict(os.environ, YK_LINK=str(shortcut), YK_PROGRAM=str(program),
                       YK_ARGUMENTS=arguments, YK_FOLDER=str(script.parent))
    subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", command],
                   check=True, env=environment)
    print(f"The key server starts with Windows, hidden: {shortcut}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Yapfast's key server and its admin commands.")
    parser.add_argument("--data", help=f"its folder (default {data_dir()})")
    commands = parser.add_subparsers(dest="command", required=True)
    made = commands.add_parser("setup", help="the database and the admin secret, once")
    made.add_argument("--port", type=int, help=f"the public API's port (default {PUBLIC_PORT})")
    made.add_argument("--admin-port", type=int, help=f"the admin API's port (default {ADMIN_PORT})")
    made.add_argument("--new-secret", action="store_true", help="replace the admin secret")
    made.set_defaults(run=setup)
    commands.add_parser("serve", help="run the API").set_defaults(run=serve)
    made = commands.add_parser("create", help="make a key, shown once")
    made.add_argument("--note", default="", help="whose key it is")
    made.add_argument("--invites", type=int, default=1, help="how many people it may invite (default 1)")
    made.set_defaults(run=create)
    made = commands.add_parser("revoke", help="revoke a key, by its id or the key")
    made.add_argument("target")
    made.add_argument("--cascade", action="store_true", help="and every key it invited, down the line")
    made.set_defaults(run=revoke)
    made = commands.add_parser("invites", help="how many people a key may invite")
    made.add_argument("target")
    made.add_argument("count", type=int)
    made.set_defaults(run=invites)
    commands.add_parser("list", help="every key, without the keys").set_defaults(run=listing)
    made = commands.add_parser("autostart", help="start the server with Windows, or not")
    made.add_argument("state", choices=["on", "off"])
    made.set_defaults(run=autostart)
    args = parser.parse_args(argv)
    return args.run(args)


if __name__ == "__main__":
    sys.exit(main())
