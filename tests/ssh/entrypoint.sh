#!/bin/sh
# Installs the test key, then keeps sshd running. A test "restart" kills the listening sshd;
# this loop brings it back after a few seconds, like a server that reboots.
set -eu
install -d -m 700 -o upk -g upk /home/upk/.ssh
printf '%s\n' "$AUTHORIZED_KEY" > /home/upk/.ssh/authorized_keys
chown upk:upk /home/upk/.ssh/authorized_keys
chmod 600 /home/upk/.ssh/authorized_keys
mkdir -p /run/sshd
ssh-keygen -A >/dev/null
while true; do
    /usr/sbin/sshd -D -e || true
    sleep 4
done
