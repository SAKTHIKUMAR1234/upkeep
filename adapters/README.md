# Adapters

Python 3 adapters that use only the standard library. Install this folder as `/usr/local/lib/upkeep/adapters` and point an adapter's `run` at the script, for example `run = "python3 /usr/local/lib/upkeep/adapters/ubuntu-os/ubuntu_os.py"`. The runner starts the script in the adapter's config folder, so the script reads `adapter.toml` (services and pings for `verify`) and `adapter.conf.toml` (the fields below) from there. `examples/hosts/laptop-test` has a full example.

Run the tests with `cd adapters && python3 -m unittest`.

## ubuntu-os

Actions: `check_updates`, `update` (exit 10 when a reboot is required), `verify`, `discover`.

| Field | Default | Meaning |
|---|---|---|
| `hold` | `[]` | Package name patterns (shell globs, for example `"mariadb-*"`) held with `apt-mark hold` during `update`. Packages that were held before stay held; the others are released after the update. |
| `ignore_failed_units` | `[]` | Unit name patterns that `verify` does not count as failed. |

## frappe-bench

Actions: `maintenance_on`, `maintenance_off`, `backup`, `check_updates`, `update`, `verify`. Give `update` `needs = ["maintenance_on", "backup"]` in `adapter.toml`.

| Field | Default | Meaning |
|---|---|---|
| `pilot` | required | Pilot command, for example `"/opt/pilot/bin/pilot"`. |
| `bench` | required | Bench name, passed as `pilot --bench`. |
| `bench_dir` | required | Bench folder. The apps are in `<bench_dir>/apps`. |
| `sites` | required | Sites for maintenance, backup, migrate and ping. |
| `user` | runner user | Runs Pilot and git as this user. The runner must be root to switch users. |
| `job_wait_timeout` | `"10m"` | How long `maintenance_on` waits for running background jobs. |
| `restart` | `true` | `update` runs `pilot restart` at the end. Set `false` for a development bench. |
| `backup.with_files` | `true` | Adds `--with-files` to `backup`. |
| `backup.compress` | `false` | Adds `--compress` to `backup`. |
| `backup.path` | frappe default | Adds `--backup-path`. |

`maintenance_on` keeps the scheduler state and the maintenance mode of each site in `<state_dir>/site_state.json`. `maintenance_off` enables the scheduler again only on the sites where it was enabled, turns maintenance mode off only on the sites where it was off, and then deletes the file. It does not change a site that has no remembered state.
