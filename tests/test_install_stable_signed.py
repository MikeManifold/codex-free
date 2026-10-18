"""Exercise signature-only deployment recovery without a real service."""
import importlib.util
import io
import json
import os
import plistlib
import sys
import tempfile
import unittest
from argparse import Namespace
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location("stable_installer", SCRIPTS / "install-stable-signed.py")
installer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(installer)


class StableInstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.destination = self.home / ".local/opt/codex-free/codex-free"
        self.destination.parent.mkdir(parents=True)
        self.destination.write_bytes(b"old signed executable")
        self.source = self.home / "release"
        self.source.write_bytes(b"different new executable")
        os.chmod(self.source, 0o755)
        self.plist = self.home / "Library/LaunchAgents/local.example.codex-free.plist"
        self.plist.parent.mkdir(parents=True)
        self.plist.write_bytes(plistlib.dumps({"Label": "local.example.codex-free",
                                             "ProgramArguments": [str(self.destination)]}))
        self.config = self.home / ".codex-free/codex.config.json"
        self.config.parent.mkdir(parents=True)
        self.config.write_bytes(b'{"port":3000,"exec":{"mode":"restricted"}}')
        self.helpers = Mock()
        self.helpers.option.return_value = None
        self.helpers.service_pid.return_value = 100
        self.helpers.wait_healthy.return_value = 200
        self.args = Namespace(source=self.source, check=False, migrate_from_adhoc=False)
        for target, kwargs in [
            ("service_helpers", {"return_value": self.helpers}),
            ("identity", {"return_value": {"identifier": "local.example.codex-free"}}),
            ("check_current", {"return_value": ('identifier "old"', True)}),
            ("sign_staged", {"return_value": ('identifier "stable"', True)}),
            ("verify", {}),
        ]:
            patcher = patch.object(installer, target, **kwargs)
            patcher.start()
            self.addCleanup(patcher.stop)
        patcher = patch.object(installer.Path, "home", return_value=self.home)
        patcher.start()
        self.addCleanup(patcher.stop)

    def install(self):
        with redirect_stdout(io.StringIO()):
            installer.install(self.args)

    def test_success_preserves_config_and_launchagent(self):
        config, plist = self.config.read_bytes(), self.plist.read_bytes()
        self.install()
        self.assertEqual(self.destination.read_bytes(), self.source.read_bytes())
        self.assertEqual(self.config.read_bytes(), config)
        self.assertEqual(self.plist.read_bytes(), plist)
        self.helpers.restart_registered.assert_called_once()
        self.helpers.wait_healthy.assert_called_once_with(
            f"gui/{os.getuid()}/local.example.codex-free", 3000, old_pid=100)

    def test_rejected_signature_never_replaces_or_restarts(self):
        installer.sign_staged.side_effect = RuntimeError("previous DR failed")
        with self.assertRaisesRegex(RuntimeError, "previous DR failed"):
            self.install()
        self.assertEqual(self.destination.read_bytes(), b"old signed executable")
        self.helpers.restart_registered.assert_not_called()

    def test_failed_health_restores_old_binary_and_verifies_recovery(self):
        self.helpers.wait_healthy.side_effect = [RuntimeError("new health failed"), 201]
        with self.assertRaisesRegex(RuntimeError, "new health failed"):
            self.install()
        self.assertEqual(self.destination.read_bytes(), b"old signed executable")
        self.assertEqual(self.helpers.restart_registered.call_count, 2)
        installer.verify.assert_called_with(self.destination, 'identifier "old"')

    def test_incompatible_feature_update_is_not_an_initial_migration(self):
        installer.check_current.return_value = ('cdhash H"123"', False)
        self.args.migrate_from_adhoc = True
        with self.assertRaisesRegex(RuntimeError, "without a feature update"):
            self.install()
        installer.sign_staged.assert_not_called()
        self.helpers.restart_registered.assert_not_called()

    def test_check_is_read_only(self):
        self.args.check = True
        self.install()
        self.assertFalse(list(self.destination.parent.glob("*.backup*")))
        installer.sign_staged.assert_not_called()
        self.helpers.restart_registered.assert_not_called()


if __name__ == "__main__":
    unittest.main()
