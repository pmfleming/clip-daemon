#!/usr/bin/env python3
"""Run with unittest; native cases require CLIP_PACKAGE_NATIVE=1 in the dev shell."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("package_smoke", ROOT / "scripts/package-smoke.py")
smoke = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = smoke
spec.loader.exec_module(smoke)
FIXTURE = ROOT / "test_support/package_smoke_service.py"


class ConstructionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="clip-package-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def executable(self, name, source="raise SystemExit(99)\n"):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!" + sys.executable + "\n" + source)
        path.chmod(0o700)
        return path

    def package(self):
        package = self.root / "package"
        binary = self.executable("package/bin/clip-daemon")
        server = self.executable("engine/bin/ringboard-server")
        units = package / "share/systemd/user"
        units.mkdir(parents=True)
        for source in (ROOT / "packaging/systemd").glob("*.service"):
            (units / source.name).write_text(source.read_text().replace("@out@", str(package))
                                            .replace("@ringboard@", str(server.parent.parent)))
        service = package / "share/dbus-1/services/org.laufan.ClipDaemon.service"
        service.parent.mkdir(parents=True)
        service.write_text((ROOT / "packaging/dbus" / service.name).read_text().replace("@out@", str(package)))
        return package, binary, service

    def test_package_wiring_is_static_and_matches_selected_binary(self):
        package, binary, _ = self.package()
        with patch.object(smoke.subprocess, "Popen", side_effect=AssertionError("must not execute")):
            self.assertEqual(smoke.inspect_package(package)["binary"], str(binary))
            with contextlib.redirect_stdout(io.StringIO()) as output:
                self.assertEqual(smoke.main([str(package), "--wiring-only"]), 0)
        self.assertIn("PASS static package wiring", output.getvalue())
        self.assertIn("NOT RUN: packaged-main", output.getvalue())

    def test_old_invocation_and_rollback_are_explicit_gates(self):
        with patch.object(smoke.subprocess, "Popen", side_effect=AssertionError("must not execute")):
            for args in (["/not-used"], ["/not-used", "/old", "--wiring-only"]):
                with contextlib.redirect_stderr(io.StringIO()) as error:
                    with self.assertRaises(SystemExit) as raised:
                        smoke.main(args)
                self.assertEqual(raised.exception.code, 2)
                self.assertIn("synthetic activation is not packaged-main acceptance", error.getvalue())

    def test_wrong_activation_binary_rejected(self):
        package, _, service = self.package()
        service.write_text(service.read_text().replace(str(package), "/wrong-package"))
        with self.assertRaisesRegex(smoke.SmokeError, "activation does not select"):
            smoke.inspect_package(package)

    def test_wrong_unit_binary_rejected(self):
        package, _, _ = self.package()
        unit = package / "share/systemd/user/clip-daemon.service"
        unit.write_text(unit.read_text().replace(str(package), "/wrong-package"))
        with self.assertRaisesRegex(smoke.SmokeError, "unit does not select"):
            smoke.inspect_package(package)

    def test_inherited_ini_defaults_cannot_supply_unit_commands(self):
        package, binary, _ = self.package()
        unit = package / "share/systemd/user/clip-daemon.service"
        directive = f"ExecStart={binary} daemon\n"
        definition = unit.read_text()
        self.assertIn(directive, definition)
        unit.write_text("[DEFAULT]\n" + directive + definition.replace(directive, ""))
        with self.assertRaisesRegex(smoke.SmokeError, "cannot inherit INI defaults"):
            smoke.inspect_package(package)

    def test_extra_activation_service_rejected(self):
        package, _, service = self.package()
        (service.parent / "host.service").write_text("must never be loaded")
        with self.assertRaisesRegex(smoke.SmokeError, "unexpected packaged activation"):
            smoke.inspect_package(package)

    def test_unrendered_engine_rejected(self):
        package, _, _ = self.package()
        unit = package / "share/systemd/user/ringboard-server.service"
        unit.write_text(unit.read_text().replace(str(self.root / "engine"), "@ringboard@"))
        with self.assertRaisesRegex(smoke.SmokeError, "unrendered"):
            smoke.inspect_package(package)

    def test_environment_is_an_allowlist_even_with_old_recursion_marker(self):
        contaminated = {"HOME": "/host", "PATH": "/host/bin", "LD_PRELOAD": "/host/evil.so",
                        "DBUS_SESSION_BUS_ADDRESS": "unix:path=/host/bus", "DBUS_STARTER_ADDRESS": "/host",
                        "DBUS_SYSTEM_BUS_ADDRESS": "unix:path=/host/system", "DISPLAY": ":0",
                        "WAYLAND_SOCKET": "3", "WAYLAND_DISPLAY": "/host/wayland",
                        "XDG_DATA_DIRS": "/host/services", "XDG_CONFIG_DIRS": "/host/config",
                        "HYPRLAND_INSTANCE_SIGNATURE": "host", "SWAYSOCK": "/host/sway",
                        "RINGBOARD_SOCK": "/host/ring", "NOTIFY_SOCKET": "/host/notify",
                        "LISTEN_FDS": "3", "PYTHONPATH": "/host/python", "_CLIP_PACKAGE_SMOKE": "1"}
        with patch.dict(os.environ, contaminated):
            env = smoke.private_environment(self.root)
        self.assertNotIn("DBUS_SESSION_BUS_ADDRESS", env)
        for name, value in env.items():
            self.assertNotIn("/host", value, name)
            if name.startswith("XDG_") or name in ("HOME", "PATH", "TMPDIR"):
                self.assertTrue(Path(value).is_relative_to(self.root))
                self.assertEqual(Path(value).stat().st_mode & 0o777, 0o700)
        for name in contaminated.keys() - env.keys():
            self.assertNotIn(name, env)

    def test_activation_arguments_are_tokens_not_shell_commands(self):
        binary = self.executable("inert executable")
        args = [binary, 'space quote" slash\\', "$(touch /must-not-exist);%x"]
        self.assertEqual(shlex_split(smoke.activation_exec(args)), list(map(str, args)))
        with self.assertRaisesRegex(smoke.SmokeError, "control character"):
            smoke.activation_exec([binary, "bad\nExec=evil"])
        with self.assertRaises(smoke.SmokeError):
            smoke.activation_exec(["relative"])

    def test_selected_command_and_environment_ignore_inherited_path(self):
        binary = self.executable("selected", "import json, os, sys\nprint(json.dumps([sys.argv[1:], dict(os.environ)]))\n")
        env = smoke.private_environment(self.root)
        with patch.dict(os.environ, {"PATH": "/host/bin", "DBUS_SESSION_BUS_ADDRESS": "/host/bus"}):
            args, actual = json.loads(smoke.run_bounded([binary, "literal ; $(bad)"], env))
        self.assertEqual(args, ["literal ; $(bad)"])
        self.assertNotIn("DBUS_SESSION_BUS_ADDRESS", actual)
        self.assertEqual(actual["PATH"], env["PATH"])

    def test_completed_command_keeps_leader_identity_until_idempotent_cleanup(self):
        binary = self.executable("completed", 'print("complete")\n')
        env = smoke.private_environment(self.root)
        original_close = smoke.ProcessGroup.close
        observed = []

        def close(group):
            observed.append(group.process.returncode)
            self.assertIsNone(group.process.returncode, "must not reap before group cleanup")
            original_close(group)
            with patch.object(smoke.os, "killpg", side_effect=AssertionError("already closed")):
                original_close(group)

        with patch.object(smoke.ProcessGroup, "close", close):
            self.assertEqual(smoke.run_bounded([binary], env), b"complete\n")
        self.assertEqual(observed, [None])

    def test_externally_reaped_leader_never_authorizes_group_signal(self):
        binary = self.executable("inert", "pass\n")
        env = smoke.private_environment(self.root)
        for reaper in ("popen", "waitpid"):
            with self.subTest(reaper=reaper):
                group = smoke.ProcessGroup([binary], env)
                # Deliberate misuse; these inert children have no descendants.
                if reaper == "popen":
                    group.process.wait(timeout=3)
                else:
                    os.waitpid(group.process.pid, 0)
                with patch.object(smoke.os, "killpg") as kill:
                    with self.assertRaisesRegex(smoke.SmokeError, "already reaped"):
                        group.close()
                kill.assert_not_called()

    def test_output_and_startup_deadlines(self):
        flood = self.executable("flood", "import os\nos.write(1, b'x' * 4096)\n")
        slow = self.executable("slow", "import time\ntime.sleep(5)\n")
        closed_output = self.executable(
            "closed-output", "import os, signal, time\nsignal.alarm(2)\nos.close(1)\ntime.sleep(2)\n")
        fail = self.executable("fail", "raise SystemExit(7)\n")
        env = smoke.private_environment(self.root)
        with self.assertRaisesRegex(smoke.SmokeError, "output limit"):
            smoke.run_bounded([flood], env, limit=128)
        with self.assertRaisesRegex(smoke.SmokeError, "deadline"):
            smoke.run_bounded([slow], env, timeout=0.1)
        with self.assertRaisesRegex(smoke.SmokeError, "deadline"):
            smoke.run_bounded([closed_output], env, timeout=0.1)
        with self.assertRaisesRegex(smoke.SmokeError, "exit 7"):
            smoke.run_bounded([fail], env)

    def test_fixture_address_allows_only_its_private_path_and_optional_bus_guid(self):
        fixture_spec = importlib.util.spec_from_file_location("smoke_service_fixture", FIXTURE)
        fixture = importlib.util.module_from_spec(fixture_spec)
        fixture_spec.loader.exec_module(fixture)
        runtime = self.root / "run"
        address = "unix:path=" + str(runtime / "bus")
        for candidate in (address, address + ",guid=" + "a" * 32):
            self.assertTrue(fixture.private_address(candidate, runtime))
        for suffix in (",guid=bad", ",guid=" + "a" * 32 + ";unix:path=/host/bus", ",other=host"):
            self.assertFalse(fixture.private_address(address + suffix, runtime))
        self.assertFalse(fixture.private_address("unix:path=/host/bus", runtime))

    def test_service_fixture_rejects_non_smoke_runtime_before_import_or_connection(self):
        env = smoke.private_environment(self.root)
        env["DBUS_SESSION_BUS_ADDRESS"] = "unix:path=" + str(Path(env["XDG_RUNTIME_DIR"]) / "bus")
        audit = self.root / "must-not-create-audit"
        with self.assertRaises(smoke.SmokeError):
            smoke.run_bounded([sys.executable, FIXTURE, audit], env)
        self.assertFalse(audit.exists())

    def test_failed_bus_start_cleans_private_directory(self):
        fail = self.executable("fail", "raise SystemExit(7)\n")
        bus = smoke.PrivateBus(smoke.Tools(fail, fail, fail), [fail], timeout=0.1)
        with self.assertRaisesRegex(smoke.SmokeError, "private bus failed|startup deadline"):
            with bus:
                self.fail("failed bus must not become ready")
        self.assertFalse(bus.root.exists())
        self.assertIsNotNone(bus.child.process.returncode)

    def test_selected_multicall_tool_keeps_its_invocation_name(self):
        dispatcher = self.executable("multicall", "import os, sys\nprint(os.path.basename(sys.argv[0]))\n")
        selected = self.root / "timeout"
        selected.symlink_to(dispatcher)
        tools = smoke.Tools(dispatcher, dispatcher, selected)
        self.assertEqual(tools.timeout, selected)
        self.assertEqual(smoke.run_bounded([tools.timeout], {}).strip(), b"timeout")

    def test_interface_tracks_the_current_producer_export(self):
        source = (ROOT / "src/daemon.rs").read_text()
        self.assertIn(f'pub const INTERFACE: &str = "{smoke.INTERFACE}";', source)
        self.assertIn(f'#[zbus::interface(name = "{smoke.INTERFACE}")]', source)

    def test_call_uses_v2_unique_owner_and_never_falls_back(self):
        binary = self.executable("inert")
        bus = smoke.PrivateBus(smoke.Tools(binary, binary, binary), [binary])
        bus.owner = ":1.8"
        with patch.object(bus, "bus_call", return_value=[json.dumps({"ok": True, "data": {}})]) as call:
            self.assertEqual(bus.read_settings(), {})
        self.assertEqual(call.call_args.args, (":1.8", smoke.OBJECT, "org.laufan.ClipDaemon2", "Call", "ss",
                                              "clipboard.settings.get", "{}"))
        with patch.object(bus, "bus_call", side_effect=smoke.SmokeError("wrong interface")) as call:
            with self.assertRaisesRegex(smoke.SmokeError, "wrong interface"):
                bus.read_settings()
        self.assertEqual(call.call_count, 1)


def shlex_split(value):
    import shlex
    return shlex.split(value)


def alive(pid):
    try:
        # A killed orphan can briefly be a zombie until the private bus reaps it.
        return Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1][0] != "Z"
    except FileNotFoundError:
        return False


@unittest.skipUnless(os.environ.get("CLIP_PACKAGE_NATIVE") == "1", "explicit private native fixture only")
class PrivateActivationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="clip-activation-test-")
        self.addCleanup(self.directory.cleanup)
        self.audit = Path(self.directory.name) / "audit.jsonl"
        # Resolve exactly once in the matching dev shell, never in child envs.
        self.tools = smoke.Tools(Path(shutil.which("dbus-daemon")).resolve(),
                                 Path(shutil.which("busctl")).resolve(),
                                 Path(shutil.which("timeout")))
        self.pids = []
        self.addCleanup(self.assert_children_stopped)

    def rows(self):
        rows = [json.loads(line) for line in self.audit.read_text().splitlines()] if self.audit.exists() else []
        self.pids = list({row["pid"] for row in rows})
        return rows

    def assert_children_stopped(self):
        self.rows()
        deadline = time.monotonic() + 2
        while any(alive(pid) for pid in self.pids) and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertFalse([pid for pid in self.pids if alive(pid)], "private activation child leaked")

    def bus(self, mode="v2"):
        return smoke.PrivateBus(self.tools, [Path(sys.executable), FIXTURE, self.audit, "--mode", mode])

    def test_native_v2_activation_is_private_and_cleanup_owns_the_group(self):
        inherited_share = Path(self.directory.name) / "inherited-share"
        inherited_services = inherited_share / "dbus-1/services"
        inherited_services.mkdir(parents=True)
        # Existing, synthetic host-like registration: discovery must not include it.
        (inherited_services / "org.example.ContaminationProbe.service").write_text(
            "[D-BUS Service]\nName=org.example.ContaminationProbe\nExec=/must-not-execute\n")
        with patch.dict(os.environ, {"DBUS_SESSION_BUS_ADDRESS": "unix:path=/host/bus",
                                    "XDG_DATA_DIRS": str(inherited_share), "_CLIP_PACKAGE_SMOKE": "1"}):
            with self.bus() as bus:
                with self.assertRaises(smoke.SmokeError):
                    bus.bus_call(smoke.DBUS, smoke.DBUS_OBJECT, smoke.DBUS, "StartServiceByName", "su",
                                 "org.example.ContaminationProbe", "0")
                owner = bus.activate()
                self.assertTrue(owner.startswith(":"))
                self.assertEqual(bus.read_settings(), {"synthetic_fixture": True})
                rows = self.rows()
                self.assertEqual([row["kind"] for row in rows], ["start", "read"])
                self.assertEqual(rows[0]["pgid"], bus.child.process.pid)
                owner_pid = bus.bus_call(smoke.DBUS, smoke.DBUS_OBJECT, smoke.DBUS,
                                         "GetConnectionUnixProcessID", "s", owner)[0]
                self.assertEqual(owner_pid, rows[0]["pid"])
                # Nix's selected Python launcher can be a native wrapper; compare
                # the kernel executable to this matching interpreter, not its wrapper.
                self.assertEqual(Path(f"/proc/{owner_pid}/exe").resolve(), Path("/proc/self/exe").resolve())
                self.assertEqual(rows[0]["env"]["DBUS_SESSION_BUS_ADDRESS"].split(",guid=", 1)[0], bus.address)
                self.assertNotIn("_CLIP_PACKAGE_SMOKE", rows[0]["env"])
                self.assertNotIn("/host", json.dumps(rows))
                config = (bus.root / "bus.conf").read_text()
                self.assertNotIn("standard_session_servicedirs", config)
                self.assertNotIn("servicehelper", config)
                private_root = bus.root
        self.assertFalse(private_root.exists())

    def test_native_wrong_interface_is_rejected_without_legacy_fallback(self):
        with self.bus("v1") as bus:
            bus.activate()
            with self.assertRaises(smoke.SmokeError):
                bus.read_settings()
            self.assertEqual([row["kind"] for row in self.rows()], ["start"])

    def test_native_failed_activation_and_missing_name_have_no_retry(self):
        for mode in ("fail", "no-name"):
            with self.subTest(mode=mode):
                self.audit.unlink(missing_ok=True)
                with self.bus(mode) as bus:
                    with self.assertRaises(smoke.SmokeError):
                        bus.activate()
                    with self.assertRaisesRegex(smoke.SmokeError, "single-attempt"):
                        bus.activate()
                    self.assertEqual([row["kind"] for row in self.rows()], ["start"])
                self.assert_children_stopped()

    def test_native_retired_owner_read_does_not_activate_or_follow_replacement(self):
        with self.bus("exit-after-read") as bus:
            owner = bus.activate()
            self.assertEqual(bus.read_settings(), {"synthetic_fixture": True})
            deadline = time.monotonic() + 2
            while bus.bus_call(smoke.DBUS, smoke.DBUS_OBJECT, smoke.DBUS, "NameHasOwner", "s", smoke.BUS) != [False]:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.01)
            with self.assertRaises(smoke.SmokeError):
                bus.read_settings()
            self.assertEqual([row["kind"] for row in self.rows()], ["start", "read"])
            replacement = smoke.ProcessGroup([sys.executable, FIXTURE, self.audit], bus.env)
            try:
                deadline = time.monotonic() + 2
                while bus.bus_call(smoke.DBUS, smoke.DBUS_OBJECT, smoke.DBUS, "NameHasOwner", "s", smoke.BUS) != [True]:
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(0.01)
                new_owner = bus.bus_call(smoke.DBUS, smoke.DBUS_OBJECT, smoke.DBUS, "GetNameOwner", "s", smoke.BUS)[0]
                self.assertNotEqual(owner, new_owner)
                with self.assertRaises(smoke.SmokeError):
                    bus.read_settings()
                self.assertIsNone(replacement.exit_code())
                self.assertEqual([row["kind"] for row in self.rows()], ["start", "read", "start"])
            finally:
                replacement.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
