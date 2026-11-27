#!/usr/bin/env bash
set -euo pipefail

# Give the provider its own storage and bus, even on a logged-in desktop.
isolated=$(mktemp -d)
trap 'rm -rf -- "$isolated"' EXIT
export XDG_DATA_HOME="$isolated/data"
export XDG_CONFIG_HOME="$isolated/config"
export XDG_CACHE_HOME="$isolated/cache"
export XDG_RUNTIME_DIR="$isolated/run"
export SUONELO_TEST_KEYRING=isolated
unset GNOME_KEYRING_CONTROL GNOME_KEYRING_PID DBUS_SESSION_BUS_ADDRESS
mkdir -m 700 "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_RUNTIME_DIR"

run_session() {
    dbus-run-session -- bash -euo pipefail -c '
    export GNOME_KEYRING_CONTROL="$XDG_RUNTIME_DIR/keyring"
    gnome-keyring-daemon --unlock --foreground --components=secrets \
        --control-directory "$GNOME_KEYRING_CONTROL" <<< "suonelo-isolated-test" >/dev/null &
    keyring=$!
    trap '\''kill "$keyring" 2>/dev/null || true; wait "$keyring" 2>/dev/null || true'\'' EXIT
    gdbus wait --session --timeout=10 org.freedesktop.secrets
    "$@"
' bash "$@"
}

if [[ ${1:-} == --restart ]]; then
    shift
    SUONELO_TEST_KEYRING_PHASE=write run_session "$@"
    SUONELO_TEST_KEYRING_PHASE=restore run_session "$@"
else
    run_session "$@"
fi
