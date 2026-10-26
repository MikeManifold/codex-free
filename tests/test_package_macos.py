"""Release archives must contain deployable tools and no local credentials."""
import hashlib
import importlib.util
import io
import json
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/package-macos.py"
SPEC = importlib.util.spec_from_file_location("macos_package", SCRIPT)
packager = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(packager)


def source_archive(name="scripts/bootstrap-macos.py", kind=tarfile.REGTYPE):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w") as archive:
        entry = tarfile.TarInfo(name)
        entry.mode = 0o755
        entry.type = kind
        entry.linkname = "outside" if kind == tarfile.SYMTYPE else ""
        entry.size = 0 if kind == tarfile.SYMTYPE else 4
        archive.addfile(entry, None if kind == tarfile.SYMTYPE else io.BytesIO(b"tool"))
    return stream.getvalue()


class PackageTests(unittest.TestCase):
    def test_personal_certificate_and_wrong_architecture_cannot_be_published(self):
        with patch.object(packager, "command", return_value=b"arm64\n"), \
             patch.object(packager.subprocess, "run", return_value=subprocess.CompletedProcess([], 0,
                 stdout="", stderr="Authority=Local Signing Fixture")):
            with self.assertRaisesRegex(RuntimeError, "ad-hoc signed"):
                packager.verify_binary(Path("fixture"), "aarch64-apple-darwin")
        with patch.object(packager, "command", return_value=b"x86_64\n"):
            with self.assertRaisesRegex(RuntimeError, "architecture"):
                packager.verify_binary(Path("fixture"), "aarch64-apple-darwin")

    def test_package_preserves_executable_modes_and_verifiable_provenance(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            binary = root / "input"
            binary.write_bytes(b"fixture Mach-O bytes")
            path = packager.package(binary, "aarch64-apple-darwin", root / "out", "a" * 40, source_archive())
            checksum = path.with_suffix(".zip.sha256").read_text().split()[0]
            self.assertEqual(checksum, hashlib.sha256(path.read_bytes()).hexdigest())
            with zipfile.ZipFile(path) as archive:
                prefix = "codex-free-macos-arm64/"
                metadata = json.loads(archive.read(prefix + "release.json"))
                self.assertEqual(metadata["sourceCommit"], "a" * 40)
                self.assertEqual(metadata["binarySha256"], hashlib.sha256(binary.read_bytes()).hexdigest())
                self.assertEqual(archive.read(prefix + "scripts/bootstrap-macos.py"), b"tool")
                self.assertEqual(archive.getinfo(prefix + "codex-free").external_attr >> 16 & 0o777, 0o755)
                self.assertEqual(archive.getinfo(prefix + "scripts/bootstrap-macos.py").external_attr >> 16 & 0o777, 0o755)

    def test_personal_runtime_files_are_rejected_even_if_accidentally_committed(self):
        for name in (".env", "private_key.pem", "identity.p12", "bridge.config.local.json",
                     "settings.local.toml", "output/result.json", "docs/maintenance/notes.md"):
            with self.subTest(name=name), self.assertRaisesRegex(RuntimeError, "Local configuration"):
                packager.archive_entries(source_archive(name))

    def test_examples_and_skill_dot_directory_are_preserved(self):
        for name in ("scripts/install.config.example.json", ".agents/skills/codex-free-deploy/SKILL.md"):
            self.assertIn(name, packager.archive_entries(source_archive(name)))

    def test_links_and_path_traversal_cannot_enter_the_download_package(self):
        with self.assertRaisesRegex(RuntimeError, "regular files"):
            packager.archive_entries(source_archive("link", tarfile.SYMTYPE))
        for name in ("../private", "/absolute"):
            with self.subTest(name=name), self.assertRaisesRegex(RuntimeError, "Unsafe archive"):
                packager.archive_entries(source_archive(name))

    def test_only_selected_committed_deployment_paths_are_exported(self):
        self.assertIn("scripts/bootstrap-macos.py", packager.DEPLOYMENT_PATHS)
        self.assertIn("scripts/install-stable-signed.py", packager.DEPLOYMENT_PATHS)
        self.assertIn("scripts/local_signing.py", packager.DEPLOYMENT_PATHS)
        self.assertIn(".agents/skills/codex-free-deploy", packager.DEPLOYMENT_PATHS)
        self.assertNotIn(".", packager.DEPLOYMENT_PATHS)
        self.assertNotIn("codex.config.json", packager.DEPLOYMENT_PATHS)


if __name__ == "__main__":
    unittest.main()
