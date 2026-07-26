#!/bin/sh
# remon-server installer.
#
#   curl -fsSL https://raw.githubusercontent.com/adnanjpg/remon-server/dev/packaging/install.sh | sh
#
# Downloads the release build for this machine, verifies it against the
# published checksums, installs it, and — where systemd is running — leaves a
# started and enabled service behind.
#
# Re-running upgrades in place: configuration and the database are never
# touched, so this is also the upgrade path.
#
# POSIX sh on purpose. The hosts this targets are the ones that have nothing
# else on them yet, and /bin/sh is dash on Debian and Ubuntu.
#
# Environment:
#   REMON_VERSION=v0.17.2   install a specific tag (default: latest release)
#   REMON_PREFIX=/usr/local install somewhere else
#   REMON_NO_SERVICE=1      install the binary only, skip the service

set -eu

REPO="adnanjpg/remon-server"
PREFIX="${REMON_PREFIX:-/usr/local}"
BIN_DIR="$PREFIX/bin"
CONFIG_DIR="/etc/remon"
DATA_DIR="/var/lib/remon"
SERVICE_NAME="remon-server"
UNIT_PATH="/etc/systemd/system/$SERVICE_NAME.service"

# ── output ────────────────────────────────────────────────────────────────

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m')
    RED=$(printf '\033[31m'); GREEN=$(printf '\033[32m')
    YELLOW=$(printf '\033[33m'); RESET=$(printf '\033[0m')
else
    BOLD=''; DIM=''; RED=''; GREEN=''; YELLOW=''; RESET=''
fi

say()  { printf '%s\n' "$*"; }
step() { printf '%s==>%s %s\n' "$BOLD" "$RESET" "$*"; }
warn() { printf '%swarning:%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2; exit 1; }

# ── preflight ─────────────────────────────────────────────────────────────

need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"; }

need uname
need tar
need install
need mktemp

if command -v curl >/dev/null 2>&1; then
    DOWNLOAD='curl -fsSL -o'
elif command -v wget >/dev/null 2>&1; then
    DOWNLOAD='wget -qO'
else
    die "either curl or wget is required"
fi

fetch() { # fetch <url> <dest>
    # shellcheck disable=SC2086
    $DOWNLOAD "$2" "$1"
}

fetch_stdout() { # fetch_stdout <url>
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1"
    else
        wget -qO- "$1"
    fi
}

[ "$(id -u)" -eq 0 ] || die "must run as root (try: curl ... | sudo sh)"

# ── platform ──────────────────────────────────────────────────────────────

os=$(uname -s)
[ "$os" = "Linux" ] || die "this installer supports Linux; found $os"

case "$(uname -m)" in
    x86_64 | amd64)  PLATFORM=linux-amd64 ;;
    aarch64 | arm64) PLATFORM=linux-arm64 ;;
    *) die "unsupported architecture: $(uname -m) (amd64 and arm64 are built)" ;;
esac

# ── version ───────────────────────────────────────────────────────────────

VERSION="${REMON_VERSION:-}"
if [ -z "$VERSION" ]; then
    step "Resolving latest release"
    VERSION=$(fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n 1)
    [ -n "$VERSION" ] || die "could not determine the latest release; set REMON_VERSION=vX.Y.Z"
fi

BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
ARCHIVE="remon-server-$PLATFORM.tar.gz"

# ── download and verify ───────────────────────────────────────────────────

TMP=$(mktemp -d)
# Keep the trap simple: any exit path removes the scratch directory.
trap 'rm -rf "$TMP"' EXIT INT TERM

step "Downloading remon-server $VERSION ($PLATFORM)"
fetch "$BASE_URL/$ARCHIVE" "$TMP/$ARCHIVE" \
    || die "download failed — does $VERSION have a $PLATFORM build?"

if fetch "$BASE_URL/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
        step "Verifying checksum"
        expected=$(awk -v f="$ARCHIVE" '$2 == f || $2 == "*"f { print $1 }' "$TMP/SHA256SUMS")
        [ -n "$expected" ] || die "no checksum published for $ARCHIVE"
        actual=$(sha256sum "$TMP/$ARCHIVE" | awk '{print $1}')
        [ "$expected" = "$actual" ] || die "checksum mismatch — refusing to install"
    else
        warn "sha256sum not found; skipping checksum verification"
    fi
else
    warn "no SHA256SUMS published for $VERSION; skipping checksum verification"
fi

step "Unpacking"
tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
[ -f "$TMP/remon-server" ] || die "archive did not contain a remon-server binary"

# ── stop, install, restart ────────────────────────────────────────────────

have_systemd=0
if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
    have_systemd=1
fi

was_running=0
if [ "$have_systemd" -eq 1 ] && systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    was_running=1
    step "Stopping $SERVICE_NAME for upgrade"
    systemctl stop "$SERVICE_NAME"
fi

step "Installing to $BIN_DIR/remon-server"
install -d -m 0755 "$BIN_DIR"
install -m 0755 "$TMP/remon-server" "$BIN_DIR/remon-server"

install -d -m 0755 "$CONFIG_DIR"
install -d -m 0700 "$DATA_DIR"

# The binary carries its own defaults, so this file exists to be edited, not
# to be required. Never overwrite an operator's copy on upgrade.
if [ ! -f "$CONFIG_DIR/config.toml" ]; then
    if [ -f "$TMP/config.toml.sample" ]; then
        install -m 0644 "$TMP/config.toml.sample" "$CONFIG_DIR/config.toml"
    else
        cat > "$CONFIG_DIR/config.toml" <<'SAMPLE'
# remon-server configuration. Every key is optional — defaults are compiled
# into the binary. See `remon-server --help` and CONFIG.md for the full set.

[server]
port = 8080
host = "0.0.0.0"
# Set true only behind a reverse proxy that controls X-Forwarded-For.
trusted_proxy = false

[logging]
level = "info"
# "json" once you are shipping logs somewhere that parses them.
format = "compact"

[cors]
# Browser clients only. Native apps authenticate with bearer tokens and are
# unaffected by anything here. Add your web UI's origin to use one:
# allowed_origins = ["https://app.example.com"]
allow_any_origin = false
allowed_origins = []
SAMPLE
        chmod 0644 "$CONFIG_DIR/config.toml"
    fi
    say "  ${DIM}wrote $CONFIG_DIR/config.toml${RESET}"
else
    say "  ${DIM}kept existing $CONFIG_DIR/config.toml${RESET}"
fi

step "Validating configuration"
"$BIN_DIR/remon-server" --config-dir "$CONFIG_DIR" --data-dir "$DATA_DIR" config check \
    || die "configuration did not validate; nothing was enabled"

# ── service ───────────────────────────────────────────────────────────────

if [ -n "${REMON_NO_SERVICE:-}" ]; then
    say ""
    say "${GREEN}Installed.${RESET} Service setup skipped (REMON_NO_SERVICE)."
    say "Run it with: ${BOLD}remon-server --config-dir $CONFIG_DIR --data-dir $DATA_DIR${RESET}"
    exit 0
fi

if [ "$have_systemd" -eq 0 ]; then
    say ""
    warn "systemd not detected — installed the binary only"
    say "Run it with: ${BOLD}remon-server --config-dir $CONFIG_DIR --data-dir $DATA_DIR${RESET}"
    exit 0
fi

step "Installing systemd unit"
if [ -f "$TMP/remon-server.service" ]; then
    install -m 0644 "$TMP/remon-server.service" "$UNIT_PATH"
else
    cat > "$UNIT_PATH" <<UNIT
[Unit]
Description=Remon monitoring server
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
ExecStart=$BIN_DIR/remon-server --config-dir $CONFIG_DIR --data-dir $DATA_DIR
Restart=on-failure
RestartSec=5s
KillSignal=SIGTERM
TimeoutStopSec=30s
ProtectHome=read-only
ProtectClock=true
ProtectHostname=true
RestrictSUIDSGID=true
RestrictRealtime=true
LockPersonality=true
StateDirectory=remon
StateDirectoryMode=0700
StandardOutput=journal
StandardError=journal
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT
    chmod 0644 "$UNIT_PATH"
fi

systemctl daemon-reload
systemctl enable --quiet "$SERVICE_NAME" 2>/dev/null || true

step "Starting $SERVICE_NAME"
systemctl restart "$SERVICE_NAME"

# Give it a moment to bind or fail. A crash loop is the one outcome the
# operator must not have to discover on their own later.
port=$(sed -n 's/^[[:space:]]*port[[:space:]]*=[[:space:]]*\([0-9]\{1,\}\).*/\1/p' \
    "$CONFIG_DIR/config.toml" 2>/dev/null | head -n 1)
[ -n "$port" ] || port=8080

ok=0
i=0
while [ "$i" -lt 30 ]; do
    if command -v curl >/dev/null 2>&1; then
        curl -fsS -m 2 "http://127.0.0.1:$port/health" >/dev/null 2>&1 && { ok=1; break; }
    else
        wget -q -T 2 -O /dev/null "http://127.0.0.1:$port/health" 2>/dev/null && { ok=1; break; }
    fi
    systemctl is-active --quiet "$SERVICE_NAME" || break
    i=$((i + 1))
    sleep 1
done

# `hostname -I` is absent on busybox and on some minimal images, and the route
# lookup is absent without iproute2 — fall back rather than print "http://:8080".
host_addr=$(hostname -I 2>/dev/null | awk '{print $1}')
[ -n "$host_addr" ] || host_addr=$(ip -4 route get 1 2>/dev/null | sed -n 's/.*src \([0-9.]*\).*/\1/p')
[ -n "$host_addr" ] || host_addr="127.0.0.1"

say ""
if [ "$ok" -eq 1 ]; then
    say "${GREEN}remon-server $VERSION is running.${RESET}"
    say ""
    say "  ${BOLD}http://$host_addr:$port${RESET}"
    say ""
    if [ "$was_running" -eq 1 ]; then
        say "  ${DIM}upgraded in place; configuration and database untouched${RESET}"
    else
        say "  ${DIM}add the address above in the Remon app, then start pairing${RESET}"
        say "  ${DIM}the 8-digit code appears in: journalctl -fu $SERVICE_NAME${RESET}"
    fi
    say ""
    say "  ${DIM}config   $CONFIG_DIR/config.toml${RESET}"
    say "  ${DIM}data     $DATA_DIR${RESET}"
    say "  ${DIM}diagnose remon-server --config-dir $CONFIG_DIR --data-dir $DATA_DIR doctor${RESET}"
else
    warn "the service did not answer on http://127.0.0.1:$port/health"
    say ""
    say "  ${BOLD}systemctl status $SERVICE_NAME${RESET}"
    say "  ${BOLD}journalctl -u $SERVICE_NAME -n 50${RESET}"
    say "  ${BOLD}remon-server --config-dir $CONFIG_DIR --data-dir $DATA_DIR doctor${RESET}"
    exit 1
fi
