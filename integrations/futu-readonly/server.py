#!/usr/bin/env python3
"""Read-only MCP bridge for Futu's Legacy AppKey REST authentication.

Credentials are referenced by path and are never written to stdout/stderr.
Only GET endpoints for account reading are implemented deliberately.
"""

import json
import os
import re
import secrets
import shutil
import stat
import subprocess
import sys
import time
import urllib.parse
import urllib.request
import urllib.error
from pathlib import Path


API_HOST = "https://webapi.futunn.com"
DEFAULT_CREDENTIAL_DIR = str(Path.home() / ".config/futu-openapi")
ACCOUNT_ID_RE = re.compile(r"^[0-9]+$")
CURRENCY_RE = re.compile(r"^[A-Z]{3}$")


class FutuError(Exception):
    pass


def credential_paths():
    root = Path(os.environ.get("FUTU_LEGACY_CREDENTIAL_DIR", DEFAULT_CREDENTIAL_DIR))
    appkey_path = root / "appkey_id"
    private_key_path = root / "private_key.pem"
    for path in (appkey_path, private_key_path):
        try:
            info = path.lstat()
        except OSError:
            raise FutuError("Futu AppKey credentials are unavailable")
        if (not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077
                or (hasattr(os, "geteuid") and info.st_uid != os.geteuid())):
            raise FutuError("Futu AppKey credential permissions are unsafe")
    return appkey_path, private_key_path


def read_appkey(appkey_path):
    value = appkey_path.read_text(encoding="utf-8").strip()
    if not value:
        raise FutuError("Futu AppKey is unavailable")
    return value


def sign_payload(private_key_path, payload):
    node = os.environ.get("FUTU_LEGACY_NODE") or shutil.which("node")
    if not node:
        raise FutuError("Futu AppKey signing requires Node.js")
    try:
        process = subprocess.run(
            [node, str(Path(__file__).with_name("sign.mjs")), str(private_key_path)],
            input=payload.encode("utf-8"),
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        raise FutuError("Futu AppKey signature could not be created")
    if process.returncode != 0 or not process.stdout:
        raise FutuError("Futu AppKey signature could not be created")
    return process.stdout.decode("ascii").strip()


def signed_get(path, query=None):
    if not path.startswith("/api/v1.0/"):
        raise FutuError("Unsupported Futu API path")
    query_text = urllib.parse.urlencode(query or {}, doseq=True)
    timestamp_ms = str(int(time.time() * 1000))
    nonce = secrets.token_hex(16)
    appkey_path, private_key_path = credential_paths()
    appkey = read_appkey(appkey_path)
    payload = "{}\nGET\n{}\n{}\n".format(timestamp_ms, path, query_text)
    signature = sign_payload(private_key_path, payload)
    url = API_HOST + path + ("?" + query_text if query_text else "")
    request = urllib.request.Request(
        url,
        method="GET",
        headers={
            "Accept": "application/json",
            "X-Api-Key": appkey,
            "X-Timestamp": timestamp_ms,
            "X-Nonce": nonce,
            "Authorization": signature,
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            body = response.read().decode("utf-8")
    except urllib.error.HTTPError as error:
        code = None
        try:
            rejected = json.loads(error.read(65536))
            candidate = rejected.get("code", rejected.get("s"))
            if isinstance(candidate, int) or (isinstance(candidate, str) and re.fullmatch(r"-?[0-9]+", candidate)):
                code = candidate
        except (ValueError, AttributeError):
            pass
        raise FutuError("Futu API refused the read-only request (HTTP {}{}).".format(
            error.code, "; API code " + str(code) if code is not None else ""))
    except Exception:
        raise FutuError("Futu API network request failed")
    try:
        result = json.loads(body)
    except ValueError:
        raise FutuError("Futu API returned an invalid response")
    if not isinstance(result, dict):
        raise FutuError("Futu API returned an unexpected response")
    status = result.get("s")
    if status not in (None, 0, "0", "ok"):
        raise FutuError("Futu API rejected this read-only request")
    return result.get("d", result)


def account_id_from(arguments):
    value = str((arguments or {}).get("account_id", ""))
    if not ACCOUNT_ID_RE.fullmatch(value):
        raise FutuError("account_id must be a numeric authorized account ID")
    return value


def tool_specs():
    return [
        {
            "name": "futu_account_authorized_trd_accs",
            "description": "Read-only: list Futu accounts authorized by the configured AppKey. No orders or trade unlocks are available.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
        },
        {
            "name": "futu_account_funds",
            "description": "Read-only: get account funds for one authorized Futu account.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account_id": {"type": "string", "description": "Numeric account ID returned by futu_account_authorized_trd_accs."},
                    "currency": {"type": "string", "description": "Three-letter display currency, such as USD or HKD."},
                },
                "required": ["account_id", "currency"],
                "additionalProperties": False,
            },
        },
        {
            "name": "futu_account_positions",
            "description": "Read-only: get positions for one authorized Futu account.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account_id": {"type": "string", "description": "Numeric account ID returned by futu_account_authorized_trd_accs."},
                },
                "required": ["account_id"],
                "additionalProperties": False,
            },
        },
    ]


def invoke_tool(name, arguments):
    if name == "futu_account_authorized_trd_accs":
        return signed_get("/api/v1.0/accounts/authorized_trd_accs")
    account_id = account_id_from(arguments)
    if name == "futu_account_funds":
        currency = str((arguments or {}).get("currency", "")).upper()
        if not CURRENCY_RE.fullmatch(currency):
            raise FutuError("currency must be a three-letter code")
        query = {"currency": currency}
        return signed_get("/api/v1.0/accounts/{}/funds".format(account_id), query)
    if name == "futu_account_positions":
        return signed_get("/api/v1.0/accounts/{}/positions".format(account_id))
    raise FutuError("Unknown read-only Futu tool")


def send(message):
    sys.stdout.write(json.dumps(message, ensure_ascii=False, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def success(request_id, result):
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def failure(request_id, code, message):
    send({"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}})


def handle(message):
    method = message.get("method")
    request_id = message.get("id")
    if method == "notifications/initialized" or request_id is None:
        return
    if method == "initialize":
        params = message.get("params") or {}
        protocol_version = params.get("protocolVersion") or "2025-06-18"
        success(request_id, {
            "protocolVersion": protocol_version,
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "futu-legacy-readonly", "version": "1.0.0"},
        })
        return
    if method == "tools/list":
        success(request_id, {"tools": tool_specs()})
        return
    if method == "tools/call":
        params = message.get("params") or {}
        try:
            data = invoke_tool(params.get("name"), params.get("arguments") or {})
            success(request_id, {"content": [{"type": "text", "text": json.dumps(data, ensure_ascii=False)}]})
        except FutuError as error:
            success(request_id, {"content": [{"type": "text", "text": str(error)}], "isError": True})
        return
    failure(request_id, -32601, "Method not found")


def main():
    for line in sys.stdin:
        try:
            message = json.loads(line)
            if isinstance(message, dict):
                handle(message)
        except Exception:
            # Keep the protocol alive without ever echoing request content.
            failure(None, -32603, "Internal read-only MCP error")


if __name__ == "__main__":
    main()
