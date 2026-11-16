#!/usr/bin/env bash
# Boots an Ubuntu cloud image in qemu, installs upkeep as a systemd service and checks
# the step 1 acceptance list on it, with a real reboot in the middle of a job.
# Usage: tests/vm/run.sh <work-dir> <ubuntu-cloud-image.img>
set -euo pipefail

source "$(dirname "$0")/lib.sh" "$1" "$2"

step_is() { vm sudo upkeep job "$1" | grep -qE "^ +main +[0-9]+  $3 +[a-z0-9_-]+:$2\$"; }

trap stop_vm EXIT

boot_vm
install_upkeep "$repo/examples/config"

log "1+2. os-update plan runs end to end, with a real reboot in the middle"
boot_before=$(boot_id)
vm sudo upkeep run os-update --confirm
wait_for "the VM to reboot" 240 rebooted_since "$boot_before"
echo "boot id before: $boot_before, after: $(boot_id)"
expect_job 1 succeeded
vm "sudo grep -E 'reboot requested|runner started|job finished|boot id' /var/log/upkeep/jobs/1/job.log"
vm "sudo grep -q 'host rebooted: continuing' /var/log/upkeep/jobs/1/job.log"
echo "demo-os actions:"; vm sudo cat /var/lib/upkeep/adapters/demo-os/actions.log
[ "$(vm "sudo grep -c '^update job=1' /var/lib/upkeep/adapters/demo-os/actions.log")" = 1 ]

log "3. a failing step runs on_failure"
vm sudo touch /var/lib/upkeep/adapters/demo-app/fail-backup
vm sudo upkeep run os-update --confirm
expect_job 2 failed
vm sudo rm /var/lib/upkeep/adapters/demo-app/fail-backup

log "add a slow test adapter"
vm 'sudo mkdir -p /etc/upkeep/adapters/slow && sudo tee /etc/upkeep/adapters/slow/adapter.toml >/dev/null <<TOML
name = "slow"
run = "sh adapter.sh"
[actions.wait]
timeout = "2m"
[actions.after]
[actions.cleanup]
TOML
sudo tee /etc/upkeep/adapters/slow/adapter.sh >/dev/null <<SCRIPT
echo "\$1 job=\$UPKEEP_JOB_ID" >> "\$UPKEEP_STATE_DIR/actions.log"
[ "\$1" = wait ] && sleep 12
exit 0
SCRIPT
printf "steps = [\"slow:wait\", \"slow:after\"]\non_failure = [\"slow:cleanup\"]\n" | sudo tee /etc/upkeep/plans/slow.toml >/dev/null'

log "4. cancel stops after the current step and runs on_failure"
vm sudo upkeep run slow
wait_for "step wait to run" 30 step_is 3 wait running
vm sudo upkeep cancel 3
expect_job 3 cancelled

log "5. killing the runner mid-step re-attaches without a second run"
vm sudo upkeep run slow
wait_for "step wait to run" 30 step_is 4 wait running
vm "systemctl list-units --no-legend 'upkeep-job*'"
vm sudo systemctl kill -s KILL upkeep
echo "runner killed; step unit after the kill:"
vm "systemctl list-units --no-legend 'upkeep-job4*'"
expect_job 4 succeeded
vm "sudo grep -E 're-attached|runner restarted' /var/log/upkeep/jobs/4/job.log"
[ "$(vm "sudo grep -c '^wait job=4' /var/lib/upkeep/adapters/slow/actions.log")" = 1 ]
[ "$(vm "sudo grep -c '^after job=4' /var/lib/upkeep/adapters/slow/actions.log")" = 1 ]

log "all VM checks passed"
vm sudo upkeep jobs
