#!/bin/sh
# Demo of an OS adapter. The first update asks for a reboot (exit 10), later updates find nothing to do.
set -eu
state="$UPKEEP_STATE_DIR"
echo "$1 job=$UPKEEP_JOB_ID" >> "$state/actions.log"

say() { printf '{"type":"log","level":"info","message":"%s"}\n' "$1"; }

case "$1" in
  snapshot)
    say "snapshot taken"
    printf '{"type":"result","snapshot":"demo-snap-job%s"}\n' "$UPKEEP_JOB_ID"
    ;;
  upload)
    say "snapshot uploaded"
    printf '{"type":"result","uploaded":"demo-store/demo-snap-job%s"}\n' "$UPKEEP_JOB_ID"
    ;;
  update)
    if [ -e "$state/updated" ]; then
      say "already up to date"
      exit 0
    fi
    touch "$state/updated"
    say "packages updated, reboot needed"
    exit 10
    ;;
  restore)
    say "snapshot restored"
    ;;
  verify)
    say "boot id $(cat /proc/sys/kernel/random/boot_id)"
    ;;
  *)
    echo "unknown action: $1" >&2
    exit 2
    ;;
esac
