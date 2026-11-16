# Adapters

Python 3 adapters that use only the standard library. Install this folder as `/usr/local/lib/upkeep/adapters` and point an adapter's `run` at the script, for example `run = "python3 /usr/local/lib/upkeep/adapters/ubuntu-os/ubuntu_os.py"`. The runner starts the script in the adapter's config folder, so the script reads `adapter.toml` (services and pings for `verify`) and `adapter.conf.toml` (the fields below) from there. `examples/hosts/laptop-test` has a full example.

Run the tests with `cd adapters && python3 -m unittest`.

## ubuntu-os

Actions: `check_updates`, `update` (exit 10 when a reboot is required), `verify`, `discover`, `snapshot`, `retention`, `restore`.

| Field | Default | Meaning |
|---|---|---|
| `hold` | `[]` | Package name patterns (shell globs, for example `"mariadb-*"`) held with `apt-mark hold` during `update`. Packages that were held before stay held; the others are released after the update. |
| `ignore_failed_units` | `[]` | Unit name patterns that `verify` does not count as failed. |

### Snapshots

`snapshot`, `retention` and `restore` use restic (install it with `packaging/install-restic.sh`). They need a destination in the job: start the job from the client with `upkeep snapshot <host> --to <dest>` or `upkeep --host <host> run <target> --dest <dest>`. The client sends the destination in the job params and the repository key and SSH key or password in the job secrets. The adapter writes them to a private temporary folder for the action and removes it afterwards.

- `snapshot` backs up `/` and each mounted `extra_mounts` folder with `--one-file-system`, so other filesystems are left out. restic sends only new, compressed and encrypted chunks over SFTP, straight to `<path>/<host>` on the destination. The snapshot is tagged `upkeep`, `job=<id>` and `plan=<target>`. The result has `snapshot_id`, `data_added` (new data before compression) and `data_added_packed` (bytes sent). Exit 20 (degraded) when some files could not be read.
- `retention` runs `restic forget --prune` on the host's snapshots tagged `upkeep`, grouped by host.
- `restore` restores `--param snapshot=<id>` (default `latest`) into `--param target=<folder>`, optionally only `--param path=<path>`. It refuses `/` as the target.

| Field in `[snapshot]` | Default | Meaning |
|---|---|---|
| `engine` | `/usr/local/lib/upkeep/bin/restic` | restic binary. |
| `cache_dir` | `/var/cache/upkeep/restic` | restic cache. It is excluded from the snapshot. |
| `exclude` | `/proc/*`, `/sys/*`, `/dev/*`, `/run/*`, `/tmp/*`, `/var/tmp/*`, `/swap.img`, `/swapfile`, `/var/cache/apt/*`, `/root/.cache/*`, `/home/*/.cache/*`, `lost+found` | restic exclude patterns. A value replaces the defaults. The `/*` patterns keep the empty mount points in the snapshot. |
| `extra_exclude` | `[]` | Patterns added to `exclude`. Swap files from `/proc/swaps`, the cache and the runner's secrets folder are always excluded, and so are folders with a `CACHEDIR.TAG` file. |
| `extra_mounts` | `["/boot", "/boot/efi"]` | Other filesystems to include. Folders that are not mount points are skipped. |
| `retention` | `{ daily = 7, weekly = 4, monthly = 12 }` | Snapshots `retention` keeps. |

Stop the databases before `snapshot` (for example with `writers:maintenance_on`), so that their files are consistent.

`restore_system.py` restores a snapshot onto an empty disk of a new machine. See `docs/full-restore.md`.

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
