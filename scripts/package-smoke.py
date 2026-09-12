#!/usr/bin/env python3
"""Check an installed package with a clean HOME and private D-Bus; no user services."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def check(package):
    binary = package / "bin/clip-daemon"
    units = package / "share/systemd/user"
    paths = list(units.glob("*.service"))
    assert len(paths) == 3
    subprocess.run(["systemd-analyze", "--user", "verify", *map(str, paths)], check=True)
    server_unit = (units / "ringboard-server.service").read_text()
    server = next(line.removeprefix("ExecStart=") for line in server_unit.splitlines() if line.startswith("ExecStart="))
    assert "@" not in server and Path(server).is_file()
    subprocess.run([binary, "configure-engine"], check=True)
    settings = Path(os.environ["XDG_STATE_HOME"]) / "clip-daemon/settings.json"
    original = settings.read_text()
    assert settings.stat().st_mode & 0o077 == 0
    assert subprocess.run([binary, "capture-allowed"]).returncode == 0
    value = json.loads(original)
    value.update(capture_paused=True, private_mode=True)
    settings.write_text(json.dumps(value))
    assert subprocess.run([binary, "capture-allowed"], stderr=subprocess.DEVNULL).returncode == 1
    settings.write_text("invalid")
    assert subprocess.run([binary, "capture-allowed"], stderr=subprocess.DEVNULL).returncode == 1
    settings.write_text(original)
    process = subprocess.Popen([server], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon_pid = None
    try:
        for _ in range(100):
            if Path(os.environ["RINGBOARD_SOCK"]).is_socket():
                break
            assert process.poll() is None
            time.sleep(0.05)
        # Intentionally do not launch the daemon: installed D-Bus activation must work.
        output = subprocess.check_output(["busctl", "--user", "--json=short", "call",
            "org.laufan.ClipDaemon", "/org/laufan/ClipDaemon", "org.laufan.ClipDaemon1",
            "Call", "ss", "clipboard.settings.get", "{}"], timeout=30)
        result = json.loads(json.loads(output)["data"][0])
        assert result["ok"] and result["data"]["retention"]["synchronized"], result
        owner = subprocess.check_output(["busctl", "--user", "--json=short", "call",
            "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
            "GetConnectionUnixProcessID", "s", "org.laufan.ClipDaemon"])
        daemon_pid = json.loads(owner)["data"][0]
        print("PASS installed-units, first-boot-settings, privacy-start-condition, D-Bus-activation, live-limits")
    finally:
        if daemon_pid:
            os.kill(daemon_pid, signal.SIGTERM)
        process.terminate()
        process.wait(timeout=10)


def main():
    package = Path(sys.argv[1]).resolve()
    if "_CLIP_PACKAGE_SMOKE" in os.environ:
        check(package)
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
        child = subprocess.Popen(["dbus-run-session", "--", sys.executable, __file__, str(package)], env=env, start_new_session=True)
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
