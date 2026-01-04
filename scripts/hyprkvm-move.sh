#!/bin/bash
# HyprKVM Move Handler
#
# This script intercepts workspace movement keybindings and routes them
# through HyprKVM to enable cross-machine navigation.
#
# Usage: hyprkvm-move.sh <direction>
# Directions: left, right, up, down
#
# Install in hyprland.conf:
#   bind = SUPER, Left,  exec, ~/.config/hypr/scripts/hyprkvm-move.sh left
#   bind = SUPER, Right, exec, ~/.config/hypr/scripts/hyprkvm-move.sh right
#   bind = SUPER, Up,    exec, ~/.config/hypr/scripts/hyprkvm-move.sh up
#   bind = SUPER, Down,  exec, ~/.config/hypr/scripts/hyprkvm-move.sh down

set -e

DIRECTION="${1:-}"
SOCKET_PATH="${XDG_RUNTIME_DIR:-/tmp}/hyprkvm.sock"

if [[ -z "$DIRECTION" ]]; then
    echo "Usage: $0 <left|right|up|down>" >&2
    exit 1
fi

# Validate direction
case "$DIRECTION" in
    left|right|up|down|l|r|u|d)
        ;;
    *)
        echo "Invalid direction: $DIRECTION" >&2
        echo "Valid: left, right, up, down" >&2
        exit 1
        ;;
esac

# Check if HyprKVM daemon is running
if [[ -S "$SOCKET_PATH" ]]; then
    # Daemon is running - send move request
    # The daemon will decide whether to do a local move or network switch
    if command -v hyprkvm &>/dev/null; then
        hyprkvm move "$DIRECTION"
    else
        # Fallback: try to send via socket directly
        echo "{\"command\":\"move\",\"direction\":\"$DIRECTION\"}" | \
            nc -U -N "$SOCKET_PATH" 2>/dev/null || \
            hyprctl dispatch movefocus "$DIRECTION"
    fi
else
    # Daemon not running - fall back to native hyprctl
    hyprctl dispatch movefocus "$DIRECTION"
fi
