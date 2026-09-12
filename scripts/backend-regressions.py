#!/usr/bin/env python3
"""Real Ringboard regression tests; all history, D-Bus and XDG paths are disposable.

Requires ringboard, ringboard-server, busctl and dbus-run-session. Does not use
or control the user's services, clipboard or Wayland compositor.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

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

    def close(self):
        for child in reversed(self.children):
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        self.log.close()


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


CASES = {"wraparound": wraparound, "replacement": replacement, "legacy-replacement": legacy_replacement}


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
