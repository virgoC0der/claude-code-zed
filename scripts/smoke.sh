#!/usr/bin/env bash
# scripts/smoke.sh — manual smoke harness for zed-claude-bridge.
#
# Spins up the sidecar against `/tmp/zcb-smoke-ws`, polls for the lock file,
# uses jq + websocat to drive the MCP handshake (initialize → tools/list)
# and assert the response, then SIGTERMs the sidecar and asserts the lock
# file is removed.
#
# Requires: cargo (to build), jq, websocat. websocat is available via
# Homebrew (`brew install websocat`) or cargo (`cargo install websocat`).
#
# Run from repo root:
#
#   bash scripts/smoke.sh
#
# Exits 0 on success, non-zero on any failed assertion.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

ws_root="/tmp/zcb-smoke-ws"
lock_dir="$(mktemp -d -t zcb-smoke-locks-XXXXXX)"
trap 'cleanup' EXIT

cleanup() {
    if [[ -n "${PID:-}" ]]; then
        kill -TERM "$PID" 2>/dev/null || true
        # Give the sidecar a moment to clean up before we forcibly remove
        # any leftover artefacts.
        sleep 0.3
    fi
    rm -rf "$lock_dir" "$ws_root" 2>/dev/null || true
}

require() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "smoke.sh: missing required tool: $1" >&2
        exit 2
    fi
}

require jq
require websocat

mkdir -p "$ws_root"
# Verify lock-dir mode 0700 — the sidecar enforces this on open.
mkdir -p "$lock_dir"
chmod 700 "$lock_dir"

echo "[smoke] building sidecar (debug)…"
cargo build -p zed-claude-bridge >/dev/null

echo "[smoke] starting sidecar (lock-dir=$lock_dir)…"
./target/debug/zed-claude-bridge \
    --workspace "$ws_root" \
    --lock-dir "$lock_dir" \
    --foreground \
    > /tmp/zcb-smoke.log 2>&1 &
PID=$!

# Poll for the lock file (up to ~3 s).
deadline=$(( $(date +%s) + 3 ))
lock_path=""
while [[ $(date +%s) -lt $deadline ]]; do
    candidate="$(ls -1 "$lock_dir"/*.lock 2>/dev/null | head -1 || true)"
    if [[ -n "$candidate" ]]; then
        lock_path="$candidate"
        break
    fi
    sleep 0.1
done
if [[ -z "$lock_path" ]]; then
    echo "[smoke] FAIL: lock file did not appear within 3s" >&2
    echo "--- sidecar log ---" >&2
    cat /tmp/zcb-smoke.log >&2 || true
    exit 1
fi
echo "[smoke] lock file: $lock_path"

# Verify file mode is 0600 and dir mode is 0700.
file_mode="$(stat -f '%Lp' "$lock_path" 2>/dev/null || stat -c '%a' "$lock_path")"
dir_mode="$(stat -f '%Lp' "$lock_dir" 2>/dev/null || stat -c '%a' "$lock_dir")"
if [[ "$file_mode" != "600" ]]; then
    echo "[smoke] FAIL: lock file mode is $file_mode, expected 600" >&2
    exit 1
fi
if [[ "$dir_mode" != "700" ]]; then
    echo "[smoke] FAIL: lock dir mode is $dir_mode, expected 700" >&2
    exit 1
fi

# Pull port + token out of the lock file.
port="$(basename "$lock_path" .lock)"
token="$(jq -r .authToken "$lock_path")"
echo "[smoke] port=$port"

# Drive initialize via websocat. `-n1` makes websocat exit after one reply.
init_resp="$(printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n' | \
    websocat -n1 -H "x-claude-code-ide-authorization: $token" \
        "ws://127.0.0.1:$port")"
echo "[smoke] initialize response: $init_resp"
proto_version="$(echo "$init_resp" | jq -r '.result.protocolVersion')"
if [[ "$proto_version" != "2024-11-05" ]]; then
    echo "[smoke] FAIL: protocolVersion=$proto_version, expected 2024-11-05" >&2
    exit 1
fi

# Drive tools/list and assert getCurrentSelection is advertised.
tools_resp="$(printf '{"jsonrpc":"2.0","id":2,"method":"tools/list"}\n' | \
    websocat -n1 -H "x-claude-code-ide-authorization: $token" \
        "ws://127.0.0.1:$port")"
echo "[smoke] tools/list response: $tools_resp"
if ! echo "$tools_resp" | jq -e '.result.tools[] | select(.name == "getCurrentSelection")' >/dev/null; then
    echo "[smoke] FAIL: tools/list did not advertise getCurrentSelection" >&2
    exit 1
fi

# Send SIGTERM and assert the lock file is gone within ~2s.
kill -TERM "$PID"
wait "$PID" 2>/dev/null || true
PID=""

if [[ -e "$lock_path" ]]; then
    echo "[smoke] FAIL: lock file still exists after SIGTERM: $lock_path" >&2
    exit 1
fi

echo "[smoke] OK — handshake + tools/list passed, lock file cleaned up on SIGTERM."
