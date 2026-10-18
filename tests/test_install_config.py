"""Local settings preserve an existing deployment identity without publishing it."""
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from install_config import load_install_config


class InstallConfigTests(unittest.TestCase):
    def test_personal_settings_keep_existing_service_and_identity(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "settings.json"
            values = {"version": 1, "serviceLabel": "local.fixture.service",
                      "signingIdentifier": "local.fixture.stable-app",
                      "installationDir": str(Path(root) / "existing-install"),
                      "signingIdentityFile": str(Path(root) / "shared-public-identity.json")}
            path.write_text(json.dumps(values))
            with patch.dict(os.environ, {"CODEX_FREE_INSTALL_CONFIG": str(path)}):
                settings = load_install_config()
            for name, value in values.items():
                self.assertEqual(settings[name], value)

    def test_invalid_or_missing_explicit_settings_never_fall_back(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "settings.json"
            with patch.dict(os.environ, {"CODEX_FREE_INSTALL_CONFIG": str(path)}):
                with self.assertRaisesRegex(RuntimeError, "missing"):
                    load_install_config()
                for values in ({"version": 1, "unknown": True},
                               {"version": 1, "installationDir": "relative"},
                               {"version": 1, "signingIdentifier": "invalid identifier"}):
                    path.write_text(json.dumps(values))
                    with self.assertRaises(RuntimeError):
                        load_install_config()
