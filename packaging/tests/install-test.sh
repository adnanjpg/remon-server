#!/usr/bin/env bash
# Exercises packaging/install.sh against stubbed system commands.
#
#   bash packaging/tests/install-test.sh
#
# This does not prove the service starts — that needs a real host. It proves
# the parts that are pure script, which is where the bugs have been: init
# detection, checksum gating, `config check` gating, which files get written
# with which paths substituted into them, that an upgrade re-run leaves an
# operator's config alone, and that a release predating the bundled unit files
# still installs from the built-in fallbacks.
#
# Runs anywhere bash does, including from a Windows checkout, which is the
# point — the installer is the artifact hardest to test on the machine it is
# written on.
#
# The two absolute system paths (/run/systemd/system, /etc/init.d) cannot be
# stubbed, so the script under test is a copy with those redirected into the
# sandbox. Everything else is the real thing.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ROOT="${TMPDIR:-/tmp}/remon-install-test"
rm -rf "$ROOT"; mkdir -p "$ROOT"

PASS=0; FAIL=0
ok()    { printf '  \033[32mok\033[0m    %s\n' "$1"; PASS=$((PASS+1)); }
bad()   { printf '  \033[31mFAIL\033[0m  %s\n' "$1"; FAIL=$((FAIL+1)); }
check() { if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }

# ── build a release tarball exactly as the workflow does ──────────────────
STAGE="$ROOT/stage/remon-server-linux-amd64"
mkdir -p "$STAGE" "$ROOT/serve"

cat > "$STAGE/remon-server" <<'FAKE'
#!/bin/sh
for a in "$@"; do
  case "$a" in
    --version) echo "remon-server 0.18.0"; exit 0 ;;
    check)     echo "configuration ok"; exit 0 ;;
  esac
done
exit 0
FAKE
chmod +x "$STAGE/remon-server"

cp "$REPO_DIR/packaging/remon-server.service" "$STAGE/"
cp "$REPO_DIR/packaging/remon-server.openrc"  "$STAGE/"
cp "$REPO_DIR/config/default.toml"            "$STAGE/config.toml.sample"

( cd "$ROOT/stage" && tar -czf ../serve/remon-server-linux-amd64.tar.gz \
    -C remon-server-linux-amd64 . )
( cd "$ROOT/serve" && sha256sum remon-server-linux-amd64.tar.gz > SHA256SUMS )

# ── script under test, with the unstubbable paths redirected ──────────────
SUT="$ROOT/install-sut.sh"
sed -e "s#/run/systemd/system#$ROOT/probe/systemd#g" \
    -e "s#/etc/systemd/system#$ROOT/target/etc/systemd/system#g" \
    -e "s#/etc/init.d#$ROOT/target/etc/init.d#g" \
    "$REPO_DIR/packaging/install.sh" > "$SUT"

# ── stubs ─────────────────────────────────────────────────────────────────
STUB="$ROOT/stub"; mkdir -p "$STUB"

cat > "$STUB/id" <<'S'
#!/bin/sh
[ "${1:-}" = "-u" ] && { echo 0; exit 0; }
exit 0
S

cat > "$STUB/curl" <<S
#!/bin/sh
out=""; url=""
while [ \$# -gt 0 ]; do
  case "\$1" in
    -o) out="\$2"; shift 2 ;;
    -*) shift ;;
    *)  url="\$1"; shift ;;
  esac
done
case "\$url" in
  */releases/latest) echo '{"tag_name": "v0.18.0"}'; exit 0 ;;
  */health) [ -f "$ROOT/state/healthy" ] && { echo '{"status":"ok"}'; exit 0; }; exit 7 ;;
esac
name=\$(basename "\$url")
[ -f "$ROOT/serve/\$name" ] || exit 22
if [ -n "\$out" ]; then cp "$ROOT/serve/\$name" "\$out"; else cat "$ROOT/serve/\$name"; fi
S

cat > "$STUB/hostname" <<'S'
#!/bin/sh
echo "10.0.0.5"
S

# Git Bash reports MINGW64_NT; the installer is right to refuse that.
cat > "$STUB/uname" <<'S'
#!/bin/sh
case "${1:-}" in
  -m) echo "x86_64" ;;
  *)  echo "Linux" ;;
esac
S

# NTFS cannot take `install -m 0700`, which is a property of this sandbox and
# not of the installer. Keep the copy semantics, drop the mode.
cat > "$STUB/install" <<'S'
#!/bin/sh
dir_mode=0
args=""
while [ $# -gt 0 ]; do
  case "$1" in
    -d) dir_mode=1; shift ;;
    -m) shift 2 ;;
    *)  args="$args $1"; shift ;;
  esac
done
# shellcheck disable=SC2086
set -- $args
if [ "$dir_mode" -eq 1 ]; then
  mkdir -p "$@"
else
  src="$1"; shift
  mkdir -p "$(dirname "$1")"
  cp "$src" "$1"
  chmod +x "$1" 2>/dev/null || true
fi
S

cat > "$STUB/systemctl" <<S
#!/bin/sh
echo "systemctl \$*" >> "$ROOT/state/init.log"
case "\$1" in
  is-active) [ -f "$ROOT/state/running" ] ;;
  restart|start) touch "$ROOT/state/running" "$ROOT/state/healthy" ;;
  *) exit 0 ;;
esac
S

cat > "$STUB/rc-service" <<S
#!/bin/sh
echo "rc-service \$*" >> "$ROOT/state/init.log"
[ "\$1" = "--quiet" ] && shift
case "\$2" in
  status) [ -f "$ROOT/state/running" ] ;;
  restart|start) touch "$ROOT/state/running" "$ROOT/state/healthy" ;;
  *) exit 0 ;;
esac
S

cat > "$STUB/rc-update" <<S
#!/bin/sh
echo "rc-update \$*" >> "$ROOT/state/init.log"
exit 0
S

chmod +x "$STUB"/*

run_install() { # run_install systemd|openrc
    rm -rf "$ROOT/state" "$ROOT/target" "$ROOT/probe"
    mkdir -p "$ROOT/state" "$ROOT/target"
    # systemd is detected by the presence of its runtime directory.
    [ "$1" = "systemd" ] && mkdir -p "$ROOT/probe/systemd"
    env -i \
        PATH="$STUB:/usr/bin:/bin" HOME="$ROOT" \
        REMON_PREFIX="$ROOT/target/usr/local" \
        REMON_CONFIG_DIR="$ROOT/target/etc/remon" \
        REMON_DATA_DIR="$ROOT/target/var/lib/remon" \
        NO_COLOR=1 \
        sh "$SUT" 2>&1
}

# ── systemd ───────────────────────────────────────────────────────────────
printf '\n\033[1msystemd branch\033[0m\n'
out=$(run_install systemd) || true
UNIT="$ROOT/target/etc/systemd/system/remon-server.service"
check "detected systemd, not OpenRC"    'grep -q "systemctl" "$ROOT/state/init.log"'
check "installed the unit"              '[ -f "$UNIT" ]'
check "unit points at the real binary"  'grep -q "ExecStart=$ROOT/target/usr/local/bin/remon-server" "$UNIT"'
check "unit points at the real dirs"    'grep -q -- "--config-dir $ROOT/target/etc/remon" "$UNIT"'
check "dropped a misleading StateDir"   '! grep -q "^StateDirectory" "$UNIT"'
check "enabled the service"             'grep -q "systemctl enable" "$ROOT/state/init.log"'
check "started the service"             'grep -q "systemctl restart" "$ROOT/state/init.log"'
check "installed the binary"            '[ -x "$ROOT/target/usr/local/bin/remon-server" ]'
check "wrote a config"                  '[ -f "$ROOT/target/etc/remon/config.toml" ]'
check "reported journalctl for logs"    'grep -q "journalctl -fu remon-server" <<<"$out"'
check "reported success"                'grep -q "is running" <<<"$out"'

# ── OpenRC ────────────────────────────────────────────────────────────────
printf '\n\033[1mOpenRC branch\033[0m\n'
out=$(run_install openrc) || true
INITD="$ROOT/target/etc/init.d/remon-server"
check "fell through to OpenRC"          'grep -q "rc-service" "$ROOT/state/init.log"'
check "installed the init script"       '[ -x "$INITD" ]'
check "init points at the real binary"  'grep -q "command=\"$ROOT/target/usr/local/bin/remon-server\"" "$INITD"'
check "init points at the real dirs"    'grep -q -- "--config-dir $ROOT/target/etc/remon" "$INITD"'
check "kept OpenRC runtime variables"   'grep -q "RC_SVCNAME" "$INITD"'
check "retargeted checkpath too"        '! grep -qE "checkpath.* /etc/remon$" "$INITD"'
check "added to the default runlevel"   'grep -q "rc-update add remon-server" "$ROOT/state/init.log"'
check "started the service"             'grep -q "rc-service remon-server restart" "$ROOT/state/init.log"'
check "reported the log file for logs"  'grep -q "tail -f /var/log/remon-server.log" <<<"$out"'
check "reported success"                'grep -q "is running" <<<"$out"'

# ── upgrade re-run keeps the operator's config ────────────────────────────
printf '\n\033[1mupgrade re-run\033[0m\n'
run_install systemd >/dev/null || true
echo "# operator edit" >> "$ROOT/target/etc/remon/config.toml"
out=$(env -i PATH="$STUB:/usr/bin:/bin" HOME="$ROOT" \
    REMON_PREFIX="$ROOT/target/usr/local" \
    REMON_CONFIG_DIR="$ROOT/target/etc/remon" \
    REMON_DATA_DIR="$ROOT/target/var/lib/remon" NO_COLOR=1 \
    sh "$SUT" 2>&1) || true
check "kept the edited config"          'grep -q "operator edit" "$ROOT/target/etc/remon/config.toml"'
check "said it kept it"                 'grep -q "kept existing" <<<"$out"'
check "reported an in-place upgrade"    'grep -q "upgraded in place" <<<"$out"'

# ── a bad checksum must stop the install ──────────────────────────────────
printf '\n\033[1mchecksum gate\033[0m\n'
echo "0000000000000000000000000000000000000000000000000000000000000000  remon-server-linux-amd64.tar.gz" \
    > "$ROOT/serve/SHA256SUMS"
rm -rf "$ROOT/target"; mkdir -p "$ROOT/target"
out=$(run_install systemd) && rc=0 || rc=$?
check "refused to install"              '[ "${rc:-0}" -ne 0 ]'
check "said why"                        'grep -q "checksum mismatch" <<<"$out"'
check "installed nothing"               '[ ! -e "$ROOT/target/usr/local/bin/remon-server" ]'

# ── a tarball from before the unit files shipped ──────────────────────────
# Existing releases carry only the binary, so the built-in fallbacks are the
# path a user upgrading from one of them actually takes.
printf '\n\033[1mlegacy tarball (no unit files)\033[0m\n'
LEGACY="$ROOT/stage-legacy/remon-server-linux-amd64"
mkdir -p "$LEGACY"
cp "$STAGE/remon-server" "$LEGACY/"
( cd "$ROOT/stage-legacy" && tar -czf ../serve/remon-server-linux-amd64.tar.gz \
    -C remon-server-linux-amd64 . )
( cd "$ROOT/serve" && sha256sum remon-server-linux-amd64.tar.gz > SHA256SUMS )

out=$(run_install systemd) || true
check "generated a unit from scratch"   '[ -f "$UNIT" ]'
check "generated unit has real paths"   'grep -q "ExecStart=$ROOT/target/usr/local/bin/remon-server" "$UNIT"'
check "generated unit is complete"      'grep -q "WantedBy=multi-user.target" "$UNIT"'
check "wrote a config from scratch"     'grep -q "allow_any_origin" "$ROOT/target/etc/remon/config.toml"'
check "still reported success"          'grep -q "is running" <<<"$out"'

out=$(run_install openrc) || true
check "generated an init from scratch"  '[ -x "$INITD" ]'
check "generated init has real paths"   'grep -q "command=\"$ROOT/target/usr/local/bin/remon-server\"" "$INITD"'
check "generated init kept RC_SVCNAME"  'grep -q "RC_SVCNAME" "$INITD"'
check "generated init has depend()"     'grep -q "need net" "$INITD"'

printf '\n\033[1mResults\033[0m\n'
printf '  %d passed, %d failed\n\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
