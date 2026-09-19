#!/usr/bin/env python3
"""Real desktop clients for isolated-acceptance.py; never run against a user bus."""
import json
import os
from pathlib import Path
import select
import shutil
import sys
import time
import tty


def terminal_sink(path):
    path = Path(path)
    tty.setraw(sys.stdin.fileno())
    path.with_suffix(".ready").touch()
    value = bytearray()
    while select.select([sys.stdin], [], [], 60)[0]:
        data = os.read(sys.stdin.fileno(), 4096)
        if not data:
            break
        value.extend(data)
        path.write_bytes(value)


def check(root, start, passed, sink, sink_process, api):
    run, wait_for, text = api["run"], api["wait_for"], api["text"]
    ghostty = os.environ.get("GHOSTTY") or shutil.which("ghostty")
    assert ghostty, "Ghostty is required for terminal paste qualification"
    terminal_path = root / "terminal-pasted"
    terminal = start("terminal", ghostty, "--gtk-single-instance=false",
        "--title=clip-daemon acceptance terminal", "-e", sys.executable,
        str(Path(__file__).resolve()), "--sink", str(terminal_path))
    target = wait_for(lambda: next((c for c in api["clients"]() if c["title"] == "clip-daemon acceptance terminal"), None))
    wait_for(lambda: terminal_path.with_suffix(".ready").exists())

    def focus(address):
        assert run("hyprctl", "dispatch", f"hl.dsp.focus({{ window = 'address:{address}' }})").strip() == b"ok"
        wait_for(lambda: json.loads(run("hyprctl", "-j", "activewindow")).get("address") == address)

    value = "clip-terminal-direct-paste"
    selected = text(value)
    focus(target["address"])
    session = api["call"]("clipboard.session.begin")["session"]
    assert session["target_available"]
    api["action"](selected, "paste", session_id=session["id"])
    api["call"]("clipboard.session.hidden", {"session_id": session["id"]})
    wait_for(lambda: terminal_path.exists() and terminal_path.read_bytes() == value.encode())
    passed("ghostty-targeted-paste")

    config = os.environ.get("SHELLLIST_QML_ROOT")
    if config:
        config = Path(config).resolve()
        quickshell = os.environ.get("QUICKSHELL") or shutil.which("quickshell")
        search = os.environ.get("SHELLLIST_SEARCH") or shutil.which("shelllist-search")
        assert quickshell and search and (config / "shell/shell.qml").is_file(), "Incomplete Shelllist qualification inputs"
        bin_path = root / "shelllist-bin"
        bin_path.mkdir()
        (bin_path / "clip-daemon").symlink_to(api["BINARY"])
        (bin_path / "shelllist-search").symlink_to(Path(search).resolve())
        # The real shell eagerly loads non-clipboard surfaces. Never launch their
        # real daemons (or access host system services) during clipboard tests.
        for daemon in ("nm-daemon", "bt-daemon", "bar-daemon", "app-daemon", "update-daemon"):
            stub = bin_path / daemon
            stub.write_text("#!/bin/sh\nexit 1\n")
            stub.chmod(0o700)
        env = dict(os.environ, PATH=str(bin_path) + ":" + os.environ["PATH"], SHELLLIST_MODE="popover",
            QML_IMPORT_PATH=str(config / "qml"), QML2_IMPORT_PATH=str(config / "qml"))
        shell = start("shelllist", quickshell, "--path", str(config / "shell"), env=env)

        def ipc(method, *args):
            return run(quickshell, "ipc", "--path", str(config / "shell"), "--newest",
                "call", "shelllist", method, *args, env=env)

        time.sleep(3)
        assert not json.loads(ipc("status"))["visible"]

        def picker_paste(address, payload, read_result, expected):
            text(payload)
            focus(address)
            ipc("open", "clipboard")
            wait_for(lambda: json.loads(ipc("status"))["visible"])
            wait_for(lambda: any(layer.get("namespace") == "shelllist"
                for monitor in json.loads(run("hyprctl", "-j", "layers")).values()
                for level in monitor.get("levels", {}).values() for layer in level))
            # Actual shell/controller/content, not a synthetic layer-shell host.
            # Allow the asynchronous history query and picker animation to settle.
            time.sleep(3)
            assert run("hyprctl", "dispatch", "hl.dsp.send_shortcut({ mods = '', key = 'Return' })").strip() == b"ok"
            wait_for(lambda: read_result() == expected)
            wait_for(lambda: not json.loads(ipc("status"))["visible"])

        focus(sink["address"])
        run("hyprctl", "dispatch", f"hl.dsp.send_shortcut({{ mods = 'CTRL', key = 'a', window = 'address:{sink['address']}' }})")
        picker_paste(sink["address"], "shelllist-layer-shell-gtk", lambda: (root / "pasted").read_text(), "shelllist-layer-shell-gtk")
        passed("shelllist-layer-shell-gtk-paste")
        payload = "shelllist-layer-shell-terminal"
        picker_paste(target["address"], payload, terminal_path.read_bytes, (value + payload).encode())
        passed("shelllist-layer-shell-terminal-paste")
        shell.terminate()
        shell.wait(timeout=5)
    else:
        print("NOT RUN Shelllist layer-shell checks: set SHELLLIST_QML_ROOT and SHELLLIST_SEARCH", flush=True)

    terminal.terminate()
    terminal.wait(timeout=5)
    sink_process.terminate()
    sink_process.wait(timeout=5)
    wait_for(lambda: not api["clients"]())
    session = api["call"]("clipboard.session.begin")["session"]
    assert not session["target_available"] and session["paste_mode"] == "copy-only"
    selected = text("no-target-manual-paste")
    operation = api["action"](selected, "paste", session_id=session["id"])["operation"]
    assert operation["status"] == "completed", operation
    api["call"]("clipboard.session.hidden", {"session_id": session["id"]})
    assert run("wl-paste", "--no-newline") == b"no-target-manual-paste"
    passed("missing-target-copy-only")


if __name__ == "__main__" and sys.argv[1:2] == ["--sink"]:
    terminal_sink(sys.argv[2])
