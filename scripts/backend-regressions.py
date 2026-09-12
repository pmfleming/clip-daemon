#!/usr/bin/env python3
"""Real Ringboard regression tests; all history, D-Bus and XDG paths are disposable.

Requires ringboard, ringboard-server, busctl and dbus-run-session. Does not use
or control the user's services, clipboard or Wayland compositor.
"""
import json
import hashlib
import struct
import zlib
import os
import queue
import threading
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid

BINARY = Path(__file__).resolve().parents[1] / "target/debug/clip-daemon"
BUS = "org.laufan.ClipDaemon"


def run(*args, **kwargs):
    return subprocess.check_output(args, timeout=15, **kwargs)


def call(method, params=None, ok=True):
    wire = run("busctl", "--user", "--auto-start=no", "--json=short", "call",
               BUS, "/org/laufan/ClipDaemon", BUS + "1", "Call", "ss",
               method, json.dumps(params or {}))
    response = json.loads(json.loads(wire)["data"][0])
    assert response["ok"] == ok, response
    return response.get("data", response)


def history():
    return call("clipboard.history.query", {"limit": 200})["history"]


def add(value, favorite=False, mime=None):
    args = ["ringboard", "add"]
    if favorite:
        args.append("--favorite")
    if mime:
        args.extend(["--mime-type", mime])
    return run(*args, input=value.encode() if isinstance(value, str) else value)


def wait_for(read):
    for _ in range(150):
        if read():
            return
        time.sleep(0.02)
    raise TimeoutError("isolated service did not start")


class Desktop:
    def __init__(self, root, capacity=2):
        self.root = root
        self.children = []
        self.log = (root / "services.log").open("w")
        run("ringboard", "configure", "server", "--max-main-entries", str(capacity),
            "--max-favorite-entries", str(capacity))
        self.start(os.environ.get("RINGBOARD_SERVER", "ringboard-server"))
        wait_for(lambda: Path(os.environ["RINGBOARD_SOCK"]).is_socket())
        self.daemon = self.start(str(BINARY), "daemon")
        wait_for(lambda: BUS.encode() in run("busctl", "--user", "list", "--acquired"))
        owner = json.loads(run("busctl", "--user", "--json=short", "call",
                              "org.freedesktop.DBus", "/org/freedesktop/DBus",
                              "org.freedesktop.DBus", "GetConnectionUnixProcessID", "s", BUS))
        assert owner["data"][0] == self.daemon.pid
        assert not history()["entries"], "refusing to test nonempty history"

    def start(self, *argv):
        child = subprocess.Popen(argv, stdout=self.log, stderr=self.log)
        self.children.append(child)
        return child

    def restart_daemon(self):
        self.daemon.terminate()
        self.daemon.wait(timeout=5)
        self.daemon = self.start(str(BINARY), "daemon")
        wait_for(lambda: BUS.encode() in run("busctl", "--user", "list", "--acquired"))

    def close(self):
        for child in reversed(self.children):
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        self.log.close()


class Client:
    def __init__(self, desktop):
        self.messages = queue.Queue()
        self.process = subprocess.Popen([str(BINARY), "client"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=desktop.log, text=True)
        desktop.children.append(self.process)
        threading.Thread(target=lambda: [self.messages.put(json.loads(line)) for line in self.process.stdout], daemon=True).start()

    def send(self, request):
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()

    def until(self, predicate, timeout=10):
        deadline = time.monotonic() + timeout
        while True:
            message = self.messages.get(timeout=max(0.01, deadline - time.monotonic()))
            if predicate(message):
                return message
            if time.monotonic() >= deadline:
                raise TimeoutError("expected JSONL message was not received")


def wraparound(_desktop):
    for favorite in (False, True):
        add("entry-0000", favorite)
        first = next(e for e in history()["entries"] if e["favorite"] == favorite)
        add("entry-0001", favorite)
        add("entry-0002", favorite)
        # No intervening query: action-time reads must also detect slot reuse.
        stale = call("clipboard.entry.action", {
            "entry_id": first["id"], "revision": first["revision"], "action": "delete"
        }, ok=False)
        assert stale["error"]["code"] == "stale-action", stale
        rows = [e for e in history()["entries"] if e["favorite"] == favorite]
        assert {e["preview"] for e in rows} == {"entry-0001", "entry-0002"}, rows
        assert first["id"] not in {e["id"] for e in rows}
        for entry in rows:
            details = call("clipboard.entry.details", {"entry_id": entry["id"], "revision": entry["revision"]})
            assert details["entry"]["text"] == entry["preview"]


def replacement(desktop):
    for favorite in (False, True):
        add("oldest", favorite)
        add("newest", favorite)
        for original in ("oldest", "newest"):
            rows = [e for e in history()["entries"] if e["favorite"] == favorite]
            entry = next(e for e in rows if e["preview"] == original)
            lease = call("clipboard.entry.edit.begin", {"entry_id": entry["id"], "revision": entry["revision"]})["edit"]
            committed = call("clipboard.entry.edit.commit", {"edit_id": lease["id"], "value": original + " edited"})
            assert committed["entry"]["text"] == original + " edited"
            after = [e for e in history()["entries"] if e["favorite"] == favorite]
            assert len(after) == 2, after
            assert [e["preview"] for e in after] == [
                e["preview"] + (" edited" if e["id"] == entry["id"] else "") for e in rows
            ], after


def legacy_replacement(_desktop):
    add("original")
    entry = history()["current"]
    lease = call("clipboard.entry.edit.begin", {"entry_id": entry["id"], "revision": entry["revision"]})["edit"]
    result = call("clipboard.entry.edit.commit", {"edit_id": lease["id"], "value": "replacement"}, ok=False)
    assert "policy package" in result["error"]["message"], result
    assert [e["preview"] for e in history()["entries"]] == ["original"]


def artifact_references(desktop):
    directory = Path(os.environ["HOME"]) / "Pictures/Screenshots/clipboard-history"
    directory.mkdir(parents=True)
    paths = [directory / f"clipboard-{uuid.uuid4()}.png" for _ in range(3)]
    for path in paths:
        path.write_bytes(b"registered fixture")
    manifest = Path(os.environ["XDG_STATE_HOME"]) / "clip-daemon/generated-files.json"
    manifest.parent.mkdir(parents=True, exist_ok=True)
    manifest.write_text(json.dumps({"records": [{"path": str(path), "source_entry_id": "source",
        "image_identity": "fixture", "created_at": 0} for path in paths]}))
    # Neither preview truncation nor the 100-file UI limit may lose references.
    prefix = "file:///missing/" + "x" * 700 + "\r\n"
    add(prefix * 101 + "\r\n".join(path.as_uri() for path in paths[:2]) + "\r\n", mime="text/uri-list")
    desktop.restart_daemon()
    history()
    assert [path.exists() for path in paths] == [True, True, False]


def privacy_retry(desktop):
    directory = desktop.root / "bin"
    directory.mkdir()
    control = directory / "systemctl"
    control.write_text('''#!/usr/bin/env python3
import os,sys
from pathlib import Path
root=Path(os.environ["XDG_STATE_HOME"])
state=root/"service-state"
if "show" in sys.argv:
    print(state.read_text() if state.exists() else "active")
else:
    with (root/"attempts").open("a") as f:f.write("attempt\\n")
    if (root/"fail-control").exists():sys.exit(1)
    state.write_text("inactive" if "stop" in sys.argv else "active")
''')
    control.chmod(0o700)
    os.environ["PATH"] = str(directory) + ":" + os.environ["PATH"]
    desktop.restart_daemon()
    state = Path(os.environ["XDG_STATE_HOME"])
    attempts = state / "attempts"
    before = len(attempts.read_text().splitlines())
    failure = state / "fail-control"
    failure.touch()
    for _ in range(2):
        call("clipboard.capture.setPaused", {"paused": True, "private_mode": True}, ok=False)
        settings = call("clipboard.settings.get")
        assert settings["capture"]["desired_private_mode"]
        assert not settings["settings"]["private_mode"]
        assert settings["capture"]["paused"] is None
    assert len(attempts.read_text().splitlines()) == before + 2
    failure.unlink()
    assert call("clipboard.capture.setPaused", {"paused": True, "private_mode": True})["capture"]["private_mode"]
    desktop.restart_daemon()
    assert call("clipboard.settings.get")["settings"]["private_mode"]
    (state / "service-state").write_text("active")
    assert not call("clipboard.settings.get")["settings"]["private_mode"]


def subscription_baselines(desktop):
    for index in range(2):
        messages = queue.Queue()
        client = subprocess.Popen([str(BINARY), "client"], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=desktop.log, text=True)
        desktop.children.append(client)
        threading.Thread(target=lambda p=client, q=messages: [q.put(json.loads(line)) for line in p.stdout], daemon=True).start()
        client.stdin.write(json.dumps({"op": "subscribe", "id": str(index), "streams": [
            "clipboard.history.changed", "clipboard.current.changed"]}) + "\n")
        client.stdin.flush()
        received = set()
        deadline = time.monotonic() + 5
        while len(received) < 2:
            message = messages.get(timeout=max(0.01, deadline - time.monotonic()))
            event = message.get("event", {})
            if event.get("data", {}).get("reason") == "initial":
                received.add(event["stream"])
        assert received == {"clipboard.history.changed", "clipboard.current.changed"}


def echo_identity(desktop):
    def image(last_row):
        def chunk(kind, data):
            return struct.pack("!I", len(data)) + kind + data + struct.pack("!I", zlib.crc32(kind + data))
        pixels = (b"\0" + b"\xff\0\0" * 256) * 127 + b"\0" + last_row * 256
        return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack("!2I5B", 256, 128, 8, 2, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(pixels, level=0)) + chunk(b"IEND", b""))
    original, distinct = image(b"\xff\0\0"), image(b"\0\0\xff")
    assert original[:65536] == distinct[:65536] and original != distinct
    add(original, mime="image/png")
    source = history()["current"]["id"]
    content = hashlib.sha256(b"clip-daemon:entry-content:v1:" + original).digest()
    identity = hashlib.sha256(b"clip-daemon:inline-echo:v2:image/png\0" + content).hexdigest()
    manifest = Path(os.environ["XDG_STATE_HOME"]) / "clip-daemon/generated-files.json"
    manifest.write_text(json.dumps({"records": [], "inline_echoes": [{"identity_version": 2,
        "source_entry_id": source, "image_identity": identity, "created_at": int(time.time())}]}))
    add(distinct, mime="image/png")
    desktop.restart_daemon()
    assert len(history()["entries"]) == 2, "distinct image was collapsed"


def png_contract(desktop):
    import base64
    gif = base64.b64decode("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7")
    add(gif, mime="image/gif")
    original = history()["current"]
    os.environ["CLIP_DAEMON_IMAGE_EDITOR_COMMAND"] = '["cp","{input}","{output}"]'
    desktop.restart_daemon()
    history()  # establish opaque IDs in the restarted daemon
    client = Client(desktop)
    client.send({"op": "subscribe", "id": "watch", "streams": ["clipboard.operation"]})
    client.until(lambda m: m.get("id") == "watch")
    client.send({"op": "call", "id": "edit", "method": "clipboard.entry.action", "params": {
        "entry_id": original["id"], "revision": original["revision"], "action": "annotate"}})
    response = client.until(lambda m: m.get("id") == "edit")["response"]
    assert response["ok"], response
    terminal = client.until(lambda m: m.get("event", {}).get("event") == "failed")
    assert "invalid image" in terminal["event"]["data"]["operation"]["message"], terminal
    assert history()["current"]["id"] == original["id"]


CASES = {"wraparound": wraparound, "replacement": replacement,
         "legacy-replacement": legacy_replacement, "artifact-references": artifact_references,
         "privacy-retry": privacy_retry, "subscription-baselines": subscription_baselines, "echo-identity": echo_identity, "png-contract": png_contract}


def isolated(case):
    with tempfile.TemporaryDirectory(prefix="clip-regression-", dir="/tmp") as directory:
        root = Path(directory)
        env = dict(os.environ, RINGBOARD_SOCK=str(root / "server.sock"),
                   PASTE_SOCK=str(root / "paste.sock"))
        for key, child in [("HOME", "home"), ("XDG_RUNTIME_DIR", "run"), ("XDG_DATA_HOME", "data"),
                           ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")]:
            (root / child).mkdir(mode=0o700)
            env[key] = str(root / child)
        for key in ("WAYLAND_DISPLAY", "DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE", "CLIP_DAEMON_IMAGE_EDITOR_COMMAND"):
            env.pop(key, None)
        subprocess.run(["dbus-run-session", "--", sys.executable, __file__, "--inside", case, str(root)],
                       env=env, check=True, timeout=90)
        print(f"PASS {case}", flush=True)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--inside"]:
        root = Path(sys.argv[3])
        assert os.environ["XDG_DATA_HOME"] == str(root / "data")
        desktop = Desktop(root)
        try:
            CASES[sys.argv[2]](desktop)
        finally:
            desktop.close()
    else:
        cases = sys.argv[1:] or [name for name in CASES if (name != "legacy-replacement" if os.environ.get("RINGBOARD_SERVER") else name != "replacement")]
        for case in cases:
            isolated(case)
