#!/usr/bin/env python3
"""Isolated collector qualification. Never starts/stops the user's services."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/clip-daemon"


def run(*args, **kwargs):
    return subprocess.check_output(args, timeout=15, **kwargs)


def wait_for(read):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        value = read()
        if value:
            return value
        time.sleep(0.025)
    raise TimeoutError("isolated capture condition was not reached")


def call(method, params=None, ok=True):
    wire = run("busctl", "--user", "--auto-start=no", "--json=short", "call",
               "org.laufan.ClipDaemon", "/org/laufan/ClipDaemon", "org.laufan.ClipDaemon1",
               "Call", "ss", method, json.dumps(params or {}))
    result = json.loads(json.loads(wire)["data"][0])
    assert result["ok"] == ok, result
    return result.get("data", result)


def history():
    return call("clipboard.history.query", {"limit":200})["history"]["entries"]


def check(root):
    children, logs = [], []

    def start(name, *args, **kwargs):
        log = (root / (name + ".log")).open("w")
        logs.append(log)
        child = subprocess.Popen(args, stderr=log, start_new_session=True,
                                 **({"stdout": log} | kwargs))
        children.append(child)
        return child

    try:
        config = root / "hyprland.lua"
        config.write_text('hl.monitor({ output = "", mode = "1280x720@60", position = "auto", scale = 1 })\n')
        parent_display = os.environ["WAYLAND_DISPLAY"]
        compositor = start("hyprland", "Hyprland", "-c", str(config))
        socket = wait_for(lambda: next((p for p in (root / "r").glob("wayland-*") if p.is_socket()), None))
        os.environ["WAYLAND_DISPLAY"] = socket.name
        run(str(BINARY), "configure-engine")
        settings_path = root / "state/clip-daemon/settings.json"
        settings = json.loads(settings_path.read_text())
        settings["max_entry_bytes"] = 65536
        settings_path.write_text(json.dumps(settings))
        run(str(BINARY), "configure-engine")
        start("server", "ringboard-server")
        wait_for(lambda: Path(os.environ["RINGBOARD_SOCK"]).is_socket())
        daemon = start("daemon", str(BINARY), "daemon")
        wait_for(lambda: b"org.laufan.ClipDaemon" in run("busctl", "--user", "list", "--acquired"))
        assert not history(), "refusing to test nonempty history"

        def command(value, expected):
            if value != "status":
                call("clipboard.capture.setPaused", {"paused": expected, "private_mode": expected})
            state = call("clipboard.settings.get")["capture"]
            assert state["verified"] and state["paused"] == expected, state

        def restart(name):
            nonlocal daemon
            daemon.terminate()
            daemon.wait(timeout=10)
            daemon = start(name, str(BINARY), "daemon")
            wait_for(lambda: b"org.laufan.ClipDaemon" in run("busctl", "--user", "list", "--acquired"))

        def publish(value, mime="text/plain", primary=False):
            args = ["wl-copy", "--type", mime]
            if primary:
                args.append("--primary")
            run(*args, input=value)

        def unchanged(before):
            time.sleep(0.3)
            assert history() == before, "excluded content changed history"

        command("resume", False)
        publish(b"capture-one")
        wait_for(lambda: any(e["preview"] == "capture-one" for e in history()))
        before = history()
        publish(b"capture-one")
        unchanged(before)
        print("PASS real-capture-and-duplicate", flush=True)
        publish(b"primary-is-not-history", primary=True)
        unchanged(before)
        print("PASS primary-ignored", flush=True)

        command("pause", True)
        publish(b"private-synthetic-fixture")
        unchanged(before)
        command("resume", False)
        unchanged(before)
        publish(b"capture-after-resume")
        wait_for(lambda: any(e["preview"] == "capture-after-resume" for e in history()))
        print("PASS pause-fence-and-no-bootstrap-replay", flush=True)

        before = history()
        producer = start("sensitive", str(ROOT / "target/debug/examples/acceptance-offer"), "--sensitive", stdin=subprocess.PIPE)
        producer.stdin.write(b"synthetic-sensitive-fixture")
        producer.stdin.close()
        wait_for(lambda: b"x-kde-passwordManagerHint" in run("wl-paste", "--list-types"))
        unchanged(before)
        print("PASS whole-offer-sensitive-exclusion", flush=True)

        publish(b"x" * 65537, "application/octet-stream")
        unchanged(before)
        publish(b"x" * 65536, "application/octet-stream")
        wait_for(lambda: any(e["byte_size"] == 65536 for e in history()))
        print("PASS exact-limit-and-pre-persistence-rejection", flush=True)
        command("pause", True)
        command("status", True)
        before = history()
        publish(b"private-across-restart")
        restart("daemon-private-restart")
        command("status", True)
        unchanged(before)
        print("PASS persisted-private-startup", flush=True)

        valid_settings = settings_path.read_text()
        settings_path.write_text("{broken")
        restart("daemon-corrupt-settings")
        call("clipboard.settings.get", ok=False)
        publish(b"must-not-capture-invalid-settings")
        time.sleep(0.3)
        settings_path.write_text(valid_settings)
        restart("daemon-restored-settings")
        command("status", True)
        unchanged(before)
        command("resume", False)
        unchanged(before)
        print("PASS malformed-settings-fail-closed", flush=True)

        # A private bus has no systemd manager: restart must fail closed while
        # exposing saved versus effective retention, then recover on correction.
        call("clipboard.settings.update", {"max_entries": settings["max_entries"] - 1}, ok=False)
        state = call("clipboard.settings.get")
        assert not state["capture"]["verified"] and not state["retention"]["synchronized"], state
        publish(b"must-not-capture-failed-retention")
        unchanged(before)
        call("clipboard.capture.setPaused", {"paused": False}, ok=False)
        call("clipboard.settings.update", {"max_entries": settings["max_entries"]})
        unchanged(before)
        print("PASS retention-failure-barrier-and-recovery", flush=True)

        challenge = call("clipboard.history.wipe.prepare")["challenge"]
        call("clipboard.history.wipe.commit", {"challenge_id": challenge["id"], "response": "WIPE"})
        unchanged([])
        publish(b"new-capture-after-wipe")
        wait_for(lambda: any(e["preview"] == "new-capture-after-wipe" for e in history()))
        print("PASS wipe-does-not-recapture-current-selection", flush=True)

        compositor.terminate()
        compositor.wait(timeout=10)
        wait_for(lambda: not call("clipboard.settings.get")["capture"]["verified"])
        start("hyprland-reconnected", "Hyprland", "-c", str(config),
              env=dict(os.environ, WAYLAND_DISPLAY=parent_display))
        wait_for(lambda: socket.is_socket())
        wait_for(lambda: call("clipboard.settings.get")["capture"]["verified"])
        publish(b"capture-after-compositor-reconnect")
        wait_for(lambda: any(e["preview"] == "capture-after-compositor-reconnect" for e in history()))
        print("PASS compositor-disconnect-health-and-reconnect", flush=True)
    finally:
        for child in reversed(children):
            try:
                os.killpg(child.pid, signal.SIGTERM)
                child.wait(timeout=5)
            except ProcessLookupError:
                pass
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
        for log in logs:
            log.close()
        report = ROOT / "target/capture-acceptance"
        report.mkdir(parents=True, exist_ok=True)
        for path in root.glob("*.log"):
            (report / path.name).write_bytes(path.read_bytes())


if __name__ == "__main__":
    if sys.argv[1:2] == ["--inside"]:
        root = Path(sys.argv[2])
        assert os.environ["XDG_DATA_HOME"] == str(root / "data")
        assert os.environ["RINGBOARD_SOCK"] == str(root / "server.sock")
        check(root)
    else:
        with tempfile.TemporaryDirectory(prefix="cc-", dir="/tmp") as directory:
            root = Path(directory)
            env = dict(os.environ, AQ_DRM_DEVICES="/dev/null", RINGBOARD_SOCK=str(root / "server.sock"))
            env["WAYLAND_DISPLAY"] = str(Path(env["XDG_RUNTIME_DIR"]) / env["WAYLAND_DISPLAY"])
            for key, child in {"HOME":"home", "XDG_RUNTIME_DIR":"r", "XDG_DATA_HOME":"data",
                               "XDG_CONFIG_HOME":"config", "XDG_STATE_HOME":"state", "XDG_CACHE_HOME":"cache"}.items():
                (root / child).mkdir(mode=0o700)
                env[key] = str(root / child)
            env.pop("HYPRLAND_INSTANCE_SIGNATURE", None)
            env.pop("CLIP_DAEMON_IMAGE_EDITOR_COMMAND", None)
            subprocess.run(["dbus-run-session", "--", sys.executable, __file__, "--inside", directory], env=env, check=True, timeout=120)
