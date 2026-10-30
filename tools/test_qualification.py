#!/usr/bin/env python3
"""CI-safe isolation and failure-cleanup checks; no native clients or credentials."""
import os
import asyncio
import errno
import importlib.util
from pathlib import Path
import sys
import subprocess
import time
import tempfile
import unittest
import urllib.error
import urllib.request
from unittest.mock import Mock, patch

from qualification_native import Fixture, Processes, Qualification, private_env

probe_spec = importlib.util.spec_from_file_location("codex_native_probe", Path(__file__).with_name("codex-native-delivery.py"))
codex_probe = importlib.util.module_from_spec(probe_spec)
probe_spec.loader.exec_module(codex_probe)


class Isolation(unittest.TestCase):
    def test_codex_probe_exit_observation_does_not_reap_owned_leader(self):
        probe = codex_probe.Probe.__new__(codex_probe.Probe)
        child = Mock(pid=12345)
        probe.children = [("native", child)]
        # Mock the Linux-only observation API so this safety contract is checked
        # on every CI platform without starting a native runtime.
        native_os = Mock(P_PID=1, WEXITED=2, WNOWAIT=4, WNOHANG=8)
        native_os.waitid.return_value = object()
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(codex_probe, "os", native_os):
                with self.assertRaisesRegex(AssertionError, "child exited"):
                    asyncio.run(probe.wait_path(Path(directory) / "missing.sock"))
        native_os.waitid.assert_called_once_with(1, child.pid, 2 | 4 | 8)
        child.poll.assert_not_called()
        child.wait.assert_not_called()

    def test_codex_probe_startup_deadline_does_not_reap_owned_leader(self):
        probe = codex_probe.Probe.__new__(codex_probe.Probe)
        child = Mock(pid=12345)
        probe.children = [("native", child)]
        native_os = Mock(P_PID=1, WEXITED=2, WNOWAIT=4, WNOHANG=8)
        native_os.waitid.return_value = None
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(codex_probe, "os", native_os):
                with self.assertRaises(asyncio.TimeoutError):
                    asyncio.run(probe.wait_path(Path(directory) / "missing.sock", seconds=.01))
        self.assertGreater(native_os.waitid.call_count, 0)
        child.poll.assert_not_called()
        child.wait.assert_not_called()

    def test_codex_probe_provider_start_failure_closes_without_shutdown(self):
        probe = codex_probe.Probe.__new__(codex_probe.Probe)
        server, worker = Mock(), Mock()
        worker.start.side_effect = RuntimeError("injected thread startup failure")
        with patch.object(codex_probe.http.server, "ThreadingHTTPServer", return_value=server), patch.object(codex_probe.threading, "Thread", return_value=worker):
            with self.assertRaisesRegex(RuntimeError, "injected thread startup failure"):
                probe.fixture()
        server.server_close.assert_called_once()
        server.shutdown.assert_not_called()
        self.assertIsNone(probe.server)
        self.assertIsNone(probe.worker)

    def test_codex_probe_cleanup_attempts_all_stages_and_records_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            probe = codex_probe.Probe.__new__(codex_probe.Probe)
            probe.root, probe.current, probe.children, probe.events, probe.cases = Path(directory), None, [], [], []
            probe.processes = Mock(cleanup_errors=["injected process cleanup failure"])
            probe.processes.close.return_value = {}
            probe.server = Mock()
            probe.server.shutdown.side_effect = RuntimeError("injected provider failure")
            probe.worker = Mock()
            probe.worker.is_alive.return_value = False
            probe.socket_identity = None
            with self.assertRaisesRegex(RuntimeError, "cleanup failed"):
                probe.cleanup()
            probe.server.server_close.assert_called_once()
            probe.worker.join.assert_called_once_with(timeout=5)
            import json
            results = json.loads((probe.root / "results.json").read_text())
            self.assertEqual(len(results["cleanup_errors"]), 2)
            self.assertIn("injected process cleanup failure", results["cleanup_errors"])

    def test_darwin_zombie_only_group_permission_error_still_reaps_leader(self):
        processes = Processes(Path("/unused"))
        child = Mock(pid=12345, returncode=None)
        def wait(**kwargs):
            child.returncode = 0
            return 0
        child.wait.side_effect = wait
        with patch("qualification_native.sys.platform", "darwin"), patch("qualification_native.os.killpg", side_effect=[None, PermissionError(errno.EPERM, "zombies")]), patch("qualification_native.time.sleep"), patch.object(processes, "group_members", return_value=[(12345, "Z"), (12346, "Z+")]):
            processes.stop(child)
        child.wait.assert_called_once_with(timeout=5)
        self.assertEqual(child.returncode, 0)
        self.assertIn(child, processes.stopped)

    def test_darwin_live_member_permission_error_fails_and_reaps_leader(self):
        processes = Processes(Path("/unused"))
        child = Mock(pid=12345, returncode=None)
        child.wait.side_effect = lambda **kwargs: setattr(child, "returncode", 0)
        processes.children = [("denied", child)]
        with patch("qualification_native.sys.platform", "darwin"), patch("qualification_native.os.killpg", side_effect=PermissionError(errno.EPERM, "live member")), patch("qualification_native.time.sleep"), patch.object(processes, "group_members", return_value=[(12345, "Z"), (12346, "S")]):
            processes.close()
        child.wait.assert_called_once_with(timeout=5)
        self.assertEqual(child.returncode, 0)
        self.assertTrue(processes.cleanup_errors)
        self.assertIn("denied: pid=12345 signal=SIGTERM PermissionError errno=1", processes.cleanup_errors[0])
        self.assertIn("signal=SIGKILL", processes.cleanup_errors[0])

    def test_unverified_darwin_permission_error_fails_closed_and_reaps(self):
        processes = Processes(Path("/unused"))
        child = Mock(pid=12345, returncode=None)
        child.wait.side_effect = lambda **kwargs: setattr(child, "returncode", 0)
        with patch("qualification_native.sys.platform", "darwin"), patch("qualification_native.os.killpg", side_effect=PermissionError(errno.EPERM, "unknown")), patch("qualification_native.time.sleep"), patch.object(processes, "group_members", side_effect=RuntimeError("ps unavailable")):
            with self.assertRaisesRegex(RuntimeError, "membership: ps unavailable"):
                processes.stop(child)
        child.wait.assert_called_once_with(timeout=5)
        self.assertEqual(child.returncode, 0)

    def test_filesystem_startup_failures_do_not_start_a_fixture_thread(self):
        with patch("qualification_native.tempfile.mkdtemp", side_effect=OSError("quota")), patch("qualification_native.Fixture") as fixture:
            with self.assertRaises(OSError):
                Qualification(None, None, {})
            fixture.assert_not_called()
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory) / "work"
            work.mkdir()
            with patch("qualification_native.tempfile.mkdtemp", return_value=str(work)), patch.object(Path, "chmod", side_effect=OSError("chmod failed")), patch("qualification_native.Fixture") as fixture:
                with self.assertRaises(OSError):
                    Qualification(None, None, {})
                fixture.assert_not_called()
            self.assertFalse(work.exists())

    def test_exited_leader_does_not_leave_its_native_descendant_running(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            processes = Processes(root)
            pidfile = root / "descendant"
            code = "import subprocess,sys,pathlib; child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)']);pathlib.Path(sys.argv[1]).write_text(str(child.pid))"
            parent = processes.start("leader", [sys.executable, "-c", code, str(pidfile)], {"PATH": "/usr/bin:/bin"}, root)
            try:
                deadline = time.monotonic() + 5
                while not pidfile.exists() and time.monotonic() < deadline:
                    time.sleep(.05)
                self.assertTrue(pidfile.exists())
                # Deliberately do not poll/reap the exited leader before cleanup.
                time.sleep(.2)
            finally:
                processes.close()
            self.assertFalse(processes.cleanup_errors, processes.cleanup_errors)
            self.assertEqual(parent.returncode, 0)
            status = subprocess.run(["ps", "-o", "stat=", "-p", str(int(pidfile.read_text()))], capture_output=True, text=True, check=False).stdout.strip()
            self.assertTrue(not status or status.startswith("Z"), f"Owned descendant still running: {status}")
            processes.close()  # idempotent; no signaling of an already reaped group

    def test_teardown_failure_is_recorded_and_cannot_produce_a_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            processes = Processes(root)
            processes.children = [("later", object()), ("first", object())]
            with patch.object(processes, "stop", side_effect=[RuntimeError("injected stop failure"), None]) as stop:
                # Fake returncodes avoid starting anything for this failure path.
                processes.children = [(name, type("Child", (), {"returncode": 0})()) for name, _ in processes.children]
                processes.close()
            self.assertEqual(stop.call_count, 2)
            self.assertEqual(processes.cleanup_errors, ["first: injected stop failure"])
            harness = Qualification.__new__(Qualification)
            harness.evidence = {"result": "passed"}
            harness.fixture = Fixture()
            harness.processes = Processes(root)
            harness.processes.cleanup_errors.append("injected cleanup failure")
            harness.codex_socket = None
            harness.workroot = root / "work"
            harness.workroot.mkdir()
            asyncio.run(harness.close())
            self.assertEqual(harness.evidence["result"], "failed")
            self.assertFalse(harness.workroot.exists())
            self.assertFalse(harness.fixture.thread.is_alive())

    def test_sender_references_cannot_select_the_wrong_native_fixture_route(self):
        fixture = Fixture()
        try:
            fixture.active = {"marker": "MARKER", "sender": "opencode-source", "reply_id": "reply"}
            fixture.routes = {"opencode-source": {"provider": "opencode", "state": Path("/unused")},
                              "claude-dest": {"provider": "claude", "state": Path("/unused")},
                              "opencode-dest": {"provider": "opencode", "state": Path("/unused")}}
            for path, route, systems in [("/claude/claude-dest/v1/messages", "claude-dest", ""),
                                        ("/opencode/v1/chat/completions", "opencode-dest", "Your identity is opencode-dest")]:
                body = {"messages": [{"role": "system", "content": systems},
                                     {"role": "user", "content": "MARKER from opencode-source"}],
                        "tools": [{"name": "bridge_acknowledge_message"}]}
                record = {"body": "MARKER", "recipient": route, "sender": "opencode-source", "id": "incoming"}
                with patch("qualification_native.rpc", return_value=[record]):
                    action = fixture.action(path, body)
                self.assertEqual(action["arguments"], {"agent": route, "id": "incoming"})
        finally:
            fixture.close()

    def test_owner_credentials_and_native_configuration_are_not_inherited(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.dict(os.environ, {"ANTHROPIC_API_KEY": "owner-secret", "OPENAI_API_KEY": "owner-secret", "OPENCODE_CONFIG": "/owner/config", "CODEX_HOME": "/owner/codex", "NODE_OPTIONS": "--require owner-code", "HTTP_PROXY": "http://owner-proxy"}):
                env = private_env(root)
            for key in ("ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OPENCODE_CONFIG", "NODE_OPTIONS", "HTTP_PROXY"):
                self.assertNotIn(key, env)
            for key in ("HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR", "XDG_CONFIG_HOME"):
                self.assertTrue(Path(env[key]).is_relative_to(root))

    def test_failure_teardown_awaits_owned_process_and_http_server(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            processes = Processes(root)
            fixture = Fixture()
            child = processes.start("waiting", [sys.executable, "-c", "import time;time.sleep(60)"], {"PATH": "/usr/bin:/bin"}, root)
            try:
                # A malformed provider request must fail, never manufacture a pass.
                request = urllib.request.Request(fixture.origin + "/claude/test/v1/messages", data=b'{}')
                with self.assertRaises(urllib.error.HTTPError) as failure:
                    urllib.request.urlopen(request, timeout=3)
                failure.exception.close()
                self.assertTrue(fixture.errors)
            finally:
                exits = processes.close()
                fixture.close()
            self.assertIsNotNone(child.poll())
            self.assertNotEqual(exits["waiting"], 0)
            self.assertFalse(fixture.thread.is_alive())


if __name__ == "__main__":
    unittest.main()
