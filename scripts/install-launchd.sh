#!/usr/bin/env bash
# Install zed-claude-bridge as a user LaunchAgent.
# Idempotent: re-running re-installs and reloads the agent.
set -euo pipefail

LABEL="com.virgoC0der.zed-claude-bridge"
SRC="$(cd "$(dirname "$0")" && pwd)/com.virgoC0der.zed-claude-bridge.plist"
DST="$HOME/Library/LaunchAgents/${LABEL}.plist"

if ! command -v zed-claude-bridge >/dev/null 2>&1; then
    echo "error: zed-claude-bridge not on PATH. Install it first:" >&2
    echo "  cargo install --path crates/zed-claude-bridge" >&2
    exit 1
fi

BIN="$(command -v zed-claude-bridge)"
echo "Using binary: $BIN"

# Substitute placeholders into the template and write the final plist.
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
sed -e "s|__BIN__|$BIN|g" -e "s|__HOME__|$HOME|g" "$SRC" > "$DST"
chmod 0644 "$DST"
echo "Installed plist: $DST"

# Reload via launchctl. Modern macOS uses bootout/bootstrap on the gui domain.
UID_DOMAIN="gui/$(id -u)"

if launchctl print "$UID_DOMAIN/$LABEL" >/dev/null 2>&1; then
    echo "Stopping existing agent…"
    launchctl bootout "$UID_DOMAIN/$LABEL" 2>/dev/null || true
fi

echo "Starting agent…"
launchctl bootstrap "$UID_DOMAIN" "$DST"
launchctl enable "$UID_DOMAIN/$LABEL"
launchctl kickstart -k "$UID_DOMAIN/$LABEL"

# Wait briefly for the sidecar to come up, then sanity-check.
sleep 1
if launchctl print "$UID_DOMAIN/$LABEL" 2>/dev/null | grep -q 'state = running'; then
    echo "Agent is running."
    echo "Logs: tail -f $HOME/Library/Logs/zed-claude-bridge.log"
else
    echo "warning: agent state is not 'running'. Check the log:" >&2
    echo "  tail $HOME/Library/Logs/zed-claude-bridge.log" >&2
    exit 1
fi
