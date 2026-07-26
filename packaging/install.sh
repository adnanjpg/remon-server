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
#   REMON_VERSION=v0.18.0      install a specific tag (default: latest release)
#   REMON_PREFIX=/usr/local    install the binary somewhere else
#   REMON_CONFIG_DIR=/etc/remon
#   REMON_DATA_DIR=/var/lib/remon
#   REMON_NO_SERVICE=1         install the binary only, skip the service
#
# The config and data variables are the same ones the server itself reads, so
# a layout chosen here is the layout it will use when started by hand too.

set -eu

REPO="adnanjpg/remon-server"
PREFIX="${REMON_PREFIX:-/usr/local}"
BIN_DIR="$PREFIX/bin"
CONFIG_DIR="${REMON_CONFIG_DIR:-/etc/remon}"
DATA_DIR="${REMON_DATA_DIR:-/var/lib/remon}"
SERVICE_NAME="remon-server"
UNIT_PATH="/etc/systemd/system/$SERVICE_NAME.service"
INITD_PATH="/etc/init.d/$SERVICE_NAME"

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

# The shipped unit and init script are written for the default layout. When
# REMON_PREFIX points somewhere else, rewrite the paths in the copy we just
# installed rather than shipping a template nobody can read.
retarget() { # retarget <installed-file>
    sed -i \
        -e "s|/usr/local/bin/remon-server|$BIN_DIR/remon-server|g" \
        -e "s|/etc/remon|$CONFIG_DIR|g" \
        -e "s|/var/lib/remon|$DATA_DIR|g" \
        "$1"

    # systemd's StateDirectory always resolves under /var/lib, so it would
    # create and hand out a directory the server is not using. The installer
    # has already made the real one.
    if [ "$DATA_DIR" != "/var/lib/remon" ]; then
        sed -i -e '/^StateDirectory/d' -e '/^StateDirectoryMode/d' "$1"
    fi
}

# Still up, whichever init is managing it. Used to stop waiting on /health the
# moment the service has given up instead of burning the full timeout.
service_alive() {
    case "$INIT" in
        systemd) systemctl is-active --quiet "$SERVICE_NAME" ;;
        openrc)  rc-service --quiet "$SERVICE_NAME" status >/dev/null 2>&1 ;;
        *)       return 1 ;;
    esac
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

# INIT is "systemd", "openrc" or "" — the same two backends the server itself
# drives through its service endpoints.
INIT=""
if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
    INIT=systemd
elif command -v rc-service >/dev/null 2>&1 && command -v rc-update >/dev/null 2>&1; then
    INIT=openrc
fi

was_running=0
case "$INIT" in
    systemd)
        if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
            was_running=1
            step "Stopping $SERVICE_NAME for upgrade"
            systemctl stop "$SERVICE_NAME"
        fi
        ;;
    openrc)
        if rc-service --quiet "$SERVICE_NAME" status >/dev/null 2>&1; then
            was_running=1
            step "Stopping $SERVICE_NAME for upgrade"
            rc-service "$SERVICE_NAME" stop >/dev/null
        fi
        ;;
esac

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

if [ -z "$INIT" ]; then
    say ""
    warn "no supported init system detected — installed the binary only"
    say "Run it with: ${BOLD}remon-server --config-dir $CONFIG_DIR --data-dir $DATA_DIR${RESET}"
    exit 0
fi

if [ "$INIT" = "openrc" ]; then
    step "Installing OpenRC service"
    # Normally present, but minimal images have surprised us before.
    mkdir -p "$(dirname "$INITD_PATH")"
    if [ -f "$TMP/remon-server.openrc" ]; then
        install -m 0755 "$TMP/remon-server.openrc" "$INITD_PATH"
        retarget "$INITD_PATH"
    else
        # Unquoted heredoc so the install paths interpolate; OpenRC's own
        # runtime variables are escaped so they survive to the service.
        cat > "$INITD_PATH" <<OPENRC
#!/sbin/openrc-run

name="remon-server"
description="Remon monitoring server"
command="$BIN_DIR/remon-server"
command_args="--config-dir $CONFIG_DIR --data-dir $DATA_DIR"
supervisor="supervise-daemon"
respawn_delay=5
respawn_max=0
supervise_daemon_args="--stdout /var/log/\${RC_SVCNAME}.log --stderr /var/log/\${RC_SVCNAME}.log"
retry="SIGTERM/30"

depend() {
	need net
	after firewall
}

start_pre() {
	checkpath --directory --mode 0755 --owner root:root $CONFIG_DIR
	checkpath --directory --mode 0700 --owner root:root $DATA_DIR
	checkpath --file --mode 0640 --owner root:root "/var/log/\${RC_SVCNAME}.log"
}
OPENRC
        chmod 0755 "$INITD_PATH"
    fi

    rc-update add "$SERVICE_NAME" default >/dev/null 2>&1 || true

    step "Starting $SERVICE_NAME"
    rc-service "$SERVICE_NAME" restart >/dev/null
else

step "Installing systemd unit"
mkdir -p "$(dirname "$UNIT_PATH")"
if [ -f "$TMP/remon-server.service" ]; then
    install -m 0644 "$TMP/remon-server.service" "$UNIT_PATH"
    retarget "$UNIT_PATH"
else
    cat > "$UNIT_PATH" <<UNIT
[Unit]
Description=Remon monitoring server
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
ExecStart=$BIN_DIR/remon-server --config-dir $CONFIG_DIR --data-dir $DATA_DIR
Restart=always
RestartSec=5s
StartLimitIntervalSec=0
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

fi

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
    service_alive || break
    i=$((i + 1))
    sleep 1
done

# `hostname -I` is absent on busybox and on some minimal images, and the route
# lookup is absent without iproute2 — fall back rather than print "http://:8080".
host_addr=$(hostname -I 2>/dev/null | awk '{print $1}')
[ -n "$host_addr" ] || host_addr=$(ip -4 route get 1 2>/dev/null | sed -n 's/.*src \([0-9.]*\).*/\1/p')
[ -n "$host_addr" ] || host_addr="127.0.0.1"

# Where the pairing code will show up, and how to look at the service — both
# differ per init, and both are the next thing the operator needs.
if [ "$INIT" = "openrc" ]; then
    log_cmd="tail -f /var/log/$SERVICE_NAME.log"
    status_cmd="rc-service $SERVICE_NAME status"
    recent_cmd="tail -n 50 /var/log/$SERVICE_NAME.log"
else
    log_cmd="journalctl -fu $SERVICE_NAME"
    status_cmd="systemctl status $SERVICE_NAME"
    recent_cmd="journalctl -u $SERVICE_NAME -n 50"
fi

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
        say "  ${DIM}the 8-digit code appears in: $log_cmd${RESET}"
    fi
    say ""
    say "  ${DIM}config   $CONFIG_DIR/config.toml${RESET}"
    say "  ${DIM}data     $DATA_DIR${RESET}"
    say "  ${DIM}diagnose remon-server --config-dir $CONFIG_DIR --data-dir $DATA_DIR doctor${RESET}"
else
    warn "the service did not answer on http://127.0.0.1:$port/health"
    say ""
    say "  ${BOLD}$status_cmd${RESET}"
    say "  ${BOLD}$recent_cmd${RESET}"
    say "  ${BOLD}remon-server --config-dir $CONFIG_DIR --data-dir $DATA_DIR doctor${RESET}"
    exit 1
fi
