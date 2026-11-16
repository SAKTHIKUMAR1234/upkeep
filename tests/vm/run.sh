#!/usr/bin/env bash
# Boots an Ubuntu cloud image in qemu, installs upkeep as a systemd service and checks
# the step 1 acceptance list on it, with a real reboot in the middle of a job.
# Usage: tests/vm/run.sh <work-dir> <ubuntu-cloud-image.img>
set -euo pipefail

work=$(realpath "$1")
base=$(realpath "$2")
repo=$(cd "$(dirname "$0")/../.." && pwd)
port=${UPKEEP_VM_SSH_PORT:-22422}
key="$work/id_ed25519"

log() { printf '\n=== %s\n' "$*"; }
vm() { ssh -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o BatchMode=yes -i "$key" -p "$port" ubuntu@127.0.0.1 "$@"; }
copy() { scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i "$key" -P "$port" -r "$@"; }

wait_for() {
  local what=$1 seconds=$2; shift 2
  for _ in $(seq "$seconds"); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  echo "FAILED: timed out waiting for $what" >&2
  return 1
}

boot_id() { vm cat /proc/sys/kernel/random/boot_id; }
rebooted_since() { local now; now=$(boot_id) && [ -n "$now" ] && [ "$now" != "$1" ]; }
job_status() { vm sudo upkeep job "$1" | head -1 | awk '{print $2}'; }
job_is() { [ "$(job_status "$1")" = "$2" ]; }
step_is() { vm sudo upkeep job "$1" | grep -qE "^ +main +[0-9]+  $3 +[a-z0-9_-]+:$2\$"; }
expect_job() {
  wait_for "job $1 to be $2" 120 job_is "$1" "$2"
  vm sudo upkeep job "$1"
}

stop_vm() {
  local pid
  pid=$(cat "$work/qemu.pid" 2>/dev/null) || return 0
  kill -0 "$pid" 2>/dev/null || return 0
  vm sudo poweroff || true
  for _ in $(seq 30); do kill -0 "$pid" 2>/dev/null || return 0; sleep 1; done
  kill "$pid" || true
}
trap stop_vm EXIT

log "build release binary"
(cd "$repo" && cargo build --release -j 6 -q)

log "prepare VM disk and cloud-init seed"
rm -f "$work/disk.qcow2" "$work/seed.iso" "$work/serial.log" "$key" "$key.pub"
ssh-keygen -q -t ed25519 -N '' -f "$key"
qemu-img create -q -f qcow2 -b "$base" -F qcow2 "$work/disk.qcow2" 8G
mkdir -p "$work/seed"
printf 'instance-id: upkeep-vm\nlocal-hostname: upkeep-vm\n' > "$work/seed/meta-data"
cat > "$work/seed/user-data" <<USERDATA
#cloud-config
users:
  - name: ubuntu
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    ssh_authorized_keys: ["$(cat "$key.pub")"]
ssh_pwauth: false
USERDATA
genisoimage -quiet -output "$work/seed.iso" -volid cidata -joliet -rock "$work/seed/user-data" "$work/seed/meta-data"

log "boot VM (2 vCPU, 2 GB)"
qemu-system-x86_64 -enable-kvm -cpu host -smp 2 -m 2048 -display none \
  -drive file="$work/disk.qcow2",if=virtio -drive file="$work/seed.iso",if=virtio,format=raw \
  -netdev user,id=net0,hostfwd=tcp:127.0.0.1:"$port"-:22 -device virtio-net-pci,netdev=net0 \
  -serial file:"$work/serial.log" -daemonize -pidfile "$work/qemu.pid"
wait_for "ssh" 240 vm true
wait_for "cloud-init" 240 vm sudo cloud-init status --wait
vm 'cat /etc/os-release | grep PRETTY_NAME'

log "install upkeep as a systemd service"
copy "$repo/target/release/upkeep" "$repo/examples/config" "$repo/packaging/upkeep.service" ubuntu@127.0.0.1:/tmp/
vm 'sudo install -m 755 /tmp/upkeep /usr/local/bin/upkeep \
  && sudo mkdir -p /etc/upkeep && sudo cp -r /tmp/config/. /etc/upkeep/ \
  && sudo install -m 644 /tmp/upkeep.service /etc/systemd/system/upkeep.service \
  && sudo systemctl daemon-reload && sudo systemctl enable --now upkeep'
wait_for "the runner socket" 30 vm sudo upkeep jobs
vm sudo upkeep adapters
vm sudo upkeep status

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
