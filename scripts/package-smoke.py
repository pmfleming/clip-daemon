#!/usr/bin/env python3
"""Installed-package/optional rollback checks in private HOME, bus and history."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time

BUS = "org.laufan.ClipDaemon"


def call(method):
    wire = subprocess.check_output(["busctl", "--user", "--json=short", "call", BUS,
        "/org/laufan/ClipDaemon", BUS + "1", "Call", "ss", method, "{}"], timeout=30)
    result = json.loads(json.loads(wire)["data"][0])
    assert result["ok"], result
    return result["data"]


def wait_for(read):
    for _ in range(200):
        value = read()
        if value:
            return value
        time.sleep(0.025)
    raise TimeoutError("isolated package did not become ready")


def server_path(package):
    unit = (package / "share/systemd/user/ringboard-server.service").read_text()
    return Path(next(line.removeprefix("ExecStart=") for line in unit.splitlines() if line.startswith("ExecStart=")))


def check(package, previous):
    binary = package / "bin/clip-daemon"
    units = package / "share/systemd/user"
    paths = list(units.glob("*.service"))
    assert {path.name for path in paths} == {"clip-daemon.service", "ringboard-server.service"}
    daemon_unit = (units / "clip-daemon.service").read_text()
    engine_unit = (units / "ringboard-server.service").read_text()
    assert "Conflicts=ringboard-wayland.service" in daemon_unit
    assert "Wants=ringboard-server.service\n" in daemon_unit
    assert "PartOf=graphical-session.target\n" in daemon_unit
    assert "Requires=ringboard-server.service" not in daemon_unit
    assert "ringboard-wayland.service" not in engine_unit
    assert "Type=notify" in engine_unit and "configure-engine" in engine_unit
    subprocess.run(["systemd-analyze", "--user", "verify", *map(str, paths)], check=True)
    server = server_path(package)
    assert "@" not in str(server) and server.is_file()
    subprocess.run([binary, "configure-engine"], check=True)
    settings = Path(os.environ["XDG_STATE_HOME"]) / "clip-daemon/settings.json"
    assert settings.stat().st_mode & 0o077 == 0
    saved = json.loads(settings.read_text())
    settings.write_text("invalid")
    assert subprocess.run([binary, "configure-engine"], stderr=subprocess.DEVNULL).returncode != 0
    saved.update(capture_paused=True, private_mode=True)
    settings.write_text(json.dumps(saved))
    processes = []
    daemon_pid = None

    def start_server(path):
        process = subprocess.Popen([path], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        processes.append(process)
        def ready():
            assert process.poll() is None
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as client:
                    client.settimeout(1)
                    client.connect(os.environ["RINGBOARD_SOCK"])
                    client.send(b"\xc1")
                    return client.recv(1) == b"\xc1"
            except OSError:
                return False
        wait_for(ready)
        return process

    try:
        process = start_server(server)
        # The installed activation file must start the facade; no manual launch.
        state = call("clipboard.settings.get")
        assert state["retention"]["synchronized"], state
        assert state["capture"]["verified"] and state["capture"]["private_mode"], state
        owner = subprocess.check_output(["busctl", "--user", "--json=short", "call",
            "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
            "GetConnectionUnixProcessID", "s", BUS])
        daemon_pid = json.loads(owner)["data"][0]
        with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as client:
            client.settimeout(2)
            client.connect(os.environ["RINGBOARD_SOCK"])
            client.send(b"\xc2")
            assert client.recv(1) == b"\xc2", "packaged engine lacks capture ingestion"
        print("PASS two-units, legacy-conflict, private-files, invalid-policy, D-Bus-activation, private-startup, live-v2-limits")
        if previous:
            subprocess.run([server.parent / "ringboard", "add"], input=b"synthetic-migration-fixture", check=True, stdout=subprocess.DEVNULL)
            before = call("clipboard.history.query")["history"]["entries"]
            assert len(before) == 1
            # Validate data/settings compatibility across engine rollback/upgrade.
            # This is not a real systemd login/activation test.
            for executable in (server_path(previous), server):
                process.terminate()
                process.wait(timeout=10)
                process = start_server(executable)
                assert call("clipboard.history.query")["history"]["entries"] == before
                state = call("clipboard.settings.get")
                assert state["capture"]["verified"] and state["capture"]["private_mode"]
            print("PASS previous-engine-rollback-and-upgrade-preserve-history-and-private-intent")
    finally:
        if daemon_pid:
            os.kill(daemon_pid, signal.SIGTERM)
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)


def main():
    package = Path(sys.argv[1]).resolve()
    previous = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else None
    if "_CLIP_PACKAGE_SMOKE" in os.environ:
        check(package, previous)
        return
    with tempfile.TemporaryDirectory(prefix="clip-install-") as directory:
        root = Path(directory)
        env = dict(os.environ, _CLIP_PACKAGE_SMOKE="1")
        for variable, relative in {"HOME": "home", "XDG_DATA_HOME": "data", "XDG_CONFIG_HOME": "config",
            "XDG_STATE_HOME": "state", "XDG_CACHE_HOME": "cache", "XDG_RUNTIME_DIR": "runtime"}.items():
            path = root / relative
            path.mkdir(mode=0o700)
            env[variable] = str(path)
        env.update(XDG_DATA_DIRS=str(package / "share"), RINGBOARD_SOCK=str(root / "runtime/ring.sock"))
        for name in ("DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE"):
            env.pop(name, None)
        args = ["dbus-run-session", "--", sys.executable, __file__, str(package)]
        if previous:
            args.append(str(previous))
        child = subprocess.Popen(args, env=env, start_new_session=True)
        try:
            code = child.wait(timeout=90)
        finally:
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        assert code == 0, code


if __name__ == "__main__":
    main()
