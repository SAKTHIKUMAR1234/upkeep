#!/bin/bash
# Runs the ignored SSH tests against a throwaway sshd container, then removes it.
set -euo pipefail
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
name="upkeep-test-sshd-$$"
cleanup() {
    docker rm -f "$name" >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT

ssh-keygen -q -t ed25519 -N '' -f "$work/key"
docker build -q -t upkeep-test-sshd tests/ssh >/dev/null
docker run -d --name "$name" -p 127.0.0.1::22 -e AUTHORIZED_KEY="$(cat "$work/key.pub")" upkeep-test-sshd >/dev/null
port=$(docker port "$name" 22/tcp | head -n1 | sed 's/.*://')

UPKEEP_TEST_SSH_PORT="$port" UPKEEP_TEST_SSH_KEY="$work/key" \
    cargo test -j 4 --test app_ssh -- --ignored --nocapture "$@"
