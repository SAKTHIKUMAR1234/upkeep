#!/usr/bin/env bash
# Boots a fresh Ubuntu VM and runs the ubuntu-os adapter for real through the runner:
# check_updates, discover, update (with a reboot when one is required) and verify.
# Usage: tests/vm/os-adapter.sh <work-dir> <ubuntu-cloud-image.img>
set -euo pipefail

source "$(dirname "$0")/lib.sh" "$1" "$2"

result_of() { vm "sudo sh -c 'cat /var/log/upkeep/jobs/$1/steps/main-0-*/stdout.jsonl'" | grep '"type": "result"'; }
auto_update_state() { vm 'systemctl is-active apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service || true'; }

trap stop_vm EXIT

boot_vm
install_upkeep "$repo/tests/vm/os-config"
vm sudo apt-mark hold curl

log "1. check_updates lists pending packages and marks risky ones"
vm sudo upkeep run os:check_updates
expect_job 1 succeeded 600
result_of 1 | python3 -c 'import json,sys; r=json.load(sys.stdin); print(len(r["upgrades"]), "to upgrade, risky:", r["risky"])'

log "2. discover drafts [[service]] entries"
vm sudo upkeep run os:discover
expect_job 2 succeeded
result_of 2 | python3 -c 'import json,sys; print(json.load(sys.stdin)["toml"])'

log "3. update runs, reboots when required, then verify passes"
auto_before=$(auto_update_state)
boot_before=$(boot_id)
vm sudo upkeep run os-update --confirm
expect_job 3 succeeded 1800
vm "sudo grep -E 'reboot|runner started|held|job finished' /var/log/upkeep/jobs/3/job.log | cut -c1-200"
result_of 3 | python3 -c 'import json,sys; r=json.load(sys.stdin); print(len(r["upgraded"]), "upgraded, held during update:", r["held"], "reboot:", r["reboot_required"])'
echo "boot id before: $boot_before, after: $(boot_id)"

log "4. hold and auto-update state are put back"
[ "$(vm apt-mark showhold)" = curl ]
[ "$(auto_update_state)" = "$auto_before" ]
echo "held after the update: $(vm apt-mark showhold); auto-update units: $(echo $auto_before)"

log "5. verify and a second check_updates on the updated host"
vm sudo upkeep run os:verify
expect_job 4 succeeded
vm sudo upkeep run os:check_updates
expect_job 5 succeeded 600
result_of 5 | python3 -c 'import json,sys; r=json.load(sys.stdin); print(len(r["upgrades"]), "left to upgrade")'

log "all VM checks passed"
vm sudo upkeep jobs
