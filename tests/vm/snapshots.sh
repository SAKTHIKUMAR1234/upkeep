#!/usr/bin/env bash
# Checks the step 3 acceptance list on real VMs, with this machine as the SFTP destination:
# full and incremental snapshots, folder and database restores, retention, password auth,
# and a full restore onto the empty disk of a second VM that then boots on its own.
# Usage: tests/vm/snapshots.sh <work-dir> <ubuntu-cloud-image.img> <backups-dir>
# Needs sshd on this machine and the address the VMs reach it on (UPKEEP_TEST_DEST_ADDRESS,
# default 100.64.0.21). Set RESTIC_ARCHIVE to a downloaded restic archive to skip the download.
set -euo pipefail

mkdir -p "$1/a" "$1/b" "$3"
top=$(realpath "$1")
backups=$(realpath "$3")
api_port=17878
port_b=22423
dest_address=${UPKEEP_TEST_DEST_ADDRESS:-100.64.0.21}
dest_user=$(id -un)
key_marker="upkeep-vm-test-$$"

UPKEEP_VM_NAME=upkeep-vm-a UPKEEP_VM_FORWARDS=",hostfwd=tcp:127.0.0.1:$api_port-:7878" \
  source "$(dirname "$0")/lib.sh" "$top/a" "$2"

client() { UPKEEP_CLIENT_DIR="$top/client" UPKEEP_RESTIC="$top/bin/restic" "$repo/target/release/upkeep" "$@"; }
vmb() { ssh -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o BatchMode=yes -i "$key" -p "$port_b" ubuntu@127.0.0.1 "$@"; }
copy_b() { scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -i "$key" -P "$port_b" -r "$@"; }
job_id() { sed -n 's/^job \([0-9]*\) started.*/\1/p' | head -1; }
snapshot_id() { sed -n 's/^snapshot \([0-9a-f]*\): .*/\1/p' | tail -1; }
checksums() { echo "cd '$1' && find . -type f -print0 | sort -z | xargs -0 sha256sum"; }
db_fingerprint() { "$1" "sudo mariadb -N -e 'SELECT COUNT(*), SUM(CRC32(CONCAT(id, item))) FROM shop.orders'"; }
snapshot_dates() { grep -E '^[0-9a-f]{8}  ' | awk '{print $2}'; }

boot_b() {
  local name=$1; shift
  qemu-system-x86_64 -enable-kvm -cpu host -smp 2 -m 2048 -display none "$@" \
    -netdev user,id=net0,hostfwd=tcp:127.0.0.1:"$port_b"-:22 -device virtio-net-pci,netdev=net0 \
    -serial file:"$top/b/$name.serial.log" -daemonize -pidfile "$top/b/qemu.pid"
  wait_for "ssh on VM B ($name)" 300 vmb true
}

stop_b() {
  local pid
  pid=$(cat "$top/b/qemu.pid" 2>/dev/null) || return 0
  kill -0 "$pid" 2>/dev/null || return 0
  vmb sudo poweroff || true
  for _ in $(seq 60); do kill -0 "$pid" 2>/dev/null || return 0; sleep 1; done
  kill "$pid" || true
}

cleanup() {
  stop_vm || true
  stop_b || true
  sed -i "/$key_marker/d" "$HOME/.ssh/authorized_keys"
}
trap cleanup EXIT

rm -rf "$top/client" "$top/restore-client" "$top/dest_key" "$top/dest_key.pub" "${backups:?}"/*

log "install the pinned restic on this machine"
sh "$repo/packaging/install-restic.sh" "$top/bin"

boot_vm

log "install upkeep with the TCP API, restic, MariaDB and test data on VM A"
rm -rf "$top/a-config" && cp -r "$repo/tests/vm/snapshot-config" "$top/a-config"
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$top/a-config/token"
install -m 600 "$top/a-config/token" "$top/vm-a.token"
install_upkeep "$top/a-config"
copy "$repo/packaging/install-restic.sh" ubuntu@127.0.0.1:/tmp/
if [ -n "${RESTIC_ARCHIVE:-}" ]; then copy "$RESTIC_ARCHIVE" ubuntu@127.0.0.1:/tmp/restic.bz2; fi
vm 'sudo env RESTIC_ARCHIVE=$( [ -e /tmp/restic.bz2 ] && echo /tmp/restic.bz2 ) sh /tmp/install-restic.sh'
vm 'sudo DEBIAN_FRONTEND=noninteractive apt-get update -q >/dev/null && sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -q mariadb-server >/dev/null'
vm 'sudo mariadb -e "CREATE DATABASE shop; USE shop; CREATE TABLE shop.orders (id INT PRIMARY KEY, item VARCHAR(40));
  INSERT INTO shop.orders SELECT seq, CONCAT(\"item-\", seq) FROM seq_1_to_1000;"'
vm 'sudo mkdir -p /srv/data/docs && sudo dd if=/dev/urandom of=/srv/data/blob1 bs=1M count=20 status=none \
  && seq 1 50000 | sudo tee /srv/data/docs/numbers.txt >/dev/null && echo first | sudo tee /srv/data/notes.txt >/dev/null'
echo "snapshot-1 $(date -Is)" | vm sudo tee /etc/upkeep-marker >/dev/null

log "register the host and the destination on the client; allow the test key on this machine for SFTP only"
ssh-keygen -q -t ed25519 -N '' -C "$key_marker" -f "$top/dest_key"
echo "restrict,from=\"127.0.0.1,$dest_address\",command=\"/usr/lib/openssh/sftp-server\" $(cat "$top/dest_key.pub")" >> "$HOME/.ssh/authorized_keys"
host_key_args=()
while read -r _ kind value; do host_key_args+=(--host-key "$kind $value"); done < <(ssh-keyscan -T 10 "$dest_address" 2>/dev/null)
client host add vm-a --url "http://127.0.0.1:$api_port" --token-file "$top/vm-a.token"
client dest add laptop --ssh "$dest_user@$dest_address" --path "$backups/repos" --key "$top/dest_key" "${host_key_args[@]}"
client host list
client dest list
client --host vm-a status

log "1. snapshot 1 (full) with MariaDB stopped"
vm sudo systemctl stop mariadb
client snapshot vm-a --to laptop | tee "$top/snapshot1.out"
vm sudo systemctl start mariadb
snapshot1=$(snapshot_id < "$top/snapshot1.out")

log "1. change a few files and rows, then snapshot 2 (incremental)"
vm 'sudo dd if=/dev/urandom of=/srv/data/blob2 bs=1M count=3 status=none && echo second | sudo tee -a /srv/data/notes.txt >/dev/null'
echo "snapshot-2 $(date -Is)" | vm sudo tee /etc/upkeep-marker >/dev/null
vm 'sudo mariadb shop -e "INSERT INTO orders SELECT seq, CONCAT(\"item-\", seq) FROM seq_1001_to_1500;"'
fingerprint2=$(db_fingerprint vm)
vm sudo systemctl stop mariadb
vm "sudo sh -c \"$(checksums /var/lib/mysql)\" | sudo tee /root/mysql-snapshot2.sha256 >/dev/null"
client snapshot vm-a --to laptop | tee "$top/snapshot2.out"
vm sudo systemctl start mariadb
snapshot2=$(snapshot_id < "$top/snapshot2.out")
client snapshots vm-a --dest laptop | tee "$top/snapshots.out"
[ "$(grep -c '^[0-9a-f]\{8\}  ' "$top/snapshots.out")" = 2 ]

log "2. restore /srv/data from snapshot 2 to this machine and compare checksums"
client restore vm-a "$snapshot2" --path /srv/data --to "$top/restore-client" --dest laptop
vm "sudo sh -c \"$(checksums /srv/data)\"" > "$top/srv-data.vm.sha256"
sh -c "$(checksums "$top/restore-client/srv/data")" > "$top/srv-data.restored.sha256"
diff "$top/srv-data.vm.sha256" "$top/srv-data.restored.sha256"
echo "/srv/data: $(wc -l < "$top/srv-data.restored.sha256") files match"

log "3. restore the stopped database files on VM A through the runner and compare checksums"
job=$(client --host vm-a run os:restore --dest laptop --param "snapshot=$snapshot2" --param path=/var/lib/mysql \
  --param target=/root/restore-db | job_id)
expect_job "$job" succeeded 600
vm "sudo sh -c \"$(checksums /root/restore-db/var/lib/mysql)\"" > "$top/mysql.restored.sha256"
vm sudo cat /root/mysql-snapshot2.sha256 | diff - "$top/mysql.restored.sha256"
echo "/var/lib/mysql: $(wc -l < "$top/mysql.restored.sha256") files match"
vm sudo rm -rf /root/restore-db

log "5. retention keeps 7 daily, 4 weekly and 12 monthly of many fake-dated snapshots"
client dest add laptop-ret --ssh "$dest_user@$dest_address" --path "$backups/retention" --key "$top/dest_key" "${host_key_args[@]}"
retention_repo="$backups/retention/vm-a"
export RESTIC_PASSWORD_FILE="$top/client/secrets/laptop-ret.repository-key" RESTIC_CACHE_DIR="$top/restic-cache"
"$top/bin/restic" -r "$retention_repo" init -q
mkdir -p "$top/tiny" && echo tiny > "$top/tiny/file"
python3 - > "$top/retention-dates.txt" <<'PY'
import datetime
today = datetime.date.today()
days = [today - datetime.timedelta(days=offset) for offset in range(1, 46)]
days += [today - datetime.timedelta(days=offset) for offset in range(52, 460, 7)]
print("\n".join(day.isoformat() for day in days))
PY
while read -r day; do
  "$top/bin/restic" -r "$retention_repo" backup -q --host upkeep-vm-a --tag upkeep --time "$day 12:00:00" "$top/tiny"
done < "$top/retention-dates.txt"
unset RESTIC_PASSWORD_FILE RESTIC_CACHE_DIR
python3 - "$top/retention-dates.txt" > "$top/retention-expected.txt" <<'PY'
import datetime, sys
days = sorted((datetime.date.fromisoformat(line.strip()) for line in open(sys.argv[1])), reverse=True)
kept = set()
for bucket, count in [(lambda d: d, 7), (lambda d: d.isocalendar()[:2], 4), (lambda d: (d.year, d.month), 12)]:
    seen = []
    for day in days:
        if len(seen) < count and bucket(day) not in seen:
            seen.append(bucket(day))
            kept.add(day)
print("\n".join(sorted(day.isoformat() for day in kept)))
PY
job=$(client --host vm-a run os:retention --dest laptop-ret | job_id)
expect_job "$job" succeeded 600
client snapshots vm-a --dest laptop-ret | snapshot_dates | sort > "$top/retention-kept.txt"
echo "$(wc -l < "$top/retention-dates.txt") snapshots before, $(wc -l < "$top/retention-kept.txt") kept"
diff "$top/retention-expected.txt" "$top/retention-kept.txt"

log "boot VM B from the cloud image with a second, empty disk"
rm -f "$top/b/rescue.qcow2" "$top/b/target.qcow2" "$top/b/seed.iso"
qemu-img create -q -f qcow2 -b "$base" -F qcow2 "$top/b/rescue.qcow2" 8G
qemu-img create -q -f qcow2 "$top/b/target.qcow2" 8G
make_seed "$top/b/seed.iso" upkeep-vm-b
boot_b rescue -drive file="$top/b/rescue.qcow2",if=virtio -drive file="$top/b/target.qcow2",if=virtio \
  -drive file="$top/b/seed.iso",if=virtio,format=raw
wait_for "cloud-init on VM B" 240 vmb sudo cloud-init status --wait
vmb lsblk -dno NAME,SIZE,TYPE

log "password auth: VM A snapshots to an SFTP user with a password on VM B"
password=$(head -c 18 /dev/urandom | base64 | tr -d '/+=')
vmb "sudo useradd -m snapshots && echo 'snapshots:$password' | sudo chpasswd \
  && echo 'PasswordAuthentication yes' | sudo tee /etc/ssh/sshd_config.d/10-upkeep-test.conf >/dev/null \
  && sudo systemctl reload ssh"
b_key_args=()
while read -r _ kind value; do b_key_args+=(--host-key "$kind $value"); done < <(ssh-keyscan -T 10 -p "$port_b" 127.0.0.1 2>/dev/null)
echo "$password" | client dest add vmb-pass --ssh snapshots@10.0.2.2 --port "$port_b" --path /home/snapshots/repos --password-stdin "${b_key_args[@]}"
client snapshot vm-a --to vmb-pass | tee "$top/snapshot-password.out"
snapshot_password=$(snapshot_id < "$top/snapshot-password.out")
cp "$top/client/secrets/vmb-pass.repository-key" "$top/client/secrets/vmb-local.repository-key"
echo "$password" | client dest add vmb-local --ssh snapshots@127.0.0.1 --port "$port_b" --path /home/snapshots/repos --password-stdin "${b_key_args[@]}"
client snapshots vm-a --dest vmb-local | tee "$top/snapshots-password.out"
grep -q "^${snapshot_password:0:8}  " "$top/snapshots-password.out"
if vm "sudo grep -rIl -- '$password' /var/log/upkeep /var/lib/upkeep /etc/upkeep"; then
  echo "FAILED: the SSH password is stored on VM A" >&2; exit 1
fi
echo "password not found in VM A's logs, job database or config"
machine_id_a=$(vm cat /etc/machine-id)
stop_vm

log "4. full restore of snapshot 2 onto VM B's empty disk"
copy_b "$top/bin/restic" "$repo/adapters" "$top/dest_key" "$top/client/secrets/laptop.repository-key" ubuntu@127.0.0.1:/tmp/
vmb "sudo python3 /tmp/adapters/ubuntu-os/restore_system.py --disk /dev/vdb --ssh '$dest_user@$dest_address' \
  --repository '$backups/repos/vm-a' $(printf "%q " "${host_key_args[@]}") --key-file /tmp/dest_key \
  --repository-key-file /tmp/laptop.repository-key --engine /tmp/restic --snapshot '$snapshot2' --new-machine-id --yes" \
  | tee "$top/restore-system.out" | grep -vE '^(Setting up|Processing|Generating|Found|Adding|Sourcing|done)' | tail -25
stop_b

log "4. boot VM B from the restored disk alone"
boot_b restored -drive file="$top/b/target.qcow2",if=virtio
wait_for "MariaDB on the restored system" 120 vmb systemctl is-active --quiet mariadb
echo "hostname: $(vmb hostname)"
echo "marker: $(vmb cat /etc/upkeep-marker)"
echo "root: $(vmb findmnt -no SOURCE,FSTYPE /)"
echo "services: mariadb $(vmb systemctl is-active mariadb), upkeep $(vmb systemctl is-active upkeep), ssh $(vmb systemctl is-active ssh)"
echo "failed units: $(vmb systemctl list-units --state=failed --no-legend --plain | awk '{print $1}' | xargs)"
[ "$(vmb hostname)" = upkeep-vm-a ]
vmb cat /etc/upkeep-marker | grep -q '^snapshot-2 '
[ "$(db_fingerprint vmb)" = "$fingerprint2" ]
echo "orders: $(db_fingerprint vmb) (snapshot 2: $fingerprint2)"
machine_id_b=$(vmb cat /etc/machine-id)
[ "$machine_id_b" != "$machine_id_a" ]
echo "machine-id: new $machine_id_b (VM A had $machine_id_a)"

log "all snapshot VM checks passed"
echo "snapshot 1: $(grep '^snapshot ' "$top/snapshot1.out" | tail -1)"
echo "snapshot 2: $(grep '^snapshot ' "$top/snapshot2.out" | tail -1)"
echo "password snapshot: $(grep '^snapshot ' "$top/snapshot-password.out" | tail -1)"
du -sh "$backups/repos/vm-a"
