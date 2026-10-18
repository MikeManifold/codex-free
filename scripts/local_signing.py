"""Pinned signing identity and fail-closed staging for the local Free service.

Only public certificate metadata is read here. codesign is the sole key user.
"""
import contextlib
import fcntl
import hashlib
import json
import re
import subprocess
from pathlib import Path
from install_config import load_install_config

_SETTINGS = load_install_config()
IDENTIFIER = _SETTINGS["signingIdentifier"]
IDENTITY_FILE = Path(_SETTINGS["signingIdentityFile"])
SIGNING_DIR = IDENTITY_FILE.parent


def command(*args, timeout=30):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed ({result.returncode}): "
                           + (result.stderr or result.stdout).strip()[:1200])
    return result


@contextlib.contextmanager
def install_lock():
    # Both installers share the same nonblocking lock, including signing/recovery.
    # Never create identities or retry authentication from an update process.
    identity()
    with (SIGNING_DIR / "install.lock").open("a") as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError("Another Free installation is running; no changes made")
        try:
            yield
        finally:
            fcntl.flock(stream, fcntl.LOCK_UN)


def identity():
    data = json.loads(IDENTITY_FILE.read_text())
    fingerprint = data.get("certificate_sha1", "")
    if not re.fullmatch(r"[0-9A-F]{40}", fingerprint) or data.get("identifier") != IDENTIFIER:
        raise RuntimeError("Unexpected local signing metadata; refusing to sign")
    expected = f'identifier "{IDENTIFIER}" and certificate leaf = H"{fingerprint}"'
    if data.get("requirement") != expected:
        raise RuntimeError("Local signing requirement changed; inspect before updating")
    if data.get("keychain") != str(Path.home() / "Library/Keychains/login.keychain-db"):
        raise RuntimeError("Unexpected signing keychain")
    certificate = Path(data.get("certificate_file", ""))
    if not certificate.is_file():
        raise RuntimeError("Pinned public certificate is missing; no identity will be regenerated")
    public_der = certificate.read_bytes()
    if (hashlib.sha1(public_der).hexdigest().upper() != fingerprint
            or hashlib.sha256(public_der).hexdigest().upper() != data.get("certificate_sha256")):
        raise RuntimeError("Pinned public certificate changed; inspect before updating")
    return data


def designated_requirement(path):
    result = command("/usr/bin/codesign", "-d", "-r-", str(path))
    match = re.search(r"^\s*(?:#\s*)?designated => (.+)$",
                      result.stdout + result.stderr, re.MULTILINE)
    if not match:
        raise RuntimeError("Cannot read designated requirement: " + str(path))
    return match.group(1)


def verify(path, requirement):
    command("/usr/bin/codesign", "--verify", "--all-architectures", "--strict", "--verbose=2",
            "-R", "=" + requirement, str(path))


def check_current(current, data, allow_initial_migration=False):
    old_requirement = designated_requirement(current)
    if re.fullmatch(r'cdhash H"[0-9a-fA-F]+"', old_requirement):
        if not allow_initial_migration:
            raise RuntimeError("Current binary has no pinned stable identity; explicit initial migration required")
        details = command("/usr/bin/codesign", "-d", "--verbose=4", str(current))
        if "Signature=adhoc" not in details.stdout + details.stderr:
            raise RuntimeError("Initial migration is restricted to an ad-hoc signature")
        verify(current, old_requirement)
        return old_requirement, False
    # Never treat a different certificate/app as a migration, even with the flag.
    verify(current, data["requirement"])
    verify(current, old_requirement)
    return old_requirement, True


def sign_staged(staged, current, allow_initial_migration=False):
    data = identity()
    old_requirement, compatible = check_current(current, data, allow_initial_migration)
    if not compatible and hashlib.sha256(Path(staged).read_bytes()).digest() != hashlib.sha256(Path(current).read_bytes()).digest():
        raise RuntimeError("First migration must sign an exact copy of the installed binary")
    command("/usr/bin/codesign", "--force", "--sign", data["certificate_sha1"],
            "--keychain", data["keychain"], "--identifier", IDENTIFIER,
            "--timestamp=none", "--requirements", "=designated => " + data["requirement"],
            str(staged), timeout=120)
    verify(staged, data["requirement"])
    if designated_requirement(staged) != designated_requirement_text(data):
        raise RuntimeError("Staged designated requirement differs from the pinned identity")
    if compatible:
        verify(staged, old_requirement)
    return data["requirement"], compatible


def designated_requirement_text(data):
    # codesign displays certificate hex in lowercase, independently of input case.
    return data["requirement"].replace(data["certificate_sha1"], data["certificate_sha1"].lower())
