#!/usr/bin/env bash
# Uninstall the zed-claude-bridge LaunchAgent.
set -euo pipefail

LABEL="com.virgoC0der.zed-claude-bridge"
DST="$HOME/Library/LaunchAgents/${LABEL}.plist"
UID_DOMAIN="gui/$(id -u)"

if launchctl print "$UID_DOMAIN/$LABEL" >/dev/null 2>&1; then
    echo "Stopping agent…"
    launchctl bootout "$UID_DOMAIN/$LABEL" 2>/dev/null || true
fi

if [[ -f "$DST" ]]; then
    rm -f "$DST"
    echo "Removed $DST"
fi

echo "Done."
