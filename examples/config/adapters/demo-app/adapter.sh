#!/bin/sh
# Demo of a data-writing app. Create $UPKEEP_STATE_DIR/fail-backup to make backup fail,
# or $UPKEEP_STATE_DIR/broken to make verify and the ping fail.
set -eu
state="$UPKEEP_STATE_DIR"
echo "$1 job=$UPKEEP_JOB_ID" >> "$state/actions.log"

say() { printf '{"type":"log","level":"info","message":"%s"}\n' "$1"; }

case "$1" in
  maintenance_on)
    touch "$state/maintenance"
    say "maintenance on"
    ;;
  maintenance_off)
    rm -f "$state/maintenance"
    say "maintenance off"
    ;;
  backup)
    if [ -e "$state/fail-backup" ]; then
      echo "backup failed: disk full (simulated)" >&2
      exit 1
    fi
    for percent in 25 50 75 100; do
      printf '{"type":"progress","percent":%s,"message":"copying data"}\n' "$percent"
    done
    file="$state/backup-job$UPKEEP_JOB_ID.txt"
    date > "$file"
    printf '{"type":"result","backup":"%s"}\n' "$file"
    ;;
  verify)
    if [ -e "$state/broken" ]; then
      echo "app does not answer" >&2
      exit 1
    fi
    say "app answers"
    ;;
  *)
    echo "unknown action: $1" >&2
    exit 2
    ;;
esac
