#!/bin/sh
# remon-server uninstaller.
#
#   curl -fsSL https://raw.githubusercontent.com/adnanjpg/remon-server/dev/packaging/uninstall.sh | sudo sh
#
# Stops and removes the service and the binary. The database and the
# configuration are left alone unless --purge is given, because "uninstall"
# and "throw away my history" are different intentions and only one of them
# is recoverable.
#
# Environment (the same overrides install.sh takes — pass whatever was used to
# install, or --purge will look for state where this install never put any):
#   REMON_PREFIX=/usr/local
#   REMON_CONFIG_DIR=/etc/remon
#   REMON_DATA_DIR=/var/lib/remon

set -eu

PREFIX="${REMON_PREFIX:-/usr/local}"
BIN="$PREFIX/bin/remon-server"
CONFIG_DIR="${REMON_CONFIG_DIR:-/etc/remon}"
DATA_DIR="${REMON_DATA_DIR:-/var/lib/remon}"
SERVICE_NAME="remon-server"
UNIT_PATH="/etc/systemd/system/$SERVICE_NAME.service"
INITD_PATH="/etc/init.d/$SERVICE_NAME"

PURGE=0
for arg in "$@"; do
    case "$arg" in
        --purge) PURGE=1 ;;
        *) printf 'error: unknown argument %s\n' "$arg" >&2; exit 1 ;;
    esac
done

[ "$(id -u)" -eq 0 ] || { printf 'error: must run as root\n' >&2; exit 1; }

if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
    if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
        printf '==> Stopping %s\n' "$SERVICE_NAME"
        systemctl stop "$SERVICE_NAME"
    fi
    if systemctl is-enabled --quiet "$SERVICE_NAME" 2>/dev/null; then
        systemctl disable --quiet "$SERVICE_NAME"
    fi
    if [ -f "$UNIT_PATH" ]; then
        printf '==> Removing %s\n' "$UNIT_PATH"
        rm -f "$UNIT_PATH"
        systemctl daemon-reload
    fi
elif command -v rc-service >/dev/null 2>&1; then
    if rc-service --quiet "$SERVICE_NAME" status >/dev/null 2>&1; then
        printf '==> Stopping %s\n' "$SERVICE_NAME"
        rc-service "$SERVICE_NAME" stop >/dev/null
    fi
    rc-update del "$SERVICE_NAME" default >/dev/null 2>&1 || true
    if [ -f "$INITD_PATH" ]; then
        printf '==> Removing %s\n' "$INITD_PATH"
        rm -f "$INITD_PATH"
    fi
fi

if [ -f "$BIN" ]; then
    printf '==> Removing %s\n' "$BIN"
    rm -f "$BIN"
fi

if [ "$PURGE" -eq 1 ]; then
    printf '==> Removing %s and %s\n' "$CONFIG_DIR" "$DATA_DIR"
    rm -rf "$CONFIG_DIR" "$DATA_DIR"
    # Where OpenRC's supervise-daemon sends the server's output, pairing codes
    # included. Under systemd this lives in the journal and is not ours to prune.
    rm -f "/var/log/$SERVICE_NAME.log" "/var/log/$SERVICE_NAME.log."*
    printf '\nremon-server removed, including configuration and metrics history.\n'
else
    printf '\nremon-server removed.\n'
    [ -e "$CONFIG_DIR" ] && printf 'Kept %s\n' "$CONFIG_DIR"
    [ -e "$DATA_DIR" ] && printf 'Kept %s (metrics history, pairings)\n' "$DATA_DIR"
    printf 'Run with --purge to delete those too.\n'
fi
