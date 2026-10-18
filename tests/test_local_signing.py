"""Signing policy regression tests; never use a real private key or service."""
import hashlib
import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/local_signing.py"
SPEC = importlib.util.spec_from_file_location("local_signing", SCRIPT)
signing = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(signing)


class LocalSigningTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.certificate = self.root / "certificate.der"
        self.certificate.write_bytes(b"test public certificate")
        self.fp = hashlib.sha1(self.certificate.read_bytes()).hexdigest().upper()
        self.req = f'identifier "{signing.IDENTIFIER}" and certificate leaf = H"{self.fp}"'
        self.data = {"certificate_sha1": self.fp, "identifier": signing.IDENTIFIER,
                     "certificate_file": str(self.certificate),
                     "certificate_sha256": hashlib.sha256(self.certificate.read_bytes()).hexdigest().upper(),
                     "keychain": str(Path.home() / "Library/Keychains/login.keychain-db"),
                     "requirement": self.req}
        self.metadata = self.root / "identity.json"
        self.current = self.root / "current"
        self.stage = self.root / "stage"
        self.current.write_bytes(b"old build")
        self.stage.write_bytes(b"new build")

    def read_identity(self):
        self.metadata.write_text(json.dumps(self.data))
        with patch.object(signing, "IDENTITY_FILE", self.metadata):
            return signing.identity()

    def test_accepts_pinned_public_metadata(self):
        self.assertEqual(self.read_identity(), self.data)

    def test_refuses_another_app_identifier(self):
        self.data["identifier"] = "local.example.another-app"
        with self.assertRaisesRegex(RuntimeError, "Unexpected local signing"):
            self.read_identity()

    def test_refuses_weakened_certificate_requirement(self):
        self.data["requirement"] = f'identifier "{signing.IDENTIFIER}"'
        with self.assertRaisesRegex(RuntimeError, "requirement changed"):
            self.read_identity()

    def test_refuses_replaced_public_certificate(self):
        self.certificate.write_bytes(b"a different certificate")
        with self.assertRaisesRegex(RuntimeError, "certificate changed"):
            self.read_identity()

    def test_adhoc_migration_is_explicit(self):
        with patch.object(signing, "identity", return_value=self.data), \
             patch.object(signing, "designated_requirement", return_value='cdhash H"123"'), \
             patch.object(signing, "command") as command:
            with self.assertRaisesRegex(RuntimeError, "explicit initial migration"):
                signing.sign_staged(self.stage, self.current)
            command.assert_not_called()

    def test_initial_migration_rejects_different_build(self):
        with patch.object(signing, "identity", return_value=self.data), \
             patch.object(signing, "check_current", return_value=('cdhash H"123"', False)), \
             patch.object(signing, "command") as command:
            with self.assertRaisesRegex(RuntimeError, "exact copy"):
                signing.sign_staged(self.stage, self.current, True)
            command.assert_not_called()

    def test_invalid_old_identity_stops_before_signing_even_with_migration_flag(self):
        with patch.object(signing, "identity", return_value=self.data), \
             patch.object(signing, "designated_requirement", return_value=self.req), \
             patch.object(signing, "verify", side_effect=RuntimeError("wrong old certificate")), \
             patch.object(signing, "command") as command:
            with self.assertRaisesRegex(RuntimeError, "wrong old certificate"):
                signing.sign_staged(self.stage, self.current, True)
            command.assert_not_called()

    def test_changed_binary_must_satisfy_previous_dr(self):
        old = self.req + ' and info["version"] exists'
        canonical = signing.designated_requirement_text(self.data)
        with patch.object(signing, "identity", return_value=self.data), \
             patch.object(signing, "check_current", return_value=(old, True)), \
             patch.object(signing, "designated_requirement", return_value=canonical), \
             patch.object(signing, "command"), \
             patch.object(signing, "verify", side_effect=[None, RuntimeError("old DR failed")]) as verify:
            with self.assertRaisesRegex(RuntimeError, "old DR failed"):
                signing.sign_staged(self.stage, self.current)
            self.assertEqual(verify.call_args_list[-1].args, (self.stage, old))

    def test_signs_fixed_identifier_and_certificate_without_authentication_retry(self):
        with patch.object(signing, "identity", return_value=self.data), \
             patch.object(signing, "check_current", return_value=(self.req, True)), \
             patch.object(signing, "designated_requirement", return_value=signing.designated_requirement_text(self.data)), \
             patch.object(signing, "verify"), patch.object(signing, "command") as command:
            signing.sign_staged(self.stage, self.current)
            argv = command.call_args.args
            self.assertEqual(argv[argv.index("--sign") + 1], self.fp)
            self.assertEqual(argv[argv.index("--identifier") + 1], signing.IDENTIFIER)
            command.assert_called_once()

    def test_requirement_drift_is_rejected(self):
        with patch.object(signing, "identity", return_value=self.data), \
             patch.object(signing, "check_current", return_value=(self.req, True)), \
             patch.object(signing, "designated_requirement", return_value=self.req + ' or anchor apple'), \
             patch.object(signing, "verify"), patch.object(signing, "command"):
            with self.assertRaisesRegex(RuntimeError, "differs from the pinned"):
                signing.sign_staged(self.stage, self.current)


if __name__ == "__main__":
    unittest.main()
