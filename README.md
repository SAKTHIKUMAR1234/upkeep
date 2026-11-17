# Upkeep

Upkeep runs adapter actions and plans as durable, resumable jobs on one server. `upkeep serve` runs the runner. The `upkeep` client starts and follows jobs on the local runner or on a registered host. See `examples/config` for a runner config and `adapters/README.md` for adapters.

## Web page

`upkeep web serve` serves a page where approved users sign in with Google, see each server's checks and latest job, and start plans. The page sends each start to the runner with `confirm = true` and the user's email as `requested_by`. Only the web page holds the runner tokens. The browser never sees them.

Roles:

| Role | Can |
|---|---|
| `viewer` | See servers, jobs and logs. |
| `operator` | Also start and stop plans. |
| `admin` | Also add and remove users and read the activity log. |

Every sign-in, refused sign-in, start, stop and user change goes to the activity log with the time and email.

### Setup

1. In Google Cloud Console, create an OAuth client of type "Web application". Add the redirect URI `<public_url>/auth/callback`, for example `https://upkeep.example.com/auth/callback`.
2. Put the client secret in an owner-only file: `install -m 600 /dev/null /etc/upkeep/google-client-secret`, then write the secret into it.
3. For each server, put its runner API token in an owner-only file, for example `/etc/upkeep/hosts/replica.token` (chmod 600).
4. Write `/etc/upkeep/web.toml`:

```toml
listen = "100.64.0.11:7880"                  # where the page listens
public_url = "https://upkeep.example.com"     # what users open; no trailing /
google_client_id = "1234-abc.apps.googleusercontent.com"
google_client_secret_file = "/etc/upkeep/google-client-secret"   # chmod 600
# database = "/var/lib/upkeep/web.db"         # users, sessions and activity log (default)
# cookie_secure = true                        # false only for local tests over plain http

[hosts.replica]
label = "Replica server (.11)"
url = "http://100.64.0.11:7878"               # the runner's listen address
token_file = "/etc/upkeep/hosts/replica.token"

[hosts.yrp]
label = "YRP server (.22)"
url = "http://100.64.0.22:7878"
token_file = "/etc/upkeep/hosts/yrp.token"
```

5. Add the first admin, then start the page:

```sh
sudo upkeep web user add owner@example.com --role admin
sudo install -m 644 packaging/upkeep-web.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now upkeep-web
```

Manage users with `upkeep web user add <email> --role viewer|operator|admin`, `upkeep web user remove <email>` and `upkeep web user list`, or on the page as an admin. Emails compare without case. Removing a user ends their sessions at once. Use `--settings <file>` to read another settings file.

The page has no TLS of its own. Put it behind an HTTPS proxy for `public_url`, or reach it only over a private network such as Tailscale. A session lasts 12 hours.
