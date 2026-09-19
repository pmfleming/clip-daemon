#!/usr/bin/env python3
"""Real Wayland/Ringboard acceptance in a disposable nested desktop.

Requires Hyprland (Lua configuration), ringboard-server/wayland, wl-paste,
Satty, Ghostty, busctl, dbus-run-session, and Python GTK3 introspection for the paste sink.
Optional real Shelllist checks: SHELLLIST_QML_ROOT, SHELLLIST_SEARCH, QUICKSHELL.
Never connects the tested daemon to the user's history or session bus.
"""
import json
import os
from pathlib import Path
import queue
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zlib

PROJECT = Path(__file__).resolve().parents[1]
BINARY = PROJECT / "target/debug/clip-daemon"
BUS = "org.laufan.ClipDaemon"
OBJECT = "/org/laufan/ClipDaemon"
INTERFACE = BUS + "1"


def wait_for(read, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = read()
        if value:
            return value
        time.sleep(0.1)
    raise TimeoutError(f"condition did not become true in {timeout}s")


def run(*args, **kwargs):
    return subprocess.check_output(args, timeout=20, **kwargs)


def call(method, params=None, ok=True):
    wire = run("busctl", "--user", "--auto-start=no", "--json=short", "call",
               BUS, OBJECT, INTERFACE, "Call", "ss", method, json.dumps(params or {}))
    response = json.loads(json.loads(wire)["data"][0])
    assert response["ok"] == ok, response
    return response.get("data", response)


def history(query=""):
    return call("clipboard.history.query", {"query": query, "limit": 200})["history"]


def action(entry, name, **extra):
    return call("clipboard.entry.action", {
        "entry_id": entry["id"], "revision": entry["revision"], "action": name, **extra
    })


def text(value):
    call("clipboard.selection.publishText", {"text": value})
    return wait_for(lambda: next(iter(history(value)["entries"]), None))


def shortcut(address, key):
    argument = f"hl.dsp.send_shortcut({{ mods = '', key = '{key}', window = 'address:{address}' }})"
    assert run("hyprctl", "dispatch", argument).strip() == b"ok"


def clients():
    return json.loads(run("hyprctl", "-j", "clients"))


def png():
    def chunk(kind, data):
        return struct.pack("!I", len(data)) + kind + data + struct.pack("!I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack("!2I5B", 4, 4, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress((b"\0" + b"\xff\0\0" * 4) * 4)) + chunk(b"IEND", b""))


def paste_sink(path):
    import gi
    gi.require_version("Gtk", "3.0")
    from gi.repository import Gtk, Gdk
    window = Gtk.Window(title="clip-daemon acceptance sink")
    entry = Gtk.Entry()
    entry.connect("changed", lambda value: Path(path).write_text(value.get_text()))
    def key(widget, event):
        clipboard = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD)
        print(f"key={event.keyval} state={event.state} targets={clipboard.wait_for_targets()} clipboard={clipboard.wait_for_text()!r}", flush=True)
        return False
    entry.connect("key-press-event", key)
    window.add(entry)
    window.connect("destroy", Gtk.main_quit)
    window.show_all()
    entry.grab_focus()
    Gtk.main()


def acceptance(root):
    children = []
    logs = []
    results = []

    def start(name, *argv, **options):
        log = (root / f"{name}.log").open("w")
        logs.append(log)
        child = subprocess.Popen(argv, stderr=log, start_new_session=True, **({"stdout": log} | options))
        children.append(child)
        return child

    def passed(name):
        results.append({"check": name, "result": "pass"})
        print(f"PASS {name}", flush=True)

    try:
        config = root / "hyprland.lua"
        config.write_text('hl.monitor({ output = "", mode = "1280x720@60", position = "auto", scale = 1 })\n')
        start("hyprland", "Hyprland", "-c", str(config))
        runtime = root / "r"
        socket = wait_for(lambda: next((p for p in runtime.glob("wayland-*") if p.is_socket()), None))
        instance = wait_for(lambda: next(runtime.glob("hypr/*/.socket.sock"), None))
        os.environ["WAYLAND_DISPLAY"] = socket.name
        os.environ["HYPRLAND_INSTANCE_SIGNATURE"] = instance.parent.name
        run("dbus-update-activation-environment", "WAYLAND_DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE")
        protocols = run("wayland-info", stderr=subprocess.DEVNULL)
        assert b"ext_data_control_manager_v1" in protocols
        passed("isolated-wayland-protocols")
        # Apply a deliberately small, shared capture/publication limit before
        # either engine starts; every fixture below fits except the rejection test.
        run(str(BINARY), "configure-engine")
        settings_path = root / "state/clip-daemon/settings.json"
        settings = json.loads(settings_path.read_text())
        settings["max_entry_bytes"] = 65536
        settings_path.write_text(json.dumps(settings))
        run(str(BINARY), "configure-engine")
        start("ringboard", os.environ.get("RINGBOARD_SERVER", "ringboard-server"))
        wait_for(lambda: Path(os.environ["RINGBOARD_SOCK"]).is_socket())
        daemon = start("daemon", str(BINARY), "daemon")
        wait_for(lambda: BUS.encode() in run("busctl", "--user", "list", "--acquired"))
        owner = json.loads(run("busctl", "--user", "--json=short", "call", "org.freedesktop.DBus",
                              "/org/freedesktop/DBus", "org.freedesktop.DBus", "GetConnectionUnixProcessID", "s", BUS))
        assert owner["data"][0] == daemon.pid, "refusing to test an unexpected daemon"
        assert not history()["entries"], "refusing to mutate nonempty initial history"
        passed("isolated-empty-ringboard")
        messages = queue.Queue()
        monitor = start("events", str(BINARY), "client", stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        threading.Thread(target=lambda: [messages.put(json.loads(line)) for line in monitor.stdout], daemon=True).start()
        monitor.stdin.write(json.dumps({"op": "subscribe", "id": "watch", "streams": ["clipboard.operation"]}) + "\n")
        monitor.stdin.flush()
        subscribed = messages.get(timeout=10)
        assert subscribed.get("id") == "watch" and subscribed["response"]["ok"], subscribed

        def annotate(entry):
            monitor.stdin.write(json.dumps({"op": "call", "id": "annotate", "method": "clipboard.entry.action", "params": {
                "entry_id": entry["id"], "revision": entry["revision"], "action": "annotate"
            }}) + "\n")
            monitor.stdin.flush()
            response = messages.get(timeout=10)
            while response.get("id") != "annotate":
                response = messages.get(timeout=10)
            assert response["response"]["ok"], response
            return response["response"]["data"]["operation"]["id"]

        def terminal_event(operation_id):
            while not messages.empty():
                message = messages.get_nowait()
                # JSONL event wrappers carry the daemon's envelope under `event`.
                envelope = message.get("event", message)
                if not isinstance(envelope, dict):
                    continue
                operation = envelope.get("data", {}).get("operation", {})
                if operation.get("id") == operation_id and operation.get("status") != "started":
                    return operation
            return None

        value = "clip-acceptance-text"
        selected = text(value)
        action(selected, "copy")
        assert run("wl-paste", "--no-newline").decode() == value
        passed("text-capture-copy-source-exit")

        selected = history(value)["entries"][0]
        action(selected, "favorite")
        selected = wait_for(lambda: next((e for e in history(value)["entries"] if e["favorite"]), None))
        action(selected, "unfavorite")
        wait_for(lambda: next((e for e in history(value)["entries"] if not e["favorite"]), None))
        passed("favorite-round-trip")

        sink_process = start("paste-sink", sys.executable, __file__, "--paste-sink", str(root / "pasted"))
        sink = wait_for(lambda: next((c for c in clients() if c["title"] == "clip-daemon acceptance sink"), None))
        assert run("hyprctl", "dispatch", f"hl.dsp.focus({{ window = 'address:{sink['address']}' }})").strip() == b"ok"
        wait_for(lambda: json.loads(run("hyprctl", "-j", "activewindow")).get("address") == sink["address"])
        (root / "devices.log").write_bytes(run("hyprctl", "-j", "devices"))
        session = call("clipboard.session.begin")["session"]
        assert session["target_available"]
        selected = history(value)["entries"][0]
        assert action(selected, "paste", session_id=session["id"])["operation"]["status"] == "paste-prepared"
        call("clipboard.session.hidden", {"session_id": session["id"]})
        (root / "types.log").write_bytes(run("wl-paste", "--list-types"))
        try:
            wait_for(lambda: (root / "pasted").exists() and (root / "pasted").read_text() == value, timeout=5)
            passed("hyprland-targeted-paste")
        except TimeoutError:
            results.append({"check": "hyprland-targeted-paste", "result": "fail", "detail": "shortcut reached GTK but text was not pasted; see paste-sink.log"})
            print("FAIL hyprland-targeted-paste", flush=True)

        import importlib.util
        spec = importlib.util.spec_from_file_location("desktop_clients", PROJECT / "scripts/desktop-clients.py")
        desktop_clients = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(desktop_clients)
        desktop_clients.check(root, start, passed, sink, sink_process, globals())

        image = png()
        run(str(BINARY), "publish", "--mime", "image/png", input=image)
        selected = wait_for(lambda: next((e for e in history()["entries"] if e["kind"] == "image"), None))
        action(selected, "copy")
        assert run("wl-paste", "--type", "image/png") == image
        passed("image-round-trip")
        for key, expected in [("Return", "completed"), ("Escape", "cancelled")]:
            selected = history()["current"]
            before = selected["revision"]
            operation_id = annotate(selected)
            editor = wait_for(lambda: next((c for c in clients() if "satty" in c["class"].lower()), None))
            # A just-closed layer-shell picker can leave keyboard focus unset.
            # Drive the editor only after an explicit, observed focus transition.
            run("hyprctl", "dispatch", f"hl.dsp.focus({{ window = 'address:{editor['address']}' }})")
            wait_for(lambda: json.loads(run("hyprctl", "-j", "activewindow")).get("address") == editor["address"])
            time.sleep(0.5)
            shortcut(editor["address"], key)
            wait_for(lambda: not any(c["address"] == editor["address"] for c in clients()))
            terminal = wait_for(lambda: terminal_event(operation_id))
            if expected == "completed":
                assert terminal["status"] == "completed" and not terminal.get("warning"), terminal
            else:
                assert terminal["status"] == "failed" and "cancelled" in terminal["message"], terminal
            current = history()["current"]
            assert current["kind"] == "image"
            if expected == "cancelled":
                assert current["revision"] == before
            assert run("wl-paste", "--type", "image/png").startswith(b"\x89PNG")
            passed(f"satty-{expected}")

        files = [root / "one.txt", root / "two.txt"]
        for path in files:
            path.write_text("acceptance fixture")
        call("clipboard.selection.publishFiles", {"operation": "cut", "paths": list(map(str, files))})
        expected = "\r\n".join(path.as_uri() for path in files) + "\r\n"
        assert run("wl-paste", "--no-newline", "--type", "text/uri-list").decode() == expected
        assert run("wl-paste", "--no-newline", "--type", "x-special/gnome-copied-files").decode() == "cut\n" + expected
        passed("multi-file-mime-round-trip")

        def synthetic_offer(name, payload, *args):
            producer = start(name, str(PROJECT / "target/debug/examples/acceptance-offer"), *args, stdin=subprocess.PIPE)
            producer.stdin.write(payload)
            producer.stdin.close()
            return producer

        before_exclusion = history()
        secret = b"clip-sensitive-synthetic-fixture"
        synthetic_offer("sensitive-offer", secret, "--sensitive")
        wait_for(lambda: b"x-kde-passwordManagerHint" in run("wl-paste", "--list-types"))
        time.sleep(0.3)
        assert history() == before_exclusion
        assert not history(secret.decode())["entries"]
        assert daemon.poll() is None
        passed("sensitive-marker-before-capture")

        synthetic_offer("oversized-offer", b"clip-oversized-fixture" + b"x" * 65536,
                        "--mime", "application/octet-stream")
        time.sleep(0.3)
        assert history() == before_exclusion
        assert not history("clip-oversized-fixture")["entries"]
        assert daemon.poll() is None
        text("capture-resumes-after-policy-rejection")
        passed("oversized-offer-before-persistence")

        first, second = text("clip-delete-one"), text("clip-delete-two")
        targets = [{"entry_id": e["id"], "revision": e["revision"]} for e in (first, second)]
        call("clipboard.entries.delete", {"entries": targets})
        assert not history("clip-delete-")["entries"]
        passed("bulk-delete")
        challenge = call("clipboard.history.wipe.prepare")["challenge"]
        call("clipboard.history.wipe.commit", {"challenge_id": challenge["id"], "response": "WIPE"})
        assert not history()["entries"]
        passed("two-phase-wipe")
        assert all(result["result"] == "pass" for result in results), "acceptance has failed checks"
    except Exception as error:
        results.append({"check": "run", "result": "fail", "detail": str(error)})
        raise
    finally:
        for child in reversed(children):
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
        for log in logs:
            log.close()
        for name in ("pasted", "terminal-pasted"):
            if (root / name).exists():
                (root / f"{name}.log").write_bytes((root / name).read_bytes())
        report = PROJECT / "target/live-acceptance"
        report.mkdir(exist_ok=True, mode=0o700)
        (report / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        (report / "clients.json").write_text(json.dumps({
            "shelllist_qml_root": os.environ.get("SHELLLIST_QML_ROOT"),
            "shelllist_search": os.environ.get("SHELLLIST_SEARCH"),
            "quickshell": os.environ.get("QUICKSHELL", "quickshell"),
            "ghostty": os.environ.get("GHOSTTY", "ghostty"),
            "ringboard_server": os.environ.get("RINGBOARD_SERVER", "ringboard-server"),
            "capture": "clip-daemon in-process",
        }, indent=2) + "\n")
        for log in root.glob("*.log"):
            (report / log.name).write_bytes(log.read_bytes())


if __name__ == "__main__":
    if sys.argv[1:2] == ["--paste-sink"]:
        paste_sink(sys.argv[2])
    elif sys.argv[1:2] == ["--inside"]:
        root = Path(sys.argv[2])
        assert os.environ["XDG_RUNTIME_DIR"] == str(root / "r")
        assert os.environ["XDG_DATA_HOME"] == str(root / "data")
        assert os.environ["RINGBOARD_SOCK"] == str(root / "server.sock")
        acceptance(root)
    else:
        # Keep the runtime path short enough for Hyprland's Unix-domain sockets.
        # Nix's TMPDIR can be too long for a Unix-domain socket path.
        with tempfile.TemporaryDirectory(prefix="c-", dir="/tmp") as directory:
            root = Path(directory)
            env = dict(os.environ, AQ_DRM_DEVICES="/dev/null", RINGBOARD_SOCK=str(root / "server.sock"),
                       PASTE_SOCK=str(root / "paste.sock"), GDK_BACKEND="wayland")
            env["WAYLAND_DISPLAY"] = str(Path(env["XDG_RUNTIME_DIR"]) / env["WAYLAND_DISPLAY"])
            for key, child in [("HOME", "home"), ("XDG_RUNTIME_DIR", "r"), ("XDG_DATA_HOME", "data"),
                               ("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache")]:
                (root / child).mkdir(mode=0o700)
                env[key] = str(root / child)
            env.pop("HYPRLAND_INSTANCE_SIGNATURE", None)
            env.pop("CLIP_DAEMON_IMAGE_EDITOR_COMMAND", None)
            sys.exit(subprocess.call(["dbus-run-session", "--", sys.executable, __file__, "--inside", directory], env=env))
