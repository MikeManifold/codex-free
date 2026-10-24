#!/usr/bin/env python3
"""User-run installer: preserve launch settings, enable exec, replace signed binary."""
import argparse
import hashlib
import json
import os
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from datetime import datetime
from pathlib import Path


def option(args, name):
    values = []
    for index, item in enumerate(args):
        if item == name:
            if index + 1 == len(args):
                raise ValueError("Missing value for " + name)
            values.append(args[index + 1])
        elif item.startswith(name + "="):
            values.append(item.split("=", 1)[1])
    if len(values) > 1:
        raise ValueError("Ambiguous repeated option: " + name)
    return values[0] if values else None


def atomic_write(path, data, mode=0o600):
    fd, tmp = tempfile.mkstemp(prefix=".codex-free-install-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(tmp, mode)
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def run(*args):
    return subprocess.run(args, capture_output=True, text=True, timeout=30)


def command_error(operation, result):
    detail = (result.stderr or result.stdout or "no diagnostic output").strip()
    return RuntimeError(f"{operation} failed (exit {result.returncode}): {detail[:1200]}")


def service_pid(service):
    result = run("/bin/launchctl", "print", service)
    if result.returncode:
        raise command_error("launchctl print", result)
    match = re.search(r"^\s*pid = (\d+)\s*$", result.stdout, re.MULTILINE)
    return int(match.group(1)) if match else None


def restart_registered(service):
    # The plist is unchanged. Keep its registration; do not bootout/bootstrap.
    previous = service_pid(service)
    if previous is not None:
        result = run("/bin/launchctl", "kill", "SIGTERM", service)
        if result.returncode and service_pid(service) == previous:
            raise command_error("launchctl graceful stop", result)
        deadline = time.monotonic() + 160
        while service_pid(service) == previous:
            if time.monotonic() >= deadline:
                raise RuntimeError("Free is still draining; refusing a forced restart during possible OAuth persistence")
            time.sleep(0.2)
        if service_pid(service) is not None:
            return  # KeepAlive already started the replacement.
    result = run("/bin/launchctl", "kickstart", service)
    if result.returncode:
        raise command_error("launchctl kickstart", result)


def wait_healthy(service, port, old_pid=None, timeout=180):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    url = "http://127.0.0.1:" + str(port) + "/health"
    deadline = time.monotonic() + timeout
    last_error = "No running process yet"
    next_progress = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            pid = service_pid(service)
            if pid is not None and pid != old_pid:
                with opener.open(url, timeout=2) as response:
                    health = json.load(response)
                if isinstance(health, dict) and health.get("status") == "ok":
                    if service_pid(service) == pid:
                        return pid
                last_error = "Health not ready for the new service process"
            else:
                last_error = "Waiting for a new service process"
        except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
            last_error = str(error)
        if time.monotonic() >= next_progress:
            print("Still waiting: " + last_error[:300], flush=True)
            next_progress = time.monotonic() + 15
        time.sleep(1)
    raise RuntimeError("Local health timed out: " + last_error[:1200])


def activate_update(service, port, config_path, updated, original,
                    staging, destination, binary_backup, old_pid):
    try:
        atomic_write(config_path, updated)
        os.replace(staging, destination)
        print("Files installed. Restarting the existing registered service...", flush=True)
        restart_registered(service)
        return wait_healthy(service, port, old_pid=old_pid)
    except BaseException as install_error:
        print("Update failed: " + str(install_error), flush=True)
        try:
            atomic_write(config_path, original)
            atomic_write(destination, binary_backup.read_bytes(), 0o755)
            rollback_old_pid = service_pid(service)
            restart_registered(service)
            restored_pid = wait_healthy(service, port, old_pid=rollback_old_pid, timeout=90)
            print(f"Rollback verified: original files restored; service healthy (pid {restored_pid}).", flush=True)
        except BaseException as rollback_error:
            raise RuntimeError(
                f"Update failed: {install_error}; rollback/recovery failed: {rollback_error}. "
                f"Backups: {config_path} and {binary_backup}. Do not repeat installation blindly."
            ) from install_error
        raise RuntimeError("Update was not installed; original service restored. " + str(install_error)) from install_error


def install(args):
    from local_signing import check_current, identity, sign_staged

    if sys.platform != "darwin":
        raise RuntimeError("This installer is for the Mac LaunchAgent only.")
    home = Path.home()
    root = Path(__file__).resolve().parent.parent
    from install_config import load_install_config
    settings = load_install_config()
    plist = Path(settings["launchAgentPlist"])
    job = plistlib.loads(plist.read_bytes())
    argv = job.get("ProgramArguments", [])
    exe = job.get("Program") or (argv[0] if argv else "")
    label = job.get("Label")
    if label != settings["serviceLabel"] or Path(exe).name != "codex-free":
        raise RuntimeError("Unexpected launch configuration; no changes made.")
    if not Path(exe).is_absolute():
        raise RuntimeError("Launch executable is not absolute; no changes made.")
    destination = Path(exe).resolve(strict=True)
    source = root / "target/release/codex-free"
    expected = Path(settings["verifiedReleaseReceipt"]).read_text().split()[0]
    if hashlib.sha256(source.read_bytes()).hexdigest() != expected:
        raise RuntimeError("Release binary changed since verification; no changes made.")

    cwd = Path(job.get("WorkingDirectory") or "/")
    env = job.get("EnvironmentVariables", {})
    selected = option(argv, "--config") or env.get("CODEX_FREE_CONFIG")
    if selected:
        config_path = Path(selected)
        if not config_path.is_absolute():
            config_path = cwd / config_path
    else:
        config_path = home / ".codex-free/codex.config.json"
        if not config_path.is_file():
            config_path = cwd / "codex.config.json"
    config_path = config_path.resolve(strict=True)
    original = config_path.read_bytes()
    config = json.loads(original)
    if not isinstance(config, dict) or not isinstance(config.get("exec", {}), dict):
        raise RuntimeError("Unexpected configuration structure; no changes made.")
    port = int(option(argv, "--port") or config.get("port", 3000))
    if not 1 <= port <= 65535:
        raise RuntimeError("Invalid MCP port; no changes made.")
    config.setdefault("exec", {})["mode"] = "unrestricted"
    updated = (json.dumps(config, ensure_ascii=False, indent=2) + "\n").encode()
    domain = "gui/" + str(os.getuid())
    service = domain + "/" + label
    old_pid = service_pid(service)
    if old_pid is None:
        raise RuntimeError("Original service is not running; no changes made.")
    check_current(destination, identity())
    if args.check:
        print("Preflight OK. Running pid: " + str(old_pid))
        print("Config: " + str(config_path))
        print("Verified release: " + expected)
        print("Planned restart: graceful SIGTERM, then KeepAlive or kickstart (no forced kill).")
        return

    stamp = datetime.now().strftime("%Y%m%d-%H%M%S-%f")
    binary_backup = destination.with_name(destination.name + ".backup-" + stamp)
    config_backup = config_path.with_name(config_path.name + ".backup-" + stamp)
    original_binary_hash = hashlib.sha256(destination.read_bytes()).hexdigest()
    original_plist = plist.read_bytes()
    shutil.copy2(destination, binary_backup)
    atomic_write(config_backup, original)
    shutil.copy2(plist, plist.with_name(plist.name + ".backup-" + stamp))
    fd, staging = tempfile.mkstemp(prefix=".codex-free-stage-", dir=destination.parent)
    os.close(fd)
    try:
        shutil.copyfile(source, staging)
        os.chmod(staging, 0o755)
        if hashlib.sha256(Path(staging).read_bytes()).hexdigest() != expected:
            raise RuntimeError("Release binary changed during staging; active files unchanged")
        sign_staged(staging, destination)
        if (hashlib.sha256(destination.read_bytes()).hexdigest() != original_binary_hash
                or plist.read_bytes() != original_plist or config_path.read_bytes() != original):
            raise RuntimeError("Active files changed during staging; refusing replacement")
        print("Backup: " + str(config_backup), flush=True)
        print("Backup: " + str(binary_backup), flush=True)
        active_pid = activate_update(service, port, config_path, updated, original,
                                     staging, destination, binary_backup, old_pid)
    finally:
        if os.path.exists(staging):
            os.unlink(staging)

    print(f"Verified new service process: {active_pid}")
    print("OK: updated binary installed; exec.mode=unrestricted; local MCP health=ok.")
    print("Existing tunnel, project bindings and ChatGPT app permissions were not changed.")
    print("Verify fallback working-directory and stdin behavior through the active connector.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply-unrestricted", action="store_true")
    parser.add_argument("--check", action="store_true", help="Read-only preflight; do not install or restart")
    args = parser.parse_args()
    if not args.apply_unrestricted and not args.check:
        parser.error("Pass --apply-unrestricted to authorize the local command-policy change.")
    if args.check:
        install(args)
    else:
        from local_signing import install_lock
        with install_lock():
            install(args)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print("ERROR: " + str(error), file=sys.stderr)
        sys.exit(1)
