#!/bin/sh
# Reclaim disk space on the filesystem a disk alert is firing about.
#
# The shape most worth automating: the remediation is safe, boring, and needs
# doing at 3am — vacuum the journal, prune dangling container images, clear a
# cache directory. Nothing here deletes anything a person chose to keep.
#
# Pair it with:
#   disk.used_percent{mount_point="/"} > 90     for 5m
# and bind with on_event=fired. Start the binding in `dry_run`, read a week of
# skipped rows in GET /actions/runs, and only then promote it to `manual` (a
# confirm on your phone) or `auto`.
#
# @action name=reclaim-disk
# @action description=Vacuum journals and prune unused images when a disk fills
# @action timeout_ms=120000
# @action platforms=linux

set -eu

MOUNT="${REMON_LABEL_MOUNT_POINT:-/}"
echo "rule '$REMON_RULE' at ${REMON_VALUE:-?}% on $MOUNT — reclaiming"

before=$(df -Pk "$MOUNT" | awk 'NR==2 {print $4}')

# 1. Journals. Bounded by age, not by "delete everything": the logs covering
#    the incident that triggered this are the ones you will want tomorrow.
if command -v journalctl >/dev/null 2>&1; then
  journalctl --vacuum-time=7d >/dev/null 2>&1 || echo "journal vacuum failed" >&2
fi

# 2. Dangling images and stopped containers. `--filter until=24h` keeps
#    anything recent, so a deploy in progress is not pulled out from under.
if command -v docker >/dev/null 2>&1; then
  docker image prune --force --filter "until=24h" >/dev/null 2>&1 \
    || echo "image prune failed" >&2
fi

# 3. Package manager caches — pure cache, refetched on demand.
if command -v apt-get >/dev/null 2>&1; then
  apt-get clean >/dev/null 2>&1 || true
fi

after=$(df -Pk "$MOUNT" | awk 'NR==2 {print $4}')
freed_mb=$(( (after - before) / 1024 ))
used_pct=$(df -P "$MOUNT" | awk 'NR==2 {gsub(/%/,"",$5); print $5}')

echo "freed ${freed_mb}MB on $MOUNT; now at ${used_pct}% used"

# Report failure when the cleanup did not actually move the needle. A
# remediation that "succeeded" while the disk stayed full is worse than one
# that failed: it resets the failure counter and buys the problem another
# cooldown of silence.
if [ "$used_pct" -ge 90 ]; then
  echo "$MOUNT is still at ${used_pct}%; this needs a human" >&2
  exit 1
fi
