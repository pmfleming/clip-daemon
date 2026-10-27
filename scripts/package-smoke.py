#!/usr/bin/env python3
"""Static package wiring check and private, synthetic activation test boundary.

Real packaged-main/engine/rollback acceptance is deliberately gated: main creates
Ringboard and capture policy, which the inert activation fixture does not test.
Nothing in this module starts that main or discovers tools from inherited PATH.
"""
import argparse
import configparser
from dataclasses import dataclass
import json
import os
from pathlib import Path
import selectors
import shlex
import signal
import subprocess
import tempfile
import time
from xml.sax.saxutils import escape

BUS = "org.laufan.ClipDaemon"
OBJECT = "/org/laufan/ClipDaemon"
INTERFACE = "org.laufan.ClipDaemon2"
DBUS = "org.freedesktop.DBus"
DBUS_OBJECT = "/org/freedesktop/DBus"
GATE = ("packaged-main/engine/rollback acceptance is not enabled: it requires "
        "a separately authorized safe backend/capture fixture; synthetic activation "
        "is not packaged-main acceptance")


class SmokeError(RuntimeError):
    """A failed check, never an invitation to retry activation or a domain effect."""


def require(condition, message):
    if not condition:
        raise SmokeError(message)


def executable(path):
    path = Path(path)
    require(path.is_absolute(), "selected executable must be absolute")
    require(path.is_file() and os.access(path, os.X_OK), "selected executable is missing")
    return path


def ini(path):
    parser = configparser.ConfigParser(interpolation=None)
    parser.optionxform = str
    with path.open() as source:
        parser.read_file(source)
    # systemd/D-Bus definitions have no ConfigParser-style inherited defaults.
    # Accepting these could hide a missing actual Exec/ExecStart directive.
    require(not parser.defaults(), "packaged service definitions cannot inherit INI defaults")
    return parser


def inspect_package(package):
    """Read rendered definitions only. Do not invoke binaries or a user manager."""
    package = Path(package).resolve(strict=True)
    binary = executable(package / "bin/clip-daemon")
    units = package / "share/systemd/user"
    require({p.name for p in units.iterdir()} == {
        "clip-daemon.service", "ringboard-server.service"}, "unexpected packaged units")
    daemon = ini(units / "clip-daemon.service")
    engine = ini(units / "ringboard-server.service")
    require(shlex.split(daemon["Service"]["ExecStart"]) == [str(binary), "daemon"],
            "daemon unit does not select this package")
    require(daemon["Service"]["Type"] == "dbus" and daemon["Service"]["BusName"] == BUS,
            "wrong daemon unit bus contract")
    require(daemon["Unit"]["Conflicts"].split() == ["ringboard-wayland.service"],
            "legacy collector exclusion missing")
    require(daemon["Unit"]["Wants"].split() == ["ringboard-server.service"],
            "engine start relationship changed")
    require("ringboard-server.service" not in daemon["Unit"].get("Requires", "").split(),
            "facade must survive engine restart")
    require(daemon["Unit"]["PartOf"] == "graphical-session.target", "wrong session lifetime")
    require(engine["Service"]["Type"] == "notify", "engine readiness contract changed")
    require(shlex.split(engine["Service"]["ExecStartPre"]) == [str(binary), "configure-engine"],
            "engine policy command does not select this package")
    server = shlex.split(engine["Service"]["ExecStart"])
    require(len(server) == 1 and "@" not in server[0], "unrendered/invalid engine path")
    executable(server[0])
    require("ringboard-wayland.service" not in (units / "ringboard-server.service").read_text(),
            "engine must not start the retired collector")
    services = package / "share/dbus-1/services"
    require({p.name for p in services.iterdir()} == {BUS + ".service"},
            "unexpected packaged activation services")
    service = ini(services / (BUS + ".service"))["D-BUS Service"]
    require(service["Name"] == BUS and service["SystemdService"] == "clip-daemon.service",
            "wrong activation identity")
    require(shlex.split(service["Exec"]) == [str(binary), "daemon"],
            "activation does not select this package")
    return {"package": str(package), "binary": str(binary), "engine": server[0]}


@dataclass(frozen=True)
class Tools:
    dbus_daemon: Path
    busctl: Path
    timeout: Path

    def __post_init__(self):
        executable(self.dbus_daemon)
        executable(self.busctl)
        executable(self.timeout)


def private_environment(root):
    """Allowlist, not a denylist. No loader, helper, bus or compositor inheritance."""
    env = {"LANG": "C", "LC_ALL": "C"}
    for name, relative in {
        "HOME": "home", "XDG_CONFIG_HOME": "config", "XDG_CONFIG_DIRS": "config-dirs",
        "XDG_DATA_HOME": "data", "XDG_DATA_DIRS": "data-dirs", "XDG_STATE_HOME": "state",
        "XDG_CACHE_HOME": "cache", "XDG_RUNTIME_DIR": "run", "TMPDIR": "tmp",
        "PATH": "empty-bin",
    }.items():
        path = root / relative
        path.mkdir(mode=0o700)
        env[name] = str(path)
    env.update(DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + str(root / "run/no-system-bus"),
               WAYLAND_DISPLAY=str(root / "run/no-wayland"))
    return env


class ProcessGroup:
    """Keep our leader unreaped until group cleanup; never adopt a foreign PID.

    Only inspected tools/fixtures that retain their group are supported, not
    arbitrary process-tree containment. Callers must not poll/wait the Popen.
    """
    def __init__(self, argv, env, *, output=False):
        self.closed = False
        self.process = subprocess.Popen(
            list(map(str, argv)), env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE if output else subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, start_new_session=True,
        )

    def exit_code(self):
        require(self.process.returncode is None, "private process leader already reaped")
        try:
            status = os.waitid(os.P_PID, self.process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        except ChildProcessError:
            raise SmokeError("private process leader already reaped") from None
        if status is None:
            return None
        return status.si_status if status.si_code == os.CLD_EXITED else -status.si_status

    def wait_until(self, deadline):
        while True:
            remaining = deadline - time.monotonic()
            require(remaining > 0, "private command deadline exceeded")
            code = self.exit_code()
            if code is not None:
                return code
            time.sleep(min(0.01, remaining))

    def close(self):
        if self.closed:
            return
        self.exit_code()  # Verify ownership without reaping, including external waitpid misuse.
        self.closed = True
        # Even an exited leader reserves its identity until our final wait.
        # Activated children retain this group (covered by the inert fixture).
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(self.process.pid, sig)
            except ProcessLookupError:
                break
            if sig == signal.SIGTERM:
                time.sleep(0.05)
        try:
            self.process.wait(timeout=1)
        except subprocess.TimeoutExpired:
            raise SmokeError("private process cleanup deadline exceeded") from None
        finally:
            if self.process.stdout:
                self.process.stdout.close()


def run_bounded(argv, env, timeout=3, limit=65536):
    """Deadline AND output bound; diagnostics cannot accidentally dump clipboard data."""
    child = ProcessGroup(argv, env, output=True)
    try:
        chunks = bytearray()
        deadline = time.monotonic() + timeout
        with selectors.DefaultSelector() as selector:
            selector.register(child.process.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise SmokeError("private command deadline exceeded")
                if not selector.select(remaining):
                    raise SmokeError("private command deadline exceeded")
                block = os.read(child.process.stdout.fileno(), min(8192, limit + 1 - len(chunks)))
                chunks.extend(block)
                require(len(chunks) <= limit, "private command output limit exceeded")
                if not block:
                    break
        # EOF does not imply exit. Observe under the same deadline without
        # releasing the leader identity before the finally block cleans its group.
        code = child.wait_until(deadline)
        require(code == 0, f"private command failed (exit {code})")
        return bytes(chunks)
    finally:
        child.close()


def activation_exec(argv):
    require(bool(argv), "empty synthetic activation command")
    executable(argv[0])
    require(all(not any(ord(c) < 32 or ord(c) == 127 for c in str(arg)) for arg in argv),
            "control character in activation argument")
    # D-Bus Exec tokenization, not a shell command. Percent is literal to dbus-daemon.
    return " ".join('"' + str(arg).replace("\\", "\\\\").replace('"', '\\"') + '"' for arg in argv)


class PrivateBus:
    """Synthetic-service harness, NOT authority to start a production daemon.

    The caller supplies an inspected inert, bounded fixture command. The CLI never
    constructs this class with the package's activation executable. No systemd
    activation helper or host service directory is loaded.
    """
    def __init__(self, tools, fixture_argv, *, timeout=3):
        self.tools = tools
        self.fixture_exec = activation_exec(fixture_argv)
        self.timeout = timeout
        self.directory = None
        self.child = None
        self.owner = None
        self.activation_attempted = False

    def __enter__(self):
        self.directory = tempfile.TemporaryDirectory(prefix="clip-smoke-")
        self.root = Path(self.directory.name)
        try:
            self.env = private_environment(self.root)
            self.address = "unix:path=" + str(self.root / "run/bus")
            self.env["DBUS_SESSION_BUS_ADDRESS"] = self.address
            services = self.root / "services"
            services.mkdir(mode=0o700)
            (services / (BUS + ".service")).write_text(
                f"[D-BUS Service]\nName={BUS}\nExec={self.fixture_exec}\n")
            config = self.root / "bus.conf"
            config.write_text(
                '<busconfig><type>session</type><auth>EXTERNAL</auth>'
                f'<listen>{escape(self.address)}</listen>'
                f'<servicedir>{escape(str(services))}</servicedir>'
                '<limit name="service_start_timeout">2000</limit>'
                '<policy context="default"><allow send_destination="*"/>'
                '<allow receive_sender="*"/><allow own="*"/></policy></busconfig>')
            # Independent lifetime even if the Python test runner is killed.
            self.child = ProcessGroup([
                self.tools.timeout, "--signal=TERM", "--kill-after=1", "15",
                self.tools.dbus_daemon, "--nofork", "--nopidfile", "--config-file=" + str(config)
            ], self.env)
            deadline = time.monotonic() + self.timeout
            while not (self.root / "run/bus").exists():
                code = self.child.exit_code()
                require(code is None, f"private bus failed to start (exit {code})")
                require(time.monotonic() < deadline, "private bus startup deadline exceeded")
                time.sleep(0.01)
            names = self.bus_call(DBUS, DBUS_OBJECT, DBUS, "ListActivatableNames")
            require(set(names[0]) == {DBUS, BUS}, "unexpected private activation registry")
            require(self.bus_call(DBUS, DBUS_OBJECT, DBUS, "NameHasOwner", "s", BUS) == [False],
                    "synthetic service was already running")
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def bus_call(self, destination, path, interface, member, *args):
        wire = run_bounded([
            self.tools.busctl, "--address=" + self.address, "--auto-start=no",
            "--timeout=" + str(self.timeout), "--json=short", "call",
            destination, path, interface, member, *args,
        ], self.env, timeout=self.timeout)
        return json.loads(wire)["data"]

    def activate(self):
        require(not self.activation_attempted, "activation is single-attempt; no implicit retry")
        self.activation_attempted = True
        self.bus_call(DBUS, DBUS_OBJECT, DBUS, "StartServiceByName", "su", BUS, "0")
        owner = self.bus_call(DBUS, DBUS_OBJECT, DBUS, "GetNameOwner", "s", BUS)[0]
        require(isinstance(owner, str) and owner.startswith(":"), "missing unique owner")
        self.owner = owner
        return owner

    def read_settings(self):
        require(self.owner is not None, "activate the synthetic service first")
        # Bind to the captured unique owner: losing/replacing it cannot re-activate
        # the well-known name or redirect even this read to a different lifetime.
        data = self.bus_call(self.owner, OBJECT, INTERFACE, "Call", "ss",
                             "clipboard.settings.get", "{}")
        require(len(data) == 1 and isinstance(data[0], str), "invalid Call result")
        result = json.loads(data[0])
        require(result.get("ok") is True and isinstance(result.get("data"), dict),
                "settings read failed")
        return result["data"]

    def __exit__(self, *_):
        try:
            if self.child:
                self.child.close()
        finally:
            if self.directory:
                self.directory.cleanup()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", type=Path)
    parser.add_argument("previous", type=Path, nargs="?", help="rollback gate (not enabled)")
    parser.add_argument("--wiring-only", action="store_true", help="static definitions only; no binaries run")
    args = parser.parse_args(argv)
    if not args.wiring_only or args.previous is not None:
        parser.exit(2, "NOT RUN: " + GATE + "\n")
    try:
        identity = inspect_package(args.package)
    except (SmokeError, OSError, KeyError, ValueError, configparser.Error) as error:
        parser.exit(1, f"FAIL package wiring: {error}\n")
    print("PASS static package wiring " + json.dumps(identity, sort_keys=True))
    print("NOT RUN: " + GATE)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
