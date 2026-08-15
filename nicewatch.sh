#!/usr/bin/env bash
# Launcher for the Nicewatch GUI.  Starts the daemon if it isn't running,
# then runs the GUI (release binary if built, else Tauri dev mode).
set -euo pipefail

cd "$(dirname "$0")"

BIN_GUI_RELEASE="target/release/nicewatch-gui"
BIN_GUI_DEBUG="target/debug/nicewatch-gui"
BIN_DAEMON_RELEASE="target/release/nicewatch"
BIN_DAEMON_DEBUG="target/debug/nicewatch"
DAEMON_NAME="nicewatch"

# A stale GUI from a previous run sits on the old build and its socket
# guard blocks nothing (the daemon is all that matters).  Force-close any
# previous GUI instance before launching the fresh one.
pkill -9 -x "nicewatch-gui" 2>/dev/null || true

start_daemon() {
    local daemon=""
    if [ -x "$BIN_DAEMON_RELEASE" ]; then
        daemon="$BIN_DAEMON_RELEASE"
    elif [ -x "$BIN_DAEMON_DEBUG" ]; then
        daemon="$BIN_DAEMON_DEBUG"
    fi
    if pgrep -x "$DAEMON_NAME" >/dev/null 2>&1; then
        echo "daemon already running"
        return 0
    fi
    if [ -z "$daemon" ]; then
        echo "warning: daemon binary not built; the GUI will wait for it (see SETUP.md)"
        return 0
    fi
    echo "starting daemon: $daemon"
    # setsid: detach so the daemon outlives this terminal (closing the window
    # used to SIGHUP it, which the GUI then reported as "daemon offline").
    setsid nohup "$daemon" >/dev/null 2>&1 &
    sleep 0.5
}

if [ -x "$BIN_GUI_RELEASE" ]; then
    start_daemon
    exec "$BIN_GUI_RELEASE" "$@"
fi

# No release binary: fall back to Tauri dev mode (auto-builds the GUI).
start_daemon
cd gui
exec npx tauri dev "$@"