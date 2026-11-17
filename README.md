# Upkeep

Upkeep runs adapter actions and plans as durable, resumable jobs on one server. `upkeep serve` runs the runner. The `upkeep` client starts and follows jobs on the local runner or on a registered host. See `examples/config` for a runner config and `adapters/README.md` for adapters.

## Desktop app

The desktop app runs operations on your servers over SSH. It has email and password sign-in. All state stays on the computer that runs it, in `~/.local/share/upkeep-app` (set another folder with `--data-dir` or `UPKEEP_APP_DIR`). Run output goes to `<data>/runs/<id>.log`.

### Install

```sh
cargo build --release -p upkeep -p upkeep-desktop
install -m 755 target/release/upkeep target/release/upkeep-desktop ~/.local/bin/
```

The desktop window needs the WebKitGTK 4.1 library (`libwebkit2gtk-4.1-0` on Ubuntu). `upkeep app serve --port 7890` serves the same screens without a window, on `http://127.0.0.1:7890/`. The service listens on 127.0.0.1 only.

### Users

Add users with the CLI on the same computer. The command asks for the password twice and does not show it. A password has at least 12 characters.

```sh
upkeep user add owner@example.com
upkeep user passwd owner@example.com    # also signs the user out everywhere
upkeep user disable owner@example.com   # also signs the user out everywhere
upkeep user enable owner@example.com
upkeep user list
```

Passwords are stored as Argon2id hashes (64 MiB, 3 passes, 1 lane, random salt). After 5 failed sign-ins in a row, the email must wait 30 s, then 1 min, up to 15 min. The app locks after 15 minutes without use, and a session ends after 12 hours. The activity log records sign-ins, failed sign-ins, runs, stops, and changes to servers, operations, scripts and users.

### Servers

Click **Add server** and enter a name, an address, the SSH port, the SSH user and the path of a private key file. The app stores only the path. The key file must be readable only by you (`chmod 600`). The SSH user needs passwordless sudo. **Test connection** connects, runs `sudo -n true` and counts the operation folders. It always opens a new connection. Other calls to a saved server share one SSH connection, which stays open 5 minutes after the last call. Its socket is in `<data>/ssh`. A change of the address, port, user or key opens a new connection, and deleting the server closes it.

The app uses the system `ssh` with `BatchMode=yes`. It accepts the host key at the first connection and keeps it in `<data>/known_hosts`.

### Operations

An operation is a folder on the server:

```text
/.upkeep/
  os-update/
    run.sh      the work (required)
    check.sh    a health check after the run (optional)
```

The folder name has lowercase letters, digits and dashes. Each script starts with a `#!` line. The app writes the scripts as root with mode 0750. You can also put folders on the server yourself: the server is the source of truth.

When you click **Run**, the app runs `sudo -n /.upkeep/<folder>/run.sh` over SSH and shows the output live:

- A line `::step <title>` starts a new step. A script without these lines shows as one step.
- Exit 0 means OK.
- Exit 10 means "restart the server". The app runs `sudo -n systemctl reboot`, waits until SSH drops and comes back (at most 15 minutes), then continues.
- Exit 75 is also used by the server lock (see below). When the lock is free, exit 75 of the script means failed.
- Any other exit code means failed.

Then the app runs `check.sh` if the folder has one. The run is green only when both scripts pass. **Stop** closes the SSH session, which ends the remote script. Each server has one run at a time, also across apps and laptops: each script runs under the lock `/run/lock/upkeep.lock` on the server, and `/run/lock/upkeep.owner` names the run that holds it. A run that finds the lock taken fails at once with "Another operation is already running on this server" and the name of that run. The lock is released when the script ends or is stopped, and a restart clears `/run`. A run continues when you close the screens while the service runs. When the service starts, runs that a stopped service left are marked interrupted.

Example `run.sh`:

```bash
#!/bin/bash
set -euo pipefail
echo "::step Clean apt cache"
apt-get clean
echo "::step Clean old journals"
journalctl --vacuum-time=14d
```

### Tests

`cargo test` runs the unit and HTTP tests. `tests/ssh/run.sh` starts a throwaway Docker container with sshd, runs the SSH tests against it and removes it.
