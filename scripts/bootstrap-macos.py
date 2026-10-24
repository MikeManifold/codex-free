#!/usr/bin/env python3
"""First macOS installation. Plan by default; never export or create private keys."""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
import plistlib
import re
import shutil
import socket
import subprocess
import sys
import tempfile
from pathlib import Path


def run(*args, timeout=30):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        # Do not echo provider/configuration output into deployment transcripts.
        raise RuntimeError(f"{Path(args[0]).name} failed (exit {result.returncode}); inspect locally")
    return result.stdout


def helpers():
    path = Path(__file__).with_name("install-fallback-ready.py")
    spec = importlib.util.spec_from_file_location("bootstrap_service_helpers", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_new(path, data, mode, created):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    created.append(path)
    with os.fdopen(fd, "wb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def public_certificate(fingerprint, keychain):
    identities = run("/usr/bin/security", "find-identity", "-p", "codesigning", str(keychain))
    if fingerprint not in identities.upper():
        raise RuntimeError("Selected certificate has no usable local signing identity; inspect Keychain Access")
    certificates = run("/usr/bin/security", "find-certificate", "-a", "-p", str(keychain))
    for block in re.findall(r"-----BEGIN CERTIFICATE-----\s*(.*?)\s*-----END CERTIFICATE-----", certificates, re.S):
        der = base64.b64decode(re.sub(r"\s", "", block), validate=True)
        if hashlib.sha1(der).hexdigest().upper() == fingerprint:
            return der
    raise RuntimeError("Selected public certificate was not found in the login keychain")


def plan(args, settings):
    workspace = args.work_dir.expanduser().resolve(strict=True)
    source = args.source.expanduser().resolve(strict=True)
    config_path = args.config.expanduser().resolve(strict=True)
    if not workspace.is_dir() or not source.is_file() or not os.access(source, os.X_OK):
        raise RuntimeError("Workspace must be a directory and source must be an executable")
    original = config_path.read_bytes()
    config = json.loads(original)
    if not isinstance(config, dict):
        raise RuntimeError("Runtime configuration must be an object")
    tunnel = config.get("openaiTunnel")
    if tunnel:
        if not isinstance(tunnel, dict) or not isinstance(tunnel.get("tunnelId"), str) or not tunnel["tunnelId"]:
            raise RuntimeError("Run quickstart to configure a valid tunnel before installing")
        if config.get("apiKey"):
            raise RuntimeError("Endpoint apiKey and native tunnel mode cannot be combined")
        ref = tunnel.get("apiKeyRef", "")
        if not isinstance(ref, str) or not ref.startswith("file:") or not Path(ref[5:]).expanduser().is_file():
            raise RuntimeError("Use quickstart to save a machine-local file: tunnel key reference before installing")
    elif not config.get("apiKey"):
        raise RuntimeError("Run quickstart first; a persistent service needs a tunnel or configured endpoint authentication")
    port = config.get("port", 3000)
    if isinstance(port, bool) or not isinstance(port, int) or not 1 <= port <= 65535:
        raise RuntimeError("Invalid MCP port")
    destination = Path(settings["installationDir"]) / "codex-free"
    plist = Path(settings["launchAgentPlist"])
    for path in (destination, plist):
        if path.exists() or path.is_symlink():
            raise RuntimeError("Existing installation detected; use install-stable-signed.py, not bootstrap")
    identity_path = Path(settings["signingIdentityFile"])
    fingerprint = (args.certificate_sha1 or "").replace(" ", "").upper()
    if identity_path.exists():
        fingerprint = fingerprint or json.loads(identity_path.read_text()).get("certificate_sha1", "")
    if not re.fullmatch(r"[A-F0-9]{40}", fingerprint):
        raise RuntimeError("Provide --certificate-sha1 for an existing local code-signing identity")
    keychain = Path.home() / "Library/Keychains/login.keychain-db"
    certificate_path = identity_path.parent / "certificate.der"
    bridge_path = config_path.parent / "bridge.config.local.json"
    bridge = None
    if args.codex_cli:
        cli = args.codex_cli.expanduser().resolve(strict=True)
        if not cli.is_file() or not os.access(cli, os.X_OK):
            raise RuntimeError("Codex CLI must be an executable")
        servers = config.setdefault("mcpServers", {})
        if not isinstance(servers, dict):
            raise RuntimeError("mcpServers must be an object")
        if "codex_app_server" not in servers:
            if bridge_path.exists() or bridge_path.is_symlink():
                raise RuntimeError("Existing bridge configuration needs explicit review; no overwrite")
            bridge = {"version": 1, "codexCli": str(cli), "workDir": str(workspace),
                      "codexHome": str(Path.home() / ".codex"), "requestTimeoutMs": 120000,
                      "confirmationTimeoutMs": 300000, "allowRawRpc": False}
            servers["codex_app_server"] = {"type": "codex-app-server", "mode": "catalog",
                "toolTimeoutSec": 600, "appServer": {"configFile": str(bridge_path)}}
    updated = (json.dumps(config, ensure_ascii=False, indent=2) + "\n").encode()
    logs = config_path.parent / "logs"
    job = {"Label": settings["serviceLabel"],
           "ProgramArguments": [str(destination), "--config", str(config_path), "--work-dir", str(workspace)],
           "WorkingDirectory": str(workspace), "RunAtLoad": True, "KeepAlive": True,
           "ProcessType": "Background", "ExitTimeOut": 180,
           "EnvironmentVariables": {"PATH": str(Path.home() / ".cargo/bin") +
               ":/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"},
           "StandardOutPath": str(logs / "service.log"), "StandardErrorPath": str(logs / "service.err.log")}
    return dict(settings=settings, source=source, destination=destination, plist=plist,
                config=config_path, original=original, updated=updated, port=port, logs=logs,
                identity=identity_path, certificate=certificate_path, fingerprint=fingerprint,
                keychain=keychain, bridge_path=bridge_path, bridge=bridge, job=job)


def apply(p):
    if sys.platform != "darwin":
        raise RuntimeError("This bootstrap is macOS-only")
    service = f"gui/{os.getuid()}/{p['settings']['serviceLabel']}"
    registered = subprocess.run(["/bin/launchctl", "print", service], capture_output=True)
    if registered.returncode == 0:
        raise RuntimeError("Service is already registered; no changes made")
    try:
        connection = socket.create_connection(("127.0.0.1", p["port"]), timeout=.3)
    except ConnectionRefusedError:
        pass
    else:
        connection.close()
        raise RuntimeError("MCP port is already in use; no changes made")
    der = public_certificate(p["fingerprint"], p["keychain"])
    requirement = f'identifier "{p["settings"]["signingIdentifier"]}" and certificate leaf = H"{p["fingerprint"]}"'
    identity = {"identifier": p["settings"]["signingIdentifier"], "certificate_sha1": p["fingerprint"],
        "certificate_sha256": hashlib.sha256(der).hexdigest().upper(), "keychain": str(p["keychain"]),
        "certificate_file": str(p["certificate"]), "requirement": requirement}
    for path, data in [(p["identity"], identity)]:
        if path.exists() and json.loads(path.read_text()) != data:
            raise RuntimeError("Existing pinned identity differs; no identity rotation")
    if p["certificate"].exists() and p["certificate"].read_bytes() != der:
        raise RuntimeError("Existing public certificate differs; no overwrite")
    created = []
    bootstrapped = False
    config_changed = False
    helper = helpers()
    for directory in [p["destination"].parent, p["identity"].parent, p["plist"].parent, p["logs"]]:
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    backup = p["config"].with_name(p["config"].name + ".bootstrap-backup-" + str(os.getpid()))
    try:
        with tempfile.TemporaryDirectory(prefix=".free-first-install-", dir=p["destination"].parent) as scratch:
            staged = Path(scratch) / "codex-free"
            shutil.copyfile(p["source"], staged)
            staged.chmod(0o755)
            run("/usr/bin/codesign", "--force", "--sign", p["fingerprint"], "--keychain", str(p["keychain"]),
                "--identifier", identity["identifier"], "--timestamp=none", "--requirements",
                "=designated => " + requirement, str(staged), timeout=120)
            run("/usr/bin/codesign", "--verify", "--all-architectures", "--strict", "-R", "=" + requirement, str(staged))
            if p["config"].read_bytes() != p["original"]:
                raise RuntimeError("Runtime configuration changed during staging; no replacement")
            for path, data in [(p["certificate"], der),
                (p["identity"], (json.dumps(identity, indent=2) + "\n").encode())]:
                if not path.exists():
                    write_new(path, data, 0o600, created)
            if p["bridge"] is not None:
                write_new(p["bridge_path"], (json.dumps(p["bridge"], indent=2) + "\n").encode(), 0o600, created)
            write_new(backup, p["original"], 0o600, created)
            os.link(staged, p["destination"])
            created.append(p["destination"])
            write_new(p["plist"], plistlib.dumps(p["job"]), 0o600, created)
            helper.atomic_write(p["config"], p["updated"])
            config_changed = True
            run("/bin/launchctl", "bootstrap", f"gui/{os.getuid()}", str(p["plist"]))
            bootstrapped = True
            pid = helper.wait_healthy(service, p["port"])
            print(json.dumps({"installed": True, "pid": pid, "identifier": identity["identifier"],
                              "privateKeyExported": False, "configBackup": str(backup)}))
    except BaseException:
        if bootstrapped:
            # If stop fails, retain the installed files for inspection/recovery.
            run("/bin/launchctl", "bootout", service, timeout=200)
        if config_changed and p["config"].read_bytes() == p["updated"]:
            helper.atomic_write(p["config"], p["original"])
        for path in reversed(created):
            path.unlink(missing_ok=True)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", required=True, type=Path)
    parser.add_argument("--source", type=Path, default=Path(__file__).resolve().parents[1] / "target/release/codex-free")
    parser.add_argument("--config", type=Path, default=Path.home() / ".codex-free/codex.config.json")
    parser.add_argument("--codex-cli", type=Path, help="Add the optional App Server adapter; keep existing source if present")
    parser.add_argument("--certificate-sha1", help="Public fingerprint; the private key stays in the login keychain")
    parser.add_argument("--apply", action="store_true", help="Sign and register a first installation; otherwise only plan")
    args = parser.parse_args()
    from install_config import load_install_config
    p = plan(args, load_install_config())
    if args.apply:
        apply(p)
    else:
        print(json.dumps({"applied": False, "destination": str(p["destination"]), "plist": str(p["plist"]),
                          "identifier": p["settings"]["signingIdentifier"], "uiAdapterAdded": p["bridge"] is not None}, indent=2))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print("ERROR: " + str(error), file=sys.stderr)
        sys.exit(1)
