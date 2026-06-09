#!/bin/sh
# Peer daemon health probe.
#
# A daemon cannot report its own death — in a multi-server setup, have each
# remon-server watch a sibling's /health endpoint so a dead host still
# produces an alert from somewhere.  Wire up an alert rule
# (metric_type=probe, target=peer-health, metric_field=up, condition lt 1)
# to get notified when the peer stops answering.
#
# Configuration
# -------------
# Change PEER_URL below.  To watch multiple peers, copy this file with a
# different name and a different PEER_URL value, then register each copy
# as a separate probe.
#
# @probe name=peer-health
# @probe description=Sibling remon-server reachability check
# @probe enabled=false
# @probe interval=1m
# @probe timeout_ms=10000
# @probe platforms=linux,macos

set -eu

PEER_URL="https://peer.example.com:8080"

START_MS=$(date +%s%3N 2>/dev/null || echo 0)
if curl --fail --silent --show-error --max-time 5 "${PEER_URL}/health" >/dev/null 2>&1; then
  END_MS=$(date +%s%3N 2>/dev/null || echo 0)
  LATENCY=$((END_MS - START_MS))
  printf '{"message":"peer %s is up","metrics":[{"name":"up","value":1,"labels":{"peer":"%s"}},{"name":"latency_ms","value":%d,"unit":"ms","labels":{"peer":"%s"}}]}\n' \
    "$PEER_URL" "$PEER_URL" "$LATENCY" "$PEER_URL"
else
  printf '{"message":"peer %s is unreachable","metrics":[{"name":"up","value":0,"labels":{"peer":"%s"}}]}\n' \
    "$PEER_URL" "$PEER_URL"
  exit 1
fi
