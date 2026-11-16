# Shared helpers for the VM tests: boots an Ubuntu cloud image in qemu and installs upkeep.
# Usage from a test: source lib.sh <work-dir> <ubuntu-cloud-image.img>
work=$(realpath "$1")
base=$(realpath "$2")
repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
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
expect_job() {
  wait_for "job $1 to be $2" "${3:-120}" job_is "$1" "$2"
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

boot_vm() {
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
}

# Installs the upkeep binary, the adapters and the config folder $1 as /etc/upkeep, and starts the service.
install_upkeep() {
  log "install upkeep as a systemd service"
  copy "$repo/target/release/upkeep" "$repo/adapters" "$repo/packaging/upkeep.service" ubuntu@127.0.0.1:/tmp/
  copy "$1" ubuntu@127.0.0.1:/tmp/upkeep-config
  vm 'sudo install -m 755 /tmp/upkeep /usr/local/bin/upkeep \
    && sudo mkdir -p /usr/local/lib/upkeep /etc/upkeep && sudo cp -r /tmp/adapters /usr/local/lib/upkeep/ \
    && sudo cp -r /tmp/upkeep-config/. /etc/upkeep/ \
    && sudo install -m 644 /tmp/upkeep.service /etc/systemd/system/upkeep.service \
    && sudo systemctl daemon-reload && sudo systemctl enable --now upkeep'
  wait_for "the runner socket" 30 vm sudo upkeep jobs
  vm sudo upkeep adapters
  vm sudo upkeep status
}
