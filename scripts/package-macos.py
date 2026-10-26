#!/usr/bin/env python3
"""Package a CI executable with committed deployment tools, never local state."""
import argparse
import hashlib
import io
import json
import os
import re
import subprocess
import tarfile
import zipfile
from pathlib import Path, PurePosixPath


TARGETS = {"aarch64-apple-darwin": "arm64", "x86_64-apple-darwin": "x86_64"}
DEPLOYMENT_PATHS = (
    "README.md", "LICENSE", "docs/ARCHITECTURE.md", "docs/REFERENCE.md",
    "scripts/bootstrap-macos.py", "scripts/install_config.py",
    "scripts/install-stable-signed.py", "scripts/install-fallback-ready.py",
    "scripts/local_signing.py", "scripts/install.config.example.json",
    ".agents/skills/codex-free-deploy", "integrations/codex-app-server-bridge",
)


def command(*args):
    return subprocess.check_output(args)


def verify_binary(binary, target):
    architecture = command("/usr/bin/lipo", "-archs", str(binary)).decode().strip()
    if architecture != TARGETS[target]:
        raise RuntimeError("Executable architecture does not match the package target")
    subprocess.run(["/usr/bin/codesign", "--verify", "--strict", "--all-architectures", str(binary)], check=True)
    details = subprocess.run(["/usr/bin/codesign", "-d", "--verbose=4", str(binary)],
                             capture_output=True, text=True, check=True)
    if "Signature=adhoc" not in details.stdout + details.stderr:
        raise RuntimeError("Release binaries must be ad-hoc signed; no personal certificate is packaged")


def archive_entries(source):
    entries = {}
    with tarfile.open(fileobj=io.BytesIO(source)) as archive:
        for member in archive:
            path = PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts:
                raise RuntimeError("Unsafe archive path")
            if member.isdir():
                continue
            if not member.isfile():
                raise RuntimeError("Deployment archive must contain regular files only")
            if (path.name.startswith(".env") or path.name.endswith((".local.json", ".local.toml", ".key", ".pem", ".p12", ".pfx"))
                    or path.parts[:2] == ("docs", "maintenance") or path.parts[0] == "output"):
                raise RuntimeError("Local configuration or credential file in deployment archive")
            entries[str(path)] = (archive.extractfile(member).read(), 0o755 if member.mode & 0o111 else 0o644)
    return entries


def package(binary, target, output, commit, source):
    if not re.fullmatch(r"[a-f0-9]{40}", commit):
        raise RuntimeError("Expected an exact source commit")
    data = binary.read_bytes()
    entries = archive_entries(source)
    metadata = {"sourceCommit": commit, "target": target, "architecture": TARGETS[target],
                "buildDeploymentTarget": os.environ.get("MACOSX_DEPLOYMENT_TARGET", "13.0"),
                "binarySha256": hashlib.sha256(data).hexdigest(), "signature": "ad-hoc"}
    entries["codex-free"] = (data, 0o755)
    entries["release.json"] = ((json.dumps(metadata, indent=2) + "\n").encode(), 0o644)
    name = "codex-free-macos-" + TARGETS[target]
    output.mkdir(parents=True, exist_ok=True)
    path = output / (name + ".zip")
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for relative, (content, mode) in sorted(entries.items()):
            info = zipfile.ZipInfo(name + "/" + relative)
            info.create_system = 3
            info.external_attr = (0o100000 | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, content)
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    path.with_suffix(".zip.sha256").write_text(digest + "  " + path.name + "\n")
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--output-dir", type=Path, default=Path("dist"))
    args = parser.parse_args()
    verify_binary(args.binary, args.target)
    commit = command("git", "rev-parse", "HEAD").decode().strip()
    source = command("git", "archive", "--format=tar", "HEAD", "--", *DEPLOYMENT_PATHS)
    print(package(args.binary, args.target, args.output_dir, commit, source))


if __name__ == "__main__":
    main()
