"""Local deployment settings. Personal values are never part of the source tree."""
import json
import os
from pathlib import Path


def load_install_config():
    task_home = Path.home()
    selected = os.environ.get("CODEX_FREE_INSTALL_CONFIG")
    path = Path(selected).expanduser() if selected else task_home / ".codex-free/install.local.json"
    settings = {
        "version": 1,
        "installationDir": str(task_home / ".local/opt/codex-free"),
        "serviceLabel": "local.example.codex-free",
        "launchAgentPlist": str(task_home / "Library/LaunchAgents/local.example.codex-free.plist"),
        "signingIdentifier": "local.example.codex-free",
        "signingIdentityFile": str(task_home / ".local/opt/codex-free/signing/identity.json"),
        "verifiedReleaseReceipt": str(task_home / ".codex-free/fallback-release.sha256"),
    }
    if path.exists():
        data = json.loads(path.read_text())
        if not isinstance(data, dict) or data.get("version") != 1 or set(data) - set(settings):
            raise RuntimeError("Invalid local installation configuration")
        settings.update(data)
    elif selected:
        raise RuntimeError("Explicit local installation configuration is missing")
    for key in ("installationDir", "launchAgentPlist", "signingIdentityFile", "verifiedReleaseReceipt"):
        value = settings[key]
        if not isinstance(value, str) or not value or "\0" in value:
            raise RuntimeError("Invalid local installation path: " + key)
        value = Path(value).expanduser()
        if not value.is_absolute():
            raise RuntimeError("Local installation paths must be absolute: " + key)
        settings[key] = str(value)
    for key in ("serviceLabel", "signingIdentifier"):
        value = settings[key]
        if not isinstance(value, str) or not value or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-" for c in value):
            raise RuntimeError("Invalid local installation identifier: " + key)
    return settings
