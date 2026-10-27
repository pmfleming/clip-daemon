#!/usr/bin/env python3
"""Inert activation fixture: only one synthetic settings read, no domain imports.

Always bounded, even if its private bus/parent disappears. Never use the default
session/system bus, compositor, Ringboard, helpers, clipboard or user files.
"""
import argparse
import json
import os
from pathlib import Path
import re
import signal


def private_address(address, runtime):
    # dbus-daemon adds its GUID to the activation environment. Permit only that
    # suffix, never a second address, another path or an arbitrary option.
    return re.fullmatch(re.escape("unix:path=" + str(runtime / "bus")) +
                        r"(?:,guid=[0-9a-f]{32})?", address) is not None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("audit", type=Path)
    parser.add_argument("--mode", choices=("v2", "v1", "fail", "no-name", "exit-after-read"), default="v2")
    args = parser.parse_args()
    signal.alarm(10)
    address = os.environ["DBUS_SESSION_BUS_ADDRESS"]
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    if (not runtime.is_absolute() or runtime.name != "run" or
            not runtime.parent.name.startswith("clip-smoke-") or
            not private_address(address, runtime) or not runtime.is_dir() or
            runtime.stat().st_uid != os.geteuid() or runtime.stat().st_mode & 0o777 != 0o700):
        raise RuntimeError("fixture requires an explicit private smoke bus")

    def audit(kind):
        with args.audit.open("a") as output:
            output.write(json.dumps({"kind": kind, "pid": os.getpid(), "pgid": os.getpgrp(),
                                     "env": dict(os.environ)}) + "\n")

    audit("start")
    if args.mode == "fail":
        return 7

    import gi
    gi.require_version("Gio", "2.0")
    from gi.repository import Gio, GLib

    loop = GLib.MainLoop()
    signal.signal(signal.SIGTERM, lambda *_: loop.quit())
    connection = Gio.DBusConnection.new_for_address_sync(
        address, Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT |
        Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION, None, None)
    connection.connect("closed", lambda *_: loop.quit())
    interface = "org.laufan.ClipDaemon1" if args.mode == "v1" else "org.laufan.ClipDaemon2"
    node = Gio.DBusNodeInfo.new_for_xml(
        f'<node><interface name="{interface}"><method name="Call">'
        '<arg type="s" direction="in"/><arg type="s" direction="in"/>'
        '<arg type="s" direction="out"/></method></interface></node>')

    def called(_connection, _sender, _path, _interface, _member, parameters, invocation):
        if parameters.unpack() != ("clipboard.settings.get", "{}"):
            invocation.return_dbus_error("org.laufan.Fixture.ReadOnly", "only synthetic settings read allowed")
            return
        audit("read")
        result = {"ok": True, "data": {"synthetic_fixture": True}}
        invocation.return_value(GLib.Variant("(s)", (json.dumps(result),)))
        if args.mode == "exit-after-read":
            GLib.timeout_add(20, lambda: (loop.quit(), False)[1])

    registration = connection.register_object("/org/laufan/ClipDaemon", node.interfaces[0], called, None, None)
    if args.mode != "no-name":
        connection.call_sync(
            "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
            "RequestName", GLib.Variant("(su)", ("org.laufan.ClipDaemon", 0)),
            GLib.VariantType.new("(u)"), Gio.DBusCallFlags.NONE, 2000, None)
    loop.run()
    connection.unregister_object(registration)
    try:
        connection.close_sync(None)
    except GLib.Error:
        # Bus teardown can win this race. Closure is already the desired state;
        # unrelated close failures still fail the fixture.
        if not connection.is_closed():
            raise
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
