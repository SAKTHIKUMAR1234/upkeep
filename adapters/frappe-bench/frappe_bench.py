#!/usr/bin/env python3
"""Upkeep adapter for a Frappe bench managed by Pilot."""

import json
import os
import pwd
import re
import shlex
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from upkeep_adapter import (
    EXIT_DEGRADED,
    AdapterError,
    CommandError,
    Context,
    check_manifest_health,
    log,
    main,
    parse_duration,
    result,
    run,
    wait_until_healthy,
)

SITE_STATE_FILE = "site_state.json"
UPDATE_STATE_FILE = "update_state.json"
JOB_WAIT_INTERVAL = 5
BACKUP_LINE = re.compile(r"^(Config|Database|Public|Private)\s*:\s*(\S+)")


class Bench:
    """One Pilot bench and its sites, as configured in adapter.conf.toml."""

    def __init__(self, config: dict):
        missing = [key for key in ("pilot", "bench", "bench_dir", "sites") if not config.get(key)]
        if missing:
            raise AdapterError(f"adapter.conf.toml is missing: {', '.join(missing)}")
        self.config = config
        self.sites: list[str] = config["sites"]
        self.directory = Path(config["bench_dir"])
        self.user_prefix = user_prefix(config.get("user"))

    def pilot(self, *args: str, quiet: bool = False) -> str:
        return self.run_pilot("--yes", *args, quiet=quiet)

    def frappe(self, site: str, *args: str, quiet: bool = False) -> str:
        # Pilot forwards to frappe only when "frappe" directly follows --bench.
        return self.run_pilot("frappe", "--site", site, *args, quiet=quiet)

    def run_pilot(self, *args: str, quiet: bool) -> str:
        command = [*shlex.split(self.config["pilot"]), "--bench", self.config["bench"], *args]
        return run(self.user_prefix + command, quiet=quiet)

    def git(self, app: str, *args: str, quiet: bool = False) -> str:
        return run(self.user_prefix + ["git", "-C", str(self.directory / "apps" / app), *args], quiet=quiet)

    @property
    def apps(self) -> list[str]:
        """The bench's apps without `skip_apps`, which check_updates and update leave alone."""
        skipped = set(self.config.get("skip_apps", []))
        return [app for app in self.pilot("list-apps", quiet=True).split() if app not in skipped]


def user_prefix(user: str | None) -> list[str]:
    """The command prefix that runs a command as `user`; empty when that is the current user."""
    if not user or user == pwd.getpwuid(os.geteuid()).pw_name:
        return []
    if os.geteuid() != 0:
        raise AdapterError(f"cannot run commands as {user}: the adapter is not running as root")
    return ["runuser", "-u", user, "--", "env", f"HOME={pwd.getpwnam(user).pw_dir}"]


def read_site_state(path: Path) -> dict:
    return json.loads(path.read_text()) if path.exists() else {}


def write_site_state(path: Path, state: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(state, indent=2))
    os.replace(temporary, path)


def is_maintenance_mode_on(bench: Bench, site: str) -> bool:
    output = bench.frappe(site, "show-config", "--format", "json", quiet=True)
    try:
        config = json.loads(output[output.index("{") :])[site]
    except (ValueError, KeyError):
        # The output holds the site's secrets, so it stays out of the message.
        raise AdapterError(f"{site}: cannot read maintenance_mode from show-config") from None
    return bool(config.get("maintenance_mode"))


def scheduler_status(bench: Bench, site: str) -> str:
    output = bench.frappe(site, "scheduler", "status", "--format", "json", quiet=True)
    for line in reversed(output.splitlines()):
        if line.startswith("{"):
            return json.loads(line)["status"]
    raise AdapterError(f"{site}: cannot read the scheduler status from: {output.strip()}")


def wait_for_jobs(bench: Bench, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    for site in bench.sites:
        while True:
            try:
                bench.frappe(site, "ready-for-migration", quiet=True)
                log(f"{site}: no background jobs running")
                break
            except CommandError as error:
                if "NOT READY" not in error.output:
                    raise
            if time.monotonic() > deadline:
                raise AdapterError(f"{site}: background jobs still running after {timeout:g}s")
            log(f"{site}: waiting for background jobs to finish")
            time.sleep(JOB_WAIT_INTERVAL)


def maintenance_on(context: Context) -> None:
    bench = Bench(context.config)
    state_file = context.state_dir / SITE_STATE_FILE
    state = read_site_state(state_file)
    for site in bench.sites:
        if site not in state:
            state[site] = {
                "scheduler": scheduler_status(bench, site),
                "maintenance_mode": is_maintenance_mode_on(bench, site),
            }
            write_site_state(state_file, state)
        log(f"{site}: scheduler was {state[site]['scheduler']}, maintenance mode was {state[site]['maintenance_mode']}")
        bench.frappe(site, "set-maintenance-mode", "on")
        if state[site]["scheduler"] == "enabled":
            bench.frappe(site, "scheduler", "disable")

    wait_for_jobs(bench, parse_duration(context.config.get("job_wait_timeout"), default=600))
    result(before=state)


def maintenance_off(context: Context) -> None:
    """Restores only sites with a remembered state; sites maintenance_on never reached stay as they are."""
    bench = Bench(context.config)
    state_file = context.state_dir / SITE_STATE_FILE
    state = read_site_state(state_file)
    scheduler_enabled, maintenance_off_sites = [], []
    for site in bench.sites:
        if site not in state:
            log(f"{site}: no remembered state, leaving it as it is", level="warn")
            continue
        if state[site]["scheduler"] == "enabled":
            bench.frappe(site, "scheduler", "enable")
            scheduler_enabled.append(site)
        if not state[site]["maintenance_mode"]:
            bench.frappe(site, "set-maintenance-mode", "off")
            maintenance_off_sites.append(site)
    state_file.unlink(missing_ok=True)
    result(scheduler_enabled=scheduler_enabled, maintenance_off=maintenance_off_sites)


def parse_backup_summary(output: str, sites_dir: Path) -> dict[str, str]:
    """File paths from the summary that `frappe backup` prints, keyed by kind."""
    files = {}
    for line in output.splitlines():
        if match := BACKUP_LINE.match(line.strip()):
            files[match.group(1).lower()] = str((sites_dir / match.group(2)).resolve())
    return files


def backup(context: Context) -> None:
    bench = Bench(context.config)
    options = context.config.get("backup", {})
    flags = ["--with-files"] if options.get("with_files", True) else []
    flags += ["--compress"] if options.get("compress", False) else []
    flags += ["--backup-path", options["path"]] if options.get("path") else []

    sites_dir = bench.directory / "sites"
    backups = {}
    for site in bench.sites:
        files = parse_backup_summary(bench.frappe(site, "backup", *flags), sites_dir)
        if "database" not in files:
            raise AdapterError(f"{site}: backup finished but printed no database file")
        backups[site] = files

    # A new backup starts a new update: rollback returns to this point.
    databases = {site: os.path.relpath(files["database"], sites_dir) for site, files in backups.items()}
    write_site_state(context.state_dir / UPDATE_STATE_FILE, {"databases": databases})
    result(backups=backups)


def parse_upstream(line: str) -> tuple[str, str, str] | None:
    """(branch, remote, remote ref) from a `for-each-ref` line; None when the branch has no upstream."""
    branch, remote, remote_ref = (line.strip("\n").split("\t") + ["", ""])[:3]
    return (branch, remote, remote_ref) if remote and remote_ref else None


def parse_ahead_behind(output: str) -> tuple[int, int]:
    """(ahead, behind) from `git rev-list --left-right --count HEAD...@{u}`."""
    ahead, behind = output.split()
    return int(ahead), int(behind)


def app_status(bench: Bench, app: str) -> dict:
    head_ref = bench.git(app, "symbolic-ref", "-q", "HEAD", quiet=True).strip()
    line = bench.git(
        app,
        "for-each-ref",
        "--format=%(refname:short)%09%(upstream:remotename)%09%(upstream:remoteref)",
        head_ref,
        quiet=True,
    )
    upstream = parse_upstream(line)
    if upstream is None:
        raise AdapterError(f"{app}: branch {head_ref} has no upstream")
    branch, remote, remote_ref = upstream
    bench.git(app, "fetch", "--quiet", remote, remote_ref)
    ahead, behind = parse_ahead_behind(bench.git(app, "rev-list", "--left-right", "--count", "HEAD...@{u}", quiet=True))
    return {
        "app": app,
        "branch": branch,
        "upstream": f"{remote}/{remote_ref.removeprefix('refs/heads/')}",
        "ahead": ahead,
        "behind": behind,
    }


def check_updates(context: Context) -> int | None:
    bench = Bench(context.config)
    apps, errors = [], []
    for app in bench.apps:
        try:
            status = app_status(bench, app)
        except AdapterError as error:
            log(str(error), level="error")
            errors.append({"app": app, "error": str(error)})
            continue
        log(f"{app}: {status['behind']} behind, {status['ahead']} ahead of {status['upstream']}")
        apps.append(status)

    behind = [status["app"] for status in apps if status["behind"]]
    result(apps=apps, errors=errors, behind=behind)
    return EXIT_DEGRADED if errors else None


def update(context: Context) -> None:
    bench = Bench(context.config)
    state_file = context.state_dir / UPDATE_STATE_FILE
    state = read_site_state(state_file)
    if "databases" not in state:
        raise AdapterError("no backup from this update: run backup first")
    if "apps" not in state:
        state["apps"] = {app: bench.git(app, "rev-parse", "HEAD", quiet=True).strip() for app in bench.apps}
        write_site_state(state_file, state)

    pulled = []
    for app in bench.apps:
        before = bench.git(app, "rev-parse", "HEAD", quiet=True).strip()
        bench.git(app, "pull", "--ff-only")
        after = bench.git(app, "rev-parse", "HEAD", quiet=True).strip()
        pulled.append({"app": app, "before": before, "after": after})

    for site in bench.sites:
        bench.frappe(site, "migrate")
    state["built"] = True
    write_site_state(state_file, state)
    bench.pilot("build")
    restart = context.config.get("restart", True)
    if restart:
        bench.pilot("restart")
    result(apps=pulled, migrated=bench.sites, restarted=restart)


def rollback(context: Context) -> None:
    """Puts the apps back on their commits from before update, restores the backup and rebuilds assets update built."""
    bench = Bench(context.config)
    state_file = context.state_dir / UPDATE_STATE_FILE
    state = read_site_state(state_file)
    if "apps" not in state:
        log("update did not start: nothing to roll back")
        return

    for app, commit in state["apps"].items():
        bench.git(app, "reset", "--hard", commit)
    for site, database in state["databases"].items():
        restore_database(bench, context.config.get("restore", {}), site, database)
    if state.get("built"):
        bench.pilot("build")
    if context.config.get("restart", True):
        bench.pilot("restart")
    state_file.unlink()
    result(apps=state["apps"], restored=state["databases"])


def restore_database(bench: Bench, options: dict, site: str, database: str) -> None:
    """Runs `frappe restore`; the root password stays out of the log."""
    password_file = options.get("db_root_password_file")
    if not password_file:
        raise AdapterError("adapter.conf.toml is missing: restore.db_root_password_file")
    password = Path(password_file).read_text().strip()
    log(f"{site}: restoring {database}")
    try:
        bench.frappe(
            site,
            "restore",
            database,
            "--force",
            "--db-root-username",
            options.get("db_root_username", "root"),
            "--db-root-password",
            password,
            quiet=True,
        )
    except CommandError as error:
        output = "\n".join(error.output.replace(password, "***").strip().splitlines()[-5:])
        raise AdapterError(f"{site}: restore failed with exit code {error.code}: {output}") from None
    log(f"{site}: restored")


def verify(context: Context) -> None:
    bench = Bench(context.config)
    problems = wait_until_healthy(context, lambda: check_health(context, bench))
    if problems:
        raise AdapterError(f"verify failed: {'; '.join(problems)}")


def check_health(context: Context, bench: Bench) -> list[str]:
    problems = check_manifest_health(context)
    for site in bench.sites:
        try:
            answer = bench.frappe(site, "execute", "frappe.ping", quiet=True)
        except CommandError as error:
            answer = str(error)
        if '"pong"' in answer:
            log(f"{site}: ping ok")
        else:
            problems.append(f"{site}: frappe.ping failed: {answer.strip()[-300:]}")
            log(problems[-1], level="error")
    return problems


if __name__ == "__main__":
    main(
        {
            "maintenance_on": maintenance_on,
            "maintenance_off": maintenance_off,
            "backup": backup,
            "check_updates": check_updates,
            "update": update,
            "rollback": rollback,
            "verify": verify,
        }
    )
