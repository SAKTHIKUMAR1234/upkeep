# Upkeep

Upkeep runs adapter actions and plans as durable, resumable jobs on one server. `upkeep serve` runs the runner. The `upkeep` client starts and follows jobs on the local runner or on a registered host. See `examples/config` for a runner config and `adapters/README.md` for adapters.

## Desktop app

The desktop app runs operations on your servers over SSH. It has email and password sign-in. All state stays on the computer that runs it, in `~/.local/share/upkeep-app` (set another folder with `--data-dir` or `UPKEEP_APP_DIR`). Run output goes to `<data>/runs/<id>.log`.

### Install

```sh
cargo build --release -p upkeep -p upkeep-desktop
install -m 755 target/release/upkeep target/release/upkeep-desktop ~/.local/bin/
```

The desktop window needs the WebKitGTK 4.1 library (`libwebkit2gtk-4.1-0` on Ubuntu). `upkeep app serve --port 7890` serves the same screens without a window, on `http://127.0.0.1:7890/`. Without a public URL, the service accepts only loopback addresses. See [Website](#website) to serve it on a domain.

### Users

Add users with the CLI on the same computer. The command asks for the password twice and does not show it. A password has at least 12 characters.

```sh
upkeep user add owner@example.com
upkeep user passwd owner@example.com    # also signs the user out everywhere
upkeep user disable owner@example.com   # also signs the user out everywhere
upkeep user enable owner@example.com
upkeep user list
```

Passwords are stored as Argon2id hashes (64 MiB, 3 passes, 1 lane, random salt). After 5 failed sign-ins in a row, the email must wait 30 s, then 1 min, up to 15 min. The app locks after 15 minutes without use, and a session ends after 12 hours. Only user input counts as use: keys, clicks, mouse moves, scrolls, touches and returns to the window. While you watch a running run on a visible screen, the run also counts as use. **Lock** keeps your email for the unlock screen. **Sign out** also forgets the email. The activity log records sign-ins, failed sign-ins, runs, stops, and changes to servers, operations, scripts and users.

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

### Website

`upkeep app serve` can also serve the screens as a website behind nginx with HTTPS. Sign-in is necessary for all pages and data. The desktop app does not change.

| Setting | Flag | Default | Effect |
|---|---|---|---|
| `listen` | `--listen` | `127.0.0.1:7890` | Address and port. An address that is not loopback needs `public_url`. |
| `public_url` | `--public-url` | none | The site URL, such as `https://upkeep.example.com`. Only this host and origin get access. Cookies get `Secure`, and responses get HSTS. |
| `trusted_proxy` | `--trusted-proxy` | none | The proxy address. Only requests from it can name the client with `X-Forwarded-For` or `X-Real-IP`. |
| `idle_timeout` | `--idle-timeout` | `15m` | The session locks after this time without use. The minimum is `2m`. |
| `session_lifetime` | `--session-lifetime` | `12h` | The session ends this long after sign-in. |

Put the settings in a TOML file and give it with `--config <file>`. Flags win over the file. See `packaging/upkeep-app.toml`.

On a website, one client may make 10 sign-ins in 15 minutes that fail or are still running, and at most 2 at the same time. A client is one IPv4 address, or one IPv6 /64 network. Then the service answers `429` with `Retry-After`. Each failed sign-in writes one log line, `upkeep: failed sign-in from <client> for "<email>"`, for fail2ban. `<client>` is the IPv4 address, or the network such as `2001:db8:1:2::/64`.

Set up the website on a server:

1. Install the `upkeep` binary in `/usr/local/bin`.
2. Add the service user: `sudo useradd --system --home-dir /var/lib/upkeep-app --shell /usr/sbin/nologin upkeep`.
3. Install the settings file: `sudo install -D -m 644 packaging/upkeep-app.toml /etc/upkeep-app/app.toml`. Then set `public_url` to your host name.
4. Copy `packaging/upkeep-app.service` to `/etc/systemd/system/`, then run `sudo systemctl daemon-reload` and `sudo systemctl enable --now upkeep-app`.
5. Add users: `sudo -u upkeep UPKEEP_APP_DIR=/var/lib/upkeep-app upkeep user add owner@example.com`.
6. Put the SSH keys in `/var/lib/upkeep-app/keys`, owned by `upkeep` with mode `600`. The service cannot read home folders.
7. Get a TLS certificate for the host name, for example with certbot.
8. Copy `packaging/nginx/upkeep.conf` to `/etc/nginx/sites-enabled/`, set the host name and the certificate paths, then run `sudo nginx -t` and `sudo systemctl reload nginx`.
9. Copy `packaging/fail2ban/filter.d/upkeep.conf` and `packaging/fail2ban/jail.d/upkeep.conf` to the same folders in `/etc/fail2ban/`, then run `sudo systemctl reload fail2ban`.

### Tests

`cargo test` runs the unit and HTTP tests. `tests/ssh/run.sh` starts a throwaway Docker container with sshd, runs the SSH tests against it and removes it.
