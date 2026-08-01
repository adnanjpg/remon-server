#!/usr/bin/env bash
# Exercises packaging/uninstall.sh against stubbed system commands.
#
#   bash packaging/tests/uninstall-test.sh
#
# Same shape as install-test.sh and for the same reason: the parts that are
# pure script are where the bugs have been. The one that shipped was `--purge`
# honouring REMON_PREFIX while hardcoding the config and data directories, so
# on a host installed with REMON_CONFIG_DIR or REMON_DATA_DIR it deleted paths
# the installer had never written and left the real database in place — while
# reporting that it had removed "configuration and metrics history".
#
# Every absolute path the script touches is redirected into the sandbox,
# including the defaults, so the case that exercises them cannot reach a real
# /var/lib/remon.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ROOT="${TMPDIR:-/tmp}/remon-uninstall-test"

PASS=0; FAIL=0
ok()    { printf '  \033[32mok\033[0m    %s\n' "$1"; PASS=$((PASS+1)); }
bad()   { printf '  \033[31mFAIL\033[0m  %s\n' "$1"; FAIL=$((FAIL+1)); }
check() { if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }

TARGET=""; STUB=""; SUT=""; CALLS=""

# A sandbox holding one already-installed host, plus the stubs the script
# reaches for. Rebuilt per case so one removal cannot mask the next.
setup() {
    rm -rf "$ROOT"; mkdir -p "$ROOT"
    TARGET="$ROOT/target"; STUB="$ROOT/stub"; CALLS="$ROOT/calls.log"
    SUT="$ROOT/uninstall-sut.sh"
    mkdir -p "$STUB" "$ROOT/probe/systemd" \
             "$TARGET/usr/local/bin" "$TARGET/etc/systemd/system" \
             "$TARGET/etc/init.d" "$TARGET/etc/remon" \
             "$TARGET/var/lib/remon" "$TARGET/var/log"
    : > "$CALLS"

    # The installed tree an uninstall is supposed to find.
    echo "binary"  > "$TARGET/usr/local/bin/remon-server"
    echo "unit"    > "$TARGET/etc/systemd/system/remon-server.service"
    echo "initd"   > "$TARGET/etc/init.d/remon-server"
    echo "config"  > "$TARGET/etc/remon/config.toml"
    echo "metrics" > "$TARGET/var/lib/remon/monitor.sqlite3"
    echo "log"     > "$TARGET/var/log/remon-server.log"

    sed -e "s#/run/systemd/system#$ROOT/probe/systemd#g" \
        -e "s#/etc/systemd/system#$TARGET/etc/systemd/system#g" \
        -e "s#/etc/init.d#$TARGET/etc/init.d#g" \
        -e "s#/etc/remon#$TARGET/etc/remon#g" \
        -e "s#/var/lib/remon#$TARGET/var/lib/remon#g" \
        -e "s#/var/log#$TARGET/var/log#g" \
        -e "s#/usr/local#$TARGET/usr/local#g" \
        "$REPO_DIR/packaging/uninstall.sh" > "$SUT"

    cat > "$STUB/id" <<'S'
#!/bin/sh
[ "${1:-}" = "-u" ] && { echo "${FAKE_UID:-0}"; exit 0; }
exit 0
S
    # Records what it was asked to do; `is-active`/`is-enabled` answer from
    # state files so a case can present a running or a stopped unit.
    cat > "$STUB/systemctl" <<S
#!/bin/sh
echo "systemctl \$*" >> "$CALLS"
case "\$1" in
  is-active)  [ -f "$ROOT/state/active" ]  || exit 3 ;;
  is-enabled) [ -f "$ROOT/state/enabled" ] || exit 1 ;;
esac
exit 0
S
    cat > "$STUB/rc-service" <<S
#!/bin/sh
echo "rc-service \$*" >> "$CALLS"
case "\$*" in
  *status*) [ -f "$ROOT/state/active" ] || exit 3 ;;
esac
exit 0
S
    cat > "$STUB/rc-update" <<S
#!/bin/sh
echo "rc-update \$*" >> "$CALLS"
exit 0
S
    chmod +x "$STUB"/*
    mkdir -p "$ROOT/state"
}

# `init` picks which branch the script takes: systemd needs the probe
# directory to exist, openrc needs it gone and rc-service on PATH.
run_uninstall() {
    local init="$1"; shift
    if [ "$init" = systemd ]; then
        mkdir -p "$ROOT/probe/systemd"
        PATH="$STUB:/usr/bin:/bin" sh "$SUT" "$@" 2>&1
    else
        rm -rf "$ROOT/probe/systemd"
        PATH="$STUB:/usr/bin:/bin" sh "$SUT" "$@" 2>&1
    fi
}

printf '\n\033[1msystemd, without --purge\033[0m\n'
setup
touch "$ROOT/state/active" "$ROOT/state/enabled"
out=$(run_uninstall systemd) || true
check "stopped the unit"                'grep -q "systemctl stop" "$CALLS"'
check "disabled the unit"               'grep -q "systemctl disable" "$CALLS"'
check "removed the unit file"           '[ ! -f "$TARGET/etc/systemd/system/remon-server.service" ]'
check "reloaded the manager"            'grep -q "daemon-reload" "$CALLS"'
check "removed the binary"              '[ ! -f "$TARGET/usr/local/bin/remon-server" ]'
check "kept the config"                 '[ -f "$TARGET/etc/remon/config.toml" ]'
check "kept the database"               '[ -f "$TARGET/var/lib/remon/monitor.sqlite3" ]'
check "said what it kept"               'grep -q "Kept" <<<"$out"'

printf '\n\033[1msystemd, --purge\033[0m\n'
setup
out=$(run_uninstall systemd --purge) || true
check "removed the config dir"          '[ ! -d "$TARGET/etc/remon" ]'
check "removed the data dir"            '[ ! -d "$TARGET/var/lib/remon" ]'
check "removed the openrc log"          '[ ! -f "$TARGET/var/log/remon-server.log" ]'
check "said history went too"           'grep -q "metrics history" <<<"$out"'

# The regression that shipped: --purge read REMON_PREFIX but not these two, so
# it deleted the defaults and left the real install untouched.
printf '\n\033[1m--purge honours the directory overrides\033[0m\n'
setup
mkdir -p "$TARGET/srv/conf" "$TARGET/srv/data"
echo "config"  > "$TARGET/srv/conf/config.toml"
echo "metrics" > "$TARGET/srv/data/monitor.sqlite3"
out=$(REMON_CONFIG_DIR="$TARGET/srv/conf" REMON_DATA_DIR="$TARGET/srv/data" \
      run_uninstall systemd --purge) || true
check "removed the overridden config"   '[ ! -d "$TARGET/srv/conf" ]'
check "removed the overridden data"     '[ ! -d "$TARGET/srv/data" ]'
check "left the defaults alone"         '[ -f "$TARGET/etc/remon/config.toml" ]'
check "named what it removed"           'grep -q "$TARGET/srv/data" <<<"$out"'

printf '\n\033[1mopenrc\033[0m\n'
setup
touch "$ROOT/state/active"
out=$(run_uninstall openrc) || true
check "stopped the service"             'grep -q "rc-service remon-server stop" "$CALLS"'
check "removed it from the runlevel"    'grep -q "rc-update del" "$CALLS"'
check "removed the init script"         '[ ! -f "$TARGET/etc/init.d/remon-server" ]'
check "removed the binary"              '[ ! -f "$TARGET/usr/local/bin/remon-server" ]'

printf '\n\033[1mrefusals\033[0m\n'
setup
rc=0; FAKE_UID=1000 run_uninstall systemd >/dev/null 2>&1 || rc=$?
check "refused to run as non-root"      '[ "${rc:-0}" -ne 0 ]'
check "left the binary in place"        '[ -f "$TARGET/usr/local/bin/remon-server" ]'
setup
rc=0; run_uninstall systemd --wat >/dev/null 2>&1 || rc=$?
check "refused an unknown argument"     '[ "${rc:-0}" -ne 0 ]'
check "removed nothing"                 '[ -f "$TARGET/usr/local/bin/remon-server" ]'

printf '\n\033[1mResults\033[0m\n'
printf '  %d passed, %d failed\n\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
