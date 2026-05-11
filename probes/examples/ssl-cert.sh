#!/bin/sh
# TLS certificate expiry probe.
#
# Reports how many days remain before the certificate for a given host
# expires.  Wire up an alert rule (metric_type=probe, target=ssl-cert,
# metric_field=days_remaining, condition lt 30) to get notified before
# renewal is due.
#
# Configuration
# -------------
# Change HOST (and optionally PORT) below.  To monitor multiple domains,
# copy this file with a different name and a different HOST value, then
# register each copy as a separate probe.
#
# @probe name=ssl-cert
# @probe description=TLS certificate expiry check
# @probe enabled=false
# @probe interval=6h
# @probe timeout_ms=15000
# @probe platforms=linux,macos

set -eu

HOST="example.com"
PORT="443"

END_DATE=$(echo \
  | openssl s_client -connect "${HOST}:${PORT}" -servername "${HOST}" 2>/dev/null \
  | openssl x509 -noout -enddate 2>/dev/null \
  | cut -d= -f2)

if [ -z "$END_DATE" ]; then
  printf '{"message":"could not fetch cert for %s:%s","metrics":[]}\n' "$HOST" "$PORT"
  exit 1
fi

# Compute days remaining; handle both GNU date (Linux) and BSD date (macOS).
if date --version >/dev/null 2>&1; then
  EXPIRY_EPOCH=$(date -d "$END_DATE" +%s)
else
  EXPIRY_EPOCH=$(date -j -f "%b %d %H:%M:%S %Y %Z" "$END_DATE" +%s)
fi

NOW_EPOCH=$(date +%s)
DAYS=$(( (EXPIRY_EPOCH - NOW_EPOCH) / 86400 ))

printf '{"message":"%d days until %s cert expires","metrics":[{"name":"days_remaining","value":%d,"unit":"days","labels":{"host":"%s","port":"%s"}}]}\n' \
  "$DAYS" "$HOST" "$DAYS" "$HOST" "$PORT"
