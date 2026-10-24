"""First-install boundaries and rollback; never access real keys or launchd."""
import argparse
import base64
import contextlib
import hashlib
import importlib.util
import io
import json
import plistlib
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, call, patch


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/bootstrap-macos.py"
SPEC = importlib.util.spec_from_file_location("macos_bootstrap", SCRIPT)
bootstrap = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bootstrap)


class BootstrapTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.workspace = self.root / "workspace with spaces 项目"
        self.workspace.mkdir()
        self.source = self.root / "source"
        self.source.write_bytes(b"fixture executable")
        self.source.chmod(0o755)
        self.config = self.root / "codex.config.json"
        self.original = b'{"apiKey":"fixture-only", "exec":{"mode":"restricted"}, "multiProject":true}\n'
        self.config.write_bytes(self.original)
        self.der = b"public certificate fixture"
        self.fingerprint = hashlib.sha1(self.der).hexdigest().upper()
        self.settings = {
            "installationDir": str(self.root / "install"),
            "serviceLabel": "local.example.codex-free",
            "launchAgentPlist": str(self.root / "LaunchAgents/free.plist"),
            "signingIdentifier": "local.example.codex-free",
            "signingIdentityFile": str(self.root / "install/signing/identity.json"),
        }
        self.args = argparse.Namespace(work_dir=self.workspace, source=self.source,
            config=self.config, certificate_sha1=self.fingerprint, codex_cli=None)

    def plan(self):
        return bootstrap.plan(self.args, self.settings)

    def apply_with_mocks(self, p, *, sign_error=None, health_error=None, bootout_error=None):
        helper = bootstrap.helpers()
        helper.wait_healthy = Mock(return_value=456, side_effect=health_error)

        def command(*args, **kwargs):
            if args[:2] == ("/usr/bin/codesign", "--force") and sign_error:
                raise sign_error
            if args[:2] == ("/bin/launchctl", "bootout") and bootout_error:
                raise bootout_error
            return ""

        with patch.object(bootstrap.sys, "platform", "darwin"), \
             patch.object(bootstrap.subprocess, "run", return_value=subprocess.CompletedProcess([], 1)), \
             patch.object(bootstrap.socket, "create_connection", side_effect=ConnectionRefusedError), \
             patch.object(bootstrap, "public_certificate", return_value=self.der), \
             patch.object(bootstrap, "helpers", return_value=helper), \
             patch.object(bootstrap, "run", side_effect=command) as commands, \
             contextlib.redirect_stdout(io.StringIO()):
            self.commands = commands
            bootstrap.apply(p)
        return commands, helper

    def test_plan_has_no_side_effects_and_preserves_execution_policy(self):
        self.args.codex_cli = self.source
        p = self.plan()
        self.assertEqual(self.config.read_bytes(), self.original)
        self.assertFalse(p["destination"].parent.exists())
        self.assertFalse(p["plist"].parent.exists())
        updated = json.loads(p["updated"])
        self.assertEqual(updated["exec"], {"mode": "restricted"})
        self.assertTrue(updated["multiProject"])
        self.assertEqual(p["job"]["ProgramArguments"][-1], str(self.workspace))
        self.assertEqual(updated["mcpServers"]["codex_app_server"]["type"], "codex-app-server")
        self.assertEqual(p["bridge"]["codexCli"], str(self.source))
        self.assertFalse(p["bridge"]["allowRawRpc"])

    def test_refuses_existing_installation_or_dangling_plist(self):
        destination = Path(self.settings["installationDir"]) / "codex-free"
        destination.parent.mkdir()
        destination.write_bytes(b"existing installation")
        with self.assertRaisesRegex(RuntimeError, "Existing installation"):
            self.plan()
        destination.unlink()
        plist = Path(self.settings["launchAgentPlist"])
        plist.parent.mkdir()
        plist.symlink_to(self.root / "missing")
        with self.assertRaisesRegex(RuntimeError, "Existing installation"):
            self.plan()
        self.assertTrue(plist.is_symlink())

    def test_tunnel_uses_existing_local_key_file_not_inline_or_environment(self):
        key = self.root / "runtime.key"
        key.write_text("fixture-only")
        for ref in ["inline-key", "env:KEY", "file:" + str(self.root / "missing"), None]:
            self.config.write_text(json.dumps({"openaiTunnel": {"tunnelId": "fixture", "apiKeyRef": ref}}))
            with self.assertRaisesRegex(RuntimeError, "file: tunnel key"):
                self.plan()
        self.config.write_text(json.dumps({"openaiTunnel": {"tunnelId": "fixture", "apiKeyRef": "file:" + str(key)}}))
        self.assertEqual(self.plan()["port"], 3000)

    def test_existing_adapter_is_preserved(self):
        existing = {"type": "codex-app-server", "appServer": {"codexCli": str(self.source)}}
        self.config.write_text(json.dumps({"apiKey": "fixture", "mcpServers": {"codex_app_server": existing}}))
        self.args.codex_cli = self.source
        p = self.plan()
        self.assertIsNone(p["bridge"])
        self.assertEqual(json.loads(p["updated"])["mcpServers"]["codex_app_server"], existing)

    def test_public_certificate_selection_never_exports_a_private_key(self):
        other = base64.b64encode(b"another public certificate").decode()
        chosen = base64.b64encode(self.der).decode()
        pem = f"-----BEGIN CERTIFICATE-----\n{other}\n-----END CERTIFICATE-----\n" \
              f"-----BEGIN CERTIFICATE-----\n{chosen}\n-----END CERTIFICATE-----\n"
        with patch.object(bootstrap, "run", side_effect=[self.fingerprint, pem]) as run:
            self.assertEqual(bootstrap.public_certificate(self.fingerprint, Path("/fixture/keychain")), self.der)
        self.assertEqual(run.call_args_list, [
            call("/usr/bin/security", "find-identity", "-p", "codesigning", "/fixture/keychain"),
            call("/usr/bin/security", "find-certificate", "-a", "-p", "/fixture/keychain")])

    def test_signed_first_install_pins_identity_before_starting_service(self):
        self.args.codex_cli = self.source
        p = self.plan()
        commands, helper = self.apply_with_mocks(p)
        calls = commands.call_args_list
        self.assertEqual(calls[0].args[:2], ("/usr/bin/codesign", "--force"))
        self.assertIn("=designated => identifier", calls[0].args[-2])
        self.assertIn(self.fingerprint, calls[0].args[-2])
        self.assertEqual(calls[1].args[:2], ("/usr/bin/codesign", "--verify"))
        self.assertEqual(calls[2].args[:2], ("/bin/launchctl", "bootstrap"))
        self.assertEqual(p["destination"].read_bytes(), self.source.read_bytes())
        self.assertEqual(plistlib.loads(p["plist"].read_bytes())["Label"], self.settings["serviceLabel"])
        identity = json.loads(p["identity"].read_text())
        self.assertEqual(identity["certificate_sha256"], hashlib.sha256(self.der).hexdigest().upper())
        self.assertEqual(p["certificate"].read_bytes(), self.der)
        self.assertEqual(p["identity"].stat().st_mode & 0o777, 0o600)
        self.assertEqual(p["bridge_path"].stat().st_mode & 0o777, 0o600)
        helper.wait_healthy.assert_called_once()

    def test_signing_denial_leaves_no_install_and_does_not_retry(self):
        p = self.plan()
        with self.assertRaisesRegex(RuntimeError, "denied"):
            self.apply_with_mocks(p, sign_error=RuntimeError("denied"))
        for key in ("destination", "plist", "identity", "certificate"):
            self.assertFalse(p[key].exists())
        self.assertEqual(self.config.read_bytes(), self.original)
        self.assertEqual(self.commands.call_count, 1)

    def test_health_failure_stops_only_own_service_and_restores_configuration(self):
        self.args.codex_cli = self.source
        p = self.plan()
        with self.assertRaisesRegex(RuntimeError, "not healthy"):
            self.apply_with_mocks(p, health_error=RuntimeError("not healthy"))
        for key in ("destination", "plist", "identity", "certificate", "bridge_path"):
            self.assertFalse(p[key].exists())
        self.assertEqual(self.config.read_bytes(), self.original)
        self.assertEqual(list(self.root.glob("*.bootstrap-backup-*")), [])
        self.assertEqual(self.commands.call_args_list[-1].args,
            ("/bin/launchctl", "bootout", f"gui/{bootstrap.os.getuid()}/{self.settings['serviceLabel']}"))

    def test_concurrent_config_edit_is_not_overwritten_or_rolled_back(self):
        p = self.plan()
        changed = self.original + b" "
        self.config.write_bytes(changed)
        with self.assertRaisesRegex(RuntimeError, "configuration changed during staging"):
            self.apply_with_mocks(p)
        self.assertEqual(self.config.read_bytes(), changed)
        self.assertFalse(p["destination"].exists())
        self.assertFalse(p["identity"].exists())
        self.assertEqual(self.commands.call_count, 2)

    def test_registered_service_stops_before_signing_or_creating_files(self):
        p = self.plan()
        with patch.object(bootstrap.sys, "platform", "darwin"), \
             patch.object(bootstrap.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)), \
             patch.object(bootstrap, "public_certificate") as certificate:
            with self.assertRaisesRegex(RuntimeError, "already registered"):
                bootstrap.apply(p)
        certificate.assert_not_called()
        self.assertFalse(p["destination"].parent.exists())

    def test_failed_stop_retains_recovery_files(self):
        p = self.plan()
        with self.assertRaisesRegex(RuntimeError, "stop failed"):
            self.apply_with_mocks(p, health_error=RuntimeError("not healthy"), bootout_error=RuntimeError("stop failed"))
        self.assertTrue(p["destination"].exists())
        self.assertTrue(p["plist"].exists())
        self.assertTrue(p["identity"].exists())
        self.assertTrue(list(self.root.glob("*.bootstrap-backup-*")))


if __name__ == "__main__":
    unittest.main()
