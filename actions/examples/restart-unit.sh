#!/bin/sh
# Restart a unit named by the firing alert's labels.
#
# The built-in catalogue already covers "restart nginx.service" — a binding of
# kind=service, verb=restart, target=nginx.service needs no script at all. This
# example is for the case the catalogue cannot express: the unit to act on is
# not known when the binding is written, it comes out of the alert itself.
#
# Typical rule:
#   service.active{unit="worker@1.service"} < 1
# every label on the firing sample arrives as REMON_LABEL_<KEY>, so one binding
# on one rule covers every worker.
#
# Environment the runner provides
# -------------------------------
#   REMON_EVENT        fired | resolved | manual
#   REMON_RULE         rule name          REMON_RULE_ID    rule id
#   REMON_SEVERITY     warn | crit        REMON_VALUE      the metric value
#   REMON_LABELS       label set as JSON
#   REMON_LABEL_<KEY>  one per identifier-shaped label key, uppercased
#   REMON_ACTION       this action's name REMON_RUN_ID     the run row's id
#   REMON_MODE         manual | auto | dry_run
#   REMON_SERVER_NAME  this host's configured name
#
# Exit 0 means the remediation succeeded. Any other exit — or a timeout — is a
# failure, counts against the binding's failure_limit, and after enough of them
# in a row the binding disarms itself and says so.
#
# @action name=restart-unit
# @action description=Restart the systemd unit named by the alert's labels
# @action timeout_ms=60000
# @action platforms=linux

set -eu

UNIT="${REMON_LABEL_UNIT:-}"
if [ -z "$UNIT" ]; then
  echo "rule '$REMON_RULE' fired without a 'unit' label; nothing to restart" >&2
  echo "labels were: ${REMON_LABELS:-{}}" >&2
  exit 1
fi

# Only act on the problem starting. Bind this with on_event=fired (the
# default); the guard is here too so a mis-set binding cannot restart a unit
# at the moment it recovers.
if [ "${REMON_EVENT:-fired}" != "fired" ]; then
  echo "event is '$REMON_EVENT', not a fire; standing down"
  exit 0
fi

# Refuse to touch the monitor itself. The server refuses this too, at bind
# time and again at run time, but a script that could be copied elsewhere
# should not rely on someone else's check.
case "$UNIT" in
  remon-server|remon-server.service)
    echo "refusing to restart the monitoring agent itself" >&2
    exit 1
    ;;
esac

echo "rule '$REMON_RULE' ($REMON_SEVERITY) on $REMON_SERVER_NAME: restarting $UNIT"
systemctl restart "$UNIT"

# Give it a moment to fail, so a unit that restarts straight back into a crash
# loop is reported as a failed remediation rather than a successful one.
sleep 3
if systemctl is-active --quiet "$UNIT"; then
  echo "$UNIT is active again"
  exit 0
fi

echo "$UNIT did not come back up; leaving it to a human" >&2
systemctl status "$UNIT" --no-pager --lines 20 >&2 || true
exit 1
