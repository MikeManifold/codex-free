#!/bin/zsh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo build --release --offline
exec /usr/bin/python3 "$ROOT/scripts/install-stable-signed.py" --source "$ROOT/target/release/codex-free"
