#!/bin/sh
# Inline-header probe: reports free space on `/`.
#
# Move/copy this file to probes/disk-free-root.sh (drop the
# `examples/` parent), `chmod +x`, then either restart the server or
# POST /admin/probes/reload.
#
# Probes are PURE METRIC SOURCES in this codebase — no warn/crit
# severity here. Express thresholds via /alerts CRUD with
# metric_type=probe, target=disk-free-root, metric_field=free_pct.
#
# @probe name=disk-free-root
# @probe description=Free space on / (raw metric only — alert via /alerts)
# @probe enabled=false
# @probe interval=5m
# @probe timeout_ms=10000
# @probe platforms=linux,macos

set -eu

df_line=$(df -Pk / | awk 'NR==2')
total_kb=$(echo "$df_line" | awk '{print $2}')
used_kb=$(echo "$df_line"  | awk '{print $3}')
avail_kb=$(echo "$df_line" | awk '{print $4}')

free_pct=$(awk -v a="$avail_kb" -v t="$total_kb" \
  'BEGIN { if (t==0) print 0; else printf "%.2f", (a/t)*100 }')

avail_bytes=$((avail_kb * 1024))
used_bytes=$((used_kb * 1024))
total_bytes=$((total_kb * 1024))

# Output contract: ONE line of JSON. `metrics` is an array; each entry
# {name, value, unit?, labels?}. `labels` get used as part of the
# storage primary key, so two probes can emit the same metric_name with
# different labels and both persist.
printf '{"message":"%s%%%% free on /","metrics":[{"name":"available_bytes","value":%s,"unit":"bytes","labels":{"mount":"/"}},{"name":"used_bytes","value":%s,"unit":"bytes","labels":{"mount":"/"}},{"name":"total_bytes","value":%s,"unit":"bytes","labels":{"mount":"/"}},{"name":"free_pct","value":%s,"unit":"percent","labels":{"mount":"/"}}]}\n' \
  "$free_pct" "$avail_bytes" "$used_bytes" "$total_bytes" "$free_pct"
