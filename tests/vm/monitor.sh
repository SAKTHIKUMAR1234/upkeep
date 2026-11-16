#!/usr/bin/env bash
# Checks the step 4 monitor on two VMs that watch each other, with alerts sent to a
# local fake Telegram API that the VMs reach on the qemu host address 10.0.2.2:
# a real failed unit, a stopped peer runner, their recoveries and alert test.
# Usage: tests/vm/monitor.sh <work-dir> <ubuntu-cloud-image.img>
set -euo pipefail

mkdir -p "$1/a" "$1/b"
top=$(realpath "$1")
base=$(realpath "$2")
here=$(cd "$(dirname "$0")" && pwd)
bot_port=18443
messages="$top/telegram.jsonl"

use_a() { UPKEEP_VM_SSH_PORT=22431 UPKEEP_VM_NAME=vm-a UPKEEP_VM_FORWARDS=",hostfwd=tcp:127.0.0.1:17871-:7878" source "$here/lib.sh" "$top/a" "$base"; }
use_b() { UPKEEP_VM_SSH_PORT=22432 UPKEEP_VM_NAME=vm-b UPKEEP_VM_FORWARDS=",hostfwd=tcp:127.0.0.1:17872-:7878" source "$here/lib.sh" "$top/b" "$base"; }

# config_for <name> <peer-name> <peer-api-port>: the runner config of one VM.
config_for() {
  local dir="$top/$1-config"
  rm -rf "$dir" && cp -r "$here/monitor-config" "$dir"
  printf '%s-token\n' "$1" > "$dir/token"
  printf '%s-token\n' "$2" > "$dir/peer.token"
  echo "4242:vm-test-bot" > "$dir/telegram-token"
  chmod 600 "$dir/token" "$dir/peer.token" "$dir/telegram-token"
  cat > "$dir/upkeep.toml" <<TOML
listen = "0.0.0.0:7878"
host = "$1"

[monitor]
interval = "5s"
alert_after = 2
utc_offset = "+05:30"

[monitor.telegram]
token_file = "/etc/upkeep/telegram-token"
chat_id = "-1001"
message_thread_id = 7
api_url = "http://10.0.2.2:$bot_port"

[[monitor.peer]]
name = "$2"
url = "http://10.0.2.2:$3"
token_file = "/etc/upkeep/peer.token"
down_after = "10s"
TOML
  echo "$dir"
}

# Messages before this line of the message file are ignored, so each step sees only its own.
since=0
mark() { since=$(wc -l < "$messages"); }

# find_message <title> <text>: prints the first message after the mark that starts with title and holds text.
find_message() {
  python3 - "$messages" "$since" "$1" "$2" <<'PY'
import json, sys
path, since, title, text = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
for line in open(path).readlines()[since:]:
    message = json.loads(line)["text"]
    if message.startswith(title) and text in message:
        print("  telegram: " + message.replace("\n", " | "))
        sys.exit(0)
sys.exit(1)
PY
}

expect_message() {
  wait_for "a '$1' message with '$2'" 90 find_message "$1" "$2"
  find_message "$1" "$2"
}

# check_is <state> <check>: the runner's status shows that check in that state.
check_is() { vm sudo upkeep status | grep -qE "^$1 +$2 "; }

cleanup() {
  use_a; stop_vm
  use_b; stop_vm
  [ -z "${bot_pid:-}" ] || kill "$bot_pid" 2>/dev/null || true
}
trap cleanup EXIT

: > "$messages"
python3 "$here/fake_telegram.py" "$bot_port" "$messages" &
bot_pid=$!

config_a=$(config_for vm-a vm-b 17872)
config_b=$(config_for vm-b vm-a 17871)
use_a; boot_vm; install_upkeep "$config_a"
use_b; boot_vm; install_upkeep "$config_b"

log "1. both runners watch each other; alert test reaches the fake Telegram API"
mark
use_a; vm sudo upkeep alert test
expect_message "<b>🧪 TEST · vm-a</b>" "Upkeep can reach this chat."
use_a; wait_for "vm-a to see vm-b up" 60 check_is ok peer:vm-b
use_b; wait_for "vm-b to see vm-a up" 60 check_is ok peer:vm-a
vm sudo upkeep status

log "2. a real failed unit on vm-a alerts, then recovers"
use_a
vm sudo systemctl reset-failed
wait_for "no failed units on vm-a" 60 check_is ok failed-units
mark
vm 'sudo systemd-run --quiet --unit=upkeep-demo-broken /bin/false'
expect_message "<b>🔴 CRITICAL · vm-a</b>" "upkeep-demo-broken.service"
vm sudo upkeep status | head -4
vm sudo systemctl reset-failed upkeep-demo-broken.service
expect_message "<b>✅ FIXED · vm-a</b>" "No failed system services"

log "3. vm-b's runner stops: vm-a says it does not answer, then recovery"
mark
use_b; vm sudo systemctl stop upkeep
expect_message "<b>🚨 SERVER NOT ANSWERING · vm-b</b>" "vm-a cannot reach Upkeep on vm-b"
use_b; vm sudo systemctl start upkeep
expect_message "<b>✅ FIXED · vm-b</b>" "Seen from vm-a"

log "4. vm-a's runner stops: vm-b says it does not answer, then recovery"
mark
use_a; vm sudo systemctl stop upkeep
expect_message "<b>🚨 SERVER NOT ANSWERING · vm-a</b>" "vm-b cannot reach Upkeep on vm-a"
use_a; vm sudo systemctl start upkeep
expect_message "<b>✅ FIXED · vm-a</b>" "Seen from vm-b"

log "all VM checks passed; every message the fake Telegram API got:"
python3 -c 'import json,sys; [print("  " + json.loads(l)["text"].replace("\n", " | ")) for l in open(sys.argv[1])]' "$messages"
