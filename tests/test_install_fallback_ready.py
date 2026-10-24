"""Installer regression tests. Never invoke launchctl or change a real service."""
import contextlib
import importlib.util
import io
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, call, patch


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/install-fallback-ready.py"
SPEC = importlib.util.spec_from_file_location("fallback_installer", SCRIPT)
installer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(installer)
SERVICE = "gui/501/local.example.codex-free"


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.config = root / "config.json"
        self.binary = root / "codex-free"
        self.backup = root / "codex-free.backup"
        self.staging = root / "stage"
        self.config.write_bytes(b"old-config")
        self.binary.write_bytes(b"old-binary")
        self.backup.write_bytes(b"old-binary")
        self.staging.write_bytes(b"new-binary")
        # A missed mock fails instead of ever managing a real system process.
        self.run_patch = patch.object(installer, "run", side_effect=AssertionError("Unexpected system command"))
        self.run_patch.start()
        self.addCleanup(self.run_patch.stop)

    def activate(self):
        with contextlib.redirect_stdout(io.StringIO()):
            return installer.activate_update(
                SERVICE, 3000, self.config, b"new-config", b"old-config",
                str(self.staging), self.binary, self.backup, 100,
            )

    def assert_original_restored(self):
        self.assertEqual(self.config.read_bytes(), b"old-config")
        self.assertEqual(self.binary.read_bytes(), b"old-binary")
        self.assertEqual(self.backup.read_bytes(), b"old-binary")

    def test_restart_keeps_service_registered(self):
        with patch.object(installer, "run", return_value=subprocess.CompletedProcess([], 0, "", "")) as run, patch.object(installer,"service_pid",side_effect=[100,None,None]):
            installer.restart_registered(SERVICE)
        self.assertEqual(run.call_args_list,[call("/bin/launchctl","kill","SIGTERM",SERVICE),call("/bin/launchctl","kickstart",SERVICE)])

    def test_keepalive_replacement_is_not_killed_again(self):
        with patch.object(installer,"run",return_value=subprocess.CompletedProcess([],0,"","")) as run, patch.object(installer,"service_pid",side_effect=[100,200,200]):
            installer.restart_registered(SERVICE)
        run.assert_called_once_with("/bin/launchctl","kill","SIGTERM",SERVICE)

    def test_stalled_drain_never_escalates_to_forced_restart(self):
        with patch.object(installer,"run",return_value=subprocess.CompletedProcess([],0,"","")) as run, patch.object(installer,"service_pid",return_value=100), patch.object(installer.time,"monotonic",side_effect=[0,161]):
            with self.assertRaisesRegex(RuntimeError,"refusing a forced restart"):
                installer.restart_registered(SERVICE)
        run.assert_called_once_with("/bin/launchctl","kill","SIGTERM",SERVICE)

    def test_restart_preserves_diagnostic(self):
        result = subprocess.CompletedProcess([], 5, "", "Input/output error")
        with patch.object(installer, "run", return_value=result), patch.object(installer,"service_pid",return_value=None):
            with self.assertRaisesRegex(RuntimeError, "exit 5.*Input/output error"):
                installer.restart_registered(SERVICE)

    def test_parse_pid_without_dumping_environment(self):
        result = subprocess.CompletedProcess([], 0, "service = {\n state = running\n pid = 4321\n}", "")
        with patch.object(installer, "run", return_value=result):
            self.assertEqual(installer.service_pid(SERVICE), 4321)

    def test_service_without_pid_is_not_running(self):
        with patch.object(installer, "run", return_value=subprocess.CompletedProcess([], 0, "state = waiting", "")):
            self.assertIsNone(installer.service_pid(SERVICE))

    def test_success_installs_and_waits_for_new_process(self):
        with patch.object(installer, "restart_registered") as restart, patch.object(installer, "wait_healthy", return_value=200) as health:
            self.assertEqual(self.activate(), 200)
        restart.assert_called_once_with(SERVICE)
        health.assert_called_once_with(SERVICE, 3000, old_pid=100)
        self.assertEqual(self.config.read_bytes(), b"new-config")
        self.assertEqual(self.binary.read_bytes(), b"new-binary")
        self.assertFalse(self.staging.exists())

    def test_restart_failure_restores_files_and_verifies_old_service(self):
        with patch.object(installer, "restart_registered", side_effect=[RuntimeError("restart denied"), None]) as restart, patch.object(installer, "service_pid", return_value=101), patch.object(installer, "wait_healthy", return_value=202) as health:
            with self.assertRaisesRegex(RuntimeError, "original service restored.*restart denied"):
                self.activate()
        self.assertEqual(restart.call_count, 2)
        health.assert_called_once_with(SERVICE, 3000, old_pid=101, timeout=90)
        self.assert_original_restored()

    def test_health_failure_also_rolls_back(self):
        with patch.object(installer, "restart_registered"), patch.object(installer, "service_pid", return_value=201), patch.object(installer, "wait_healthy", side_effect=[RuntimeError("new health timeout"), 202]) as health:
            with self.assertRaisesRegex(RuntimeError, "original service restored.*new health timeout"):
                self.activate()
        self.assertEqual(health.call_count, 2)
        self.assert_original_restored()

    def test_recovery_failure_is_not_reported_as_success(self):
        with patch.object(installer, "restart_registered", side_effect=[RuntimeError("first failure"), RuntimeError("recovery denied")]), patch.object(installer, "service_pid", return_value=101):
            with self.assertRaisesRegex(RuntimeError, "first failure; rollback/recovery failed: recovery denied"):
                self.activate()
        self.assert_original_restored()

    def test_health_rejects_old_pid_even_when_endpoint_looks_ready(self):
        opener = Mock()
        opener.open.return_value = io.BytesIO(b'{"status":"ok"}')
        with patch.object(installer, "service_pid", side_effect=[100, 200, 200]), patch.object(installer.urllib.request, "build_opener", return_value=opener), patch.object(installer.time, "sleep") as sleep:
            self.assertEqual(installer.wait_healthy(SERVICE, 3000, old_pid=100), 200)
        self.assertEqual(opener.open.call_count, 1)
        sleep.assert_called_once_with(1)

    def test_health_rechecks_process_after_request(self):
        opener = Mock()
        opener.open.side_effect = [io.BytesIO(b'{"status":"ok"}'), io.BytesIO(b'{"status":"ok"}')]
        with patch.object(installer, "service_pid", side_effect=[200, 201, 201, 201]), patch.object(installer.urllib.request, "build_opener", return_value=opener), patch.object(installer.time, "sleep"):
            self.assertEqual(installer.wait_healthy(SERVICE, 3000, old_pid=100), 201)
        self.assertEqual(opener.open.call_count, 2)

    def test_timeout_has_a_terminal_error(self):
        with patch.object(installer.urllib.request, "build_opener"):
            with self.assertRaisesRegex(RuntimeError, "Local health timed out"):
                installer.wait_healthy(SERVICE, 3000, timeout=0)


if __name__ == "__main__":
    unittest.main()
