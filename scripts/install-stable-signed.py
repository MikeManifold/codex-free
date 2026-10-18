#!/usr/bin/env python3
"""Install a pinned local signature; preserve Free configuration and registration."""
import argparse
import hashlib
import importlib.util
import json
import os
import plistlib
import shutil
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

from local_signing import check_current, identity, install_lock, sign_staged, verify


def service_helpers():
    path = Path(__file__).with_name("install-fallback-ready.py")
    spec = importlib.util.spec_from_file_location("fallback_installer", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def install(args):
    helpers = service_helpers()
    home = Path.home()
    from install_config import load_install_config
    settings = load_install_config()
    destination = Path(settings["installationDir"]) / "codex-free"
    plist = Path(settings["launchAgentPlist"])
    plist_bytes = plist.read_bytes()
    job = plistlib.loads(plist_bytes)
    argv = job.get("ProgramArguments", [])
    if (job.get("Label") != settings["serviceLabel"] or not argv
            or argv[0] != str(destination)
            or job.get("Program", str(destination)) != str(destination)):
        raise RuntimeError("Unexpected service executable; inspect before updating")
    if destination.is_symlink() or not destination.is_file():
        raise RuntimeError("Installed binary must be a regular file")
    if args.source.is_symlink() or not args.source.is_file() or not os.access(args.source, os.X_OK):
        raise RuntimeError("Source must be a regular executable file")
    data = identity()
    old_requirement, compatible = check_current(destination, data, args.migrate_from_adhoc)
    original_hash = hashlib.sha256(destination.read_bytes()).hexdigest()
    if not compatible and hashlib.sha256(args.source.read_bytes()).hexdigest() != original_hash:
        raise RuntimeError("First migration must use the installed binary, without a feature update")
    cwd = Path(job.get("WorkingDirectory") or "/")
    env = job.get("EnvironmentVariables", {})
    selected = helpers.option(argv, "--config") or env.get("CODEX_FREE_CONFIG")
    config_path = Path(selected) if selected else home / ".codex-free/codex.config.json"
    if not config_path.is_absolute():
        config_path = cwd / config_path
    config = json.loads(config_path.read_bytes())
    port = int(helpers.option(argv, "--port") or config.get("port", 3000))
    if not 1 <= port <= 65535:
        raise RuntimeError("Invalid MCP port")
    service = f"gui/{os.getuid()}/{settings['serviceLabel']}"
    old_pid = helpers.service_pid(service)
    if old_pid is None:
        raise RuntimeError("Existing service is not running; inspect before updating")
    if args.check:
        print(f"Preflight OK: pid={old_pid}; identifier={data['identifier']}; stable={compatible}; no changes made")
        return
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    backup = destination.with_name(destination.name + ".backup-stable-signing-" + stamp)
    shutil.copy2(destination, backup)
    shutil.copy2(plist, backup.with_suffix(backup.suffix + ".launchagent.plist"))
    fd, stage = tempfile.mkstemp(prefix=".codex-free-stable-", dir=destination.parent)
    os.close(fd)
    replaced = False
    try:
        shutil.copyfile(args.source, stage)
        os.chmod(stage, 0o755)
        requirement, compatible = sign_staged(stage, destination, args.migrate_from_adhoc)
        if plist.read_bytes() != plist_bytes or hashlib.sha256(destination.read_bytes()).hexdigest() != original_hash:
            raise RuntimeError("Active files changed during staging; refusing replacement")
        print(f"Staged signature verified; previous stable DR compatible={compatible}; backup={backup}", flush=True)
        # An initial ad-hoc cdhash cannot survive re-signing. Only this explicit,
        # exact-copy migration bypasses the previous DR; all future updates check it.
        os.replace(stage, destination)
        replaced = True
        helpers.restart_registered(service)
        new_pid = helpers.wait_healthy(service, port, old_pid=old_pid)
        verify(destination, requirement)
        receipt = {"old_pid": old_pid, "new_pid": new_pid, "backup": str(backup),
                   "previous_requirement": old_requirement, "requirement": requirement,
                   "previous_stable_dr_compatible": compatible,
                   "source_sha256_before_signing": hashlib.sha256(args.source.read_bytes()).hexdigest()
                   if args.source.resolve() != destination.resolve() else original_hash,
                   "installed_sha256": hashlib.sha256(destination.read_bytes()).hexdigest()}
        helpers.atomic_write(backup.with_suffix(backup.suffix + ".receipt.json"),
                             (json.dumps(receipt, indent=2) + "\n").encode())
        print(f"Installed pinned identity; MCP healthy; old_pid={old_pid}; new_pid={new_pid}", flush=True)
    except BaseException as install_error:
        if replaced:
            restore_fd, restore = tempfile.mkstemp(prefix=".codex-free-restore-", dir=destination.parent)
            os.close(restore_fd)
            try:
                shutil.copy2(backup, restore)
                os.replace(restore, destination)
                try:
                    recovery_old_pid = helpers.service_pid(service)
                except Exception:
                    recovery_old_pid = None
                helpers.restart_registered(service)
                restored_pid = helpers.wait_healthy(service, port, old_pid=recovery_old_pid, timeout=90)
                verify(destination, old_requirement)
                print(f"Rollback verified; original MCP restored (pid={restored_pid}); backup={backup}", flush=True)
            except BaseException as recovery_error:
                raise RuntimeError(f"Update failed: {install_error}; recovery incomplete: {recovery_error}; backup={backup}") from install_error
            finally:
                if os.path.exists(restore):
                    os.unlink(restore)
        raise
    finally:
        if os.path.exists(stage):
            os.unlink(stage)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--check", action="store_true", help="Read-only preflight; no signing or restart")
    parser.add_argument("--migrate-from-adhoc", action="store_true", help="One-time signature migration of an identical installed binary")
    args = parser.parse_args()
    if sys.platform != "darwin":
        raise RuntimeError("macOS only")
    if args.check:
        install(args)
    else:
        with install_lock():
            install(args)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print("ERROR: " + str(error), file=sys.stderr)
        sys.exit(1)
