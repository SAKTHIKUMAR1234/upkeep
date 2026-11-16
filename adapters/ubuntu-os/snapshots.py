"""Whole-system snapshots with restic, written over SFTP straight to the destination repository."""

import json
import os
import shutil
import subprocess
import tempfile
import time
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path

from upkeep_adapter import EXIT_DEGRADED, AdapterError, CommandError, Context, log, progress, result

DEFAULT_ENGINE = "/usr/local/lib/upkeep/bin/restic"
DEFAULT_CACHE_DIR = "/var/cache/upkeep/restic"
# Contents only: the empty folders stay in the snapshot, so a restored system has its mount points.
DEFAULT_EXCLUDES = [
    "/proc/*",
    "/sys/*",
    "/dev/*",
    "/run/*",
    "/tmp/*",
    "/var/tmp/*",
    "/swap.img",
    "/swapfile",
    "/var/cache/apt/*",
    "/root/.cache/*",
    "/home/*/.cache/*",
    "lost+found",
]
DEFAULT_EXTRA_MOUNTS = ["/boot", "/boot/efi"]
DEFAULT_RETENTION = {"daily": 7, "weekly": 4, "monthly": 12}
HOST_KEY_ALIAS = "upkeep-destination"
SNAPSHOT_TAG = "upkeep"
EXIT_REPOSITORY_MISSING = 10
EXIT_INCOMPLETE_SNAPSHOT = 3
PROGRESS_INTERVAL = 30.0


@dataclass
class SnapshotConfig:
    """The `[snapshot]` table of adapter.conf.toml."""

    engine: str = DEFAULT_ENGINE
    cache_dir: str = DEFAULT_CACHE_DIR
    exclude: list[str] = field(default_factory=lambda: list(DEFAULT_EXCLUDES))
    extra_exclude: list[str] = field(default_factory=list)
    extra_mounts: list[str] = field(default_factory=lambda: list(DEFAULT_EXTRA_MOUNTS))
    retention: dict = field(default_factory=lambda: dict(DEFAULT_RETENTION))

    @classmethod
    def from_config(cls, config: dict) -> "SnapshotConfig":
        table = dict(config.get("snapshot", {}))
        unknown = set(table) - set(cls.__dataclass_fields__)
        if unknown:
            raise AdapterError(f"unknown [snapshot] fields: {', '.join(sorted(unknown))}")
        retention = {**DEFAULT_RETENTION, **table.pop("retention", {})}
        if set(retention) != set(DEFAULT_RETENTION) or not all(
            isinstance(count, int) and count >= 0 for count in retention.values()
        ):
            raise AdapterError(f"[snapshot.retention] takes daily, weekly and monthly counts, got {retention}")
        return cls(**table, retention=retention)


@dataclass
class Destination:
    """The SFTP destination the client sent in the job params."""

    user: str
    address: str
    port: int
    repository: str
    host_keys: list[str]

    @classmethod
    def from_params(cls, params: dict) -> "Destination":
        destination = params.get("destination")
        if not destination:
            raise AdapterError("no snapshot destination: start the job with --dest or upkeep snapshot --to")
        try:
            return cls(
                user=destination["user"],
                address=destination["address"],
                port=int(destination["port"]),
                repository=destination["repository"],
                host_keys=list(destination["host_keys"]),
            )
        except (KeyError, TypeError, ValueError) as error:
            raise AdapterError(f"invalid snapshot destination: {error!r}") from error


def ssh_command(destination: Destination, known_hosts: Path, key_file: Path | None) -> list[str]:
    """The SSH command restic starts for SFTP. Host keys are checked strictly; no password is on it."""
    command = [
        "ssh",
        "-F",
        "/dev/null",
        "-p",
        str(destination.port),
        "-l",
        destination.user,
        "-o",
        f"UserKnownHostsFile={known_hosts}",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        f"HostKeyAlias={HOST_KEY_ALIAS}",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ServerAliveCountMax=6",
    ]
    if key_file:
        command += ["-i", str(key_file), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes"]
    else:
        command += [
            "-o",
            "PubkeyAuthentication=no",
            "-o",
            "PreferredAuthentications=password,keyboard-interactive",
            "-o",
            "NumberOfPasswordPrompts=1",
        ]
    return command + [destination.address, "-s", "sftp"]


def write_private(path: Path, text: str, mode: int = 0o600) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    with os.fdopen(descriptor, "w") as file:
        file.write(text)


def shell_join(args: list[str]) -> str:
    """Joins arguments for restic's `sftp.command`, which splits on spaces and honours double quotes."""
    quoted = []
    for arg in args:
        if any(character.isspace() or character == '"' for character in arg):
            arg = '"' + arg.replace("\\", "\\\\").replace('"', '\\"') + '"'
        quoted.append(arg)
    return " ".join(quoted)


@dataclass
class Repository:
    """restic against one repository, with its key and SSH files in a private temporary folder."""

    engine: str
    url: str
    env: dict
    folder: Path
    ssh: list[str]

    def command(self, *args: str) -> list[str]:
        return [self.engine, "-r", self.url, "-o", f"sftp.command={shell_join(self.ssh)}", *args]


@contextmanager
def open_repository(destination: Destination, secrets: dict, engine: str, cache_dir: str) -> Iterator[Repository]:
    """Writes the repository key and SSH files for one action and removes them afterwards."""
    if not secrets.get("repository_key"):
        raise AdapterError("the job has no repository key; start it from the upkeep client")
    if not (secrets.get("ssh_key") or secrets.get("ssh_password")):
        raise AdapterError("the job has no SSH key or password for the destination")
    if not Path(engine).exists() and not shutil.which(engine):
        raise AdapterError(f"snapshot engine {engine} not found; install it with packaging/install-restic.sh")

    folder = Path(tempfile.mkdtemp(prefix="upkeep-snapshot-"))
    try:
        write_private(folder / "repository-key", secrets["repository_key"])
        write_private(folder / "known_hosts", "".join(f"{HOST_KEY_ALIAS} {key}\n" for key in destination.host_keys))
        env = {
            "RESTIC_PASSWORD_FILE": str(folder / "repository-key"),
            "RESTIC_CACHE_DIR": cache_dir,
            "RESTIC_PROGRESS_FPS": str(1 / PROGRESS_INTERVAL),
        }
        key_file = None
        if secrets.get("ssh_key"):
            key_file = folder / "ssh-key"
            write_private(key_file, secrets["ssh_key"])
        else:
            write_private(folder / "ssh-password", secrets["ssh_password"])
            write_private(folder / "askpass", f"#!/bin/sh\nexec cat '{folder / 'ssh-password'}'\n", mode=0o700)
            env |= {"SSH_ASKPASS": str(folder / "askpass"), "SSH_ASKPASS_REQUIRE": "force"}
        yield Repository(
            engine=engine,
            url=f"sftp:{HOST_KEY_ALIAS}:{destination.repository}",
            env=env,
            folder=folder,
            ssh=ssh_command(destination, folder / "known_hosts", key_file),
        )
    finally:
        shutil.rmtree(folder, ignore_errors=True)


def engine_env(repository: Repository) -> dict:
    environment = {key: value for key, value in os.environ.items() if not key.startswith("RESTIC_")}
    return environment | repository.env


def run_engine(repository: Repository, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    """Runs restic quietly and returns its output; errors carry the last lines of its output."""
    command = repository.command(*args)
    completed = subprocess.run(
        command,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        errors="replace",
        env=engine_env(repository),
        check=False,
    )
    if check and completed.returncode != 0:
        raise CommandError(["restic", *args], completed.returncode, completed.stderr + completed.stdout)
    return completed


def stream_engine(repository: Repository, args: list[str], on_message: Callable[[dict], None]) -> int:
    """Runs restic with --json, passes each JSON message on and logs every other line."""
    log(f"$ restic {' '.join(args)}")
    with subprocess.Popen(
        repository.command(*args),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        errors="replace",
        env=engine_env(repository),
    ) as process:
        for line in process.stdout:
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                message = None
            if isinstance(message, dict):
                on_message(message)
            elif line.strip():
                log(line.rstrip("\n"))
    return process.returncode


def ensure_repository(repository: Repository) -> None:
    """Creates the repository on its first snapshot."""
    checked = run_engine(repository, "cat", "config", check=False)
    if checked.returncode == 0:
        return
    if checked.returncode != EXIT_REPOSITORY_MISSING:
        raise CommandError(["restic", "cat", "config"], checked.returncode, checked.stderr + checked.stdout)
    log("repository does not exist yet: creating it")
    run_engine(repository, "init")


def swap_files(proc_swaps: str) -> list[str]:
    """Swap files (not partitions) from /proc/swaps."""
    lines = proc_swaps.splitlines()[1:]
    return [fields[0] for fields in (line.split() for line in lines) if len(fields) > 1 and fields[1] == "file"]


def build_excludes(config: SnapshotConfig, swaps: list[str], private_dirs: list[str]) -> list[str]:
    """Exclude patterns: the configured list, swap files, the snapshot cache and Upkeep's secrets."""
    patterns = config.exclude + config.extra_exclude + swaps + [config.cache_dir, *private_dirs]
    return list(dict.fromkeys(patterns))


def backup_paths(extra_mounts: list[str], is_mount: Callable[[str], bool]) -> list[str]:
    """The root filesystem plus each configured extra mount that is mounted here."""
    return ["/"] + [path for path in extra_mounts if path != "/" and is_mount(path)]


def snapshot_tags(job_id, target: str) -> list[str]:
    return [SNAPSHOT_TAG, f"job={job_id}", f"plan={target}"]


class BackupMessages:
    """Follows `restic backup --json` messages: progress, file errors and the final summary."""

    def __init__(self, clock: Callable[[], float] = time.monotonic):
        self.summary: dict | None = None
        self.errors = 0
        self.clock = clock
        self.last_progress = float("-inf")

    def __call__(self, message: dict) -> None:
        kind = message.get("message_type")
        if kind == "summary":
            self.summary = message
        elif kind == "error":
            self.errors += 1
            item = message.get("item") or ""
            text = (message.get("error") or {}).get("message") or message
            log(f"cannot read {item}: {text}", level="warn")
        elif kind == "status" and self.clock() - self.last_progress >= PROGRESS_INTERVAL:
            self.last_progress = self.clock()
            done = message.get("bytes_done", 0)
            percent = int(message.get("percent_done", 0) * 100)
            progress(percent, f"{done // (1024 * 1024)} MiB read, {message.get('files_done', 0)} files")


def summary_result(summary: dict) -> dict:
    keys = [
        "snapshot_id",
        "data_added",
        "data_added_packed",
        "total_bytes_processed",
        "total_files_processed",
        "files_new",
        "files_changed",
        "files_unmodified",
        "total_duration",
    ]
    return {key: summary.get(key) for key in keys}


def snapshot(context: Context) -> int | None:
    config = SnapshotConfig.from_config(context.config)
    destination = Destination.from_params(context.params)
    host = context.data.get("host") or os.uname().nodename
    paths = backup_paths(config.extra_mounts, os.path.ismount)
    secrets_dir = str(Path(context.data["secrets_file"]).parent) if context.data.get("secrets_file") else None
    swaps = swap_files(Path("/proc/swaps").read_text()) if Path("/proc/swaps").exists() else []
    excludes = build_excludes(config, swaps, [secrets_dir] if secrets_dir else [])
    Path(config.cache_dir).mkdir(parents=True, exist_ok=True)

    with open_repository(destination, context.secrets, config.engine, config.cache_dir) as repository:
        ensure_repository(repository)
        exclude_file = repository.folder / "excludes"
        write_private(exclude_file, "\n".join(excludes) + "\n")
        args = ["backup", "--json", "--one-file-system", "--exclude-caches", "--exclude-file", str(exclude_file)]
        args += ["--host", host]
        for tag in snapshot_tags(context.data.get("job_id"), context.data.get("target", "")):
            args += ["--tag", tag]
        log(f"snapshot of {', '.join(paths)} to {destination.address}:{destination.repository}")
        messages = BackupMessages()
        code = stream_engine(repository, args + paths, messages)

    if messages.summary is None:
        raise AdapterError(f"restic backup failed with exit code {code} and saved no snapshot")
    summary = summary_result(messages.summary)
    result(**summary, host=host, paths=paths, unreadable_files=messages.errors)
    log(f"snapshot {summary['snapshot_id'][:8]} saved: {summary['data_added_packed']} bytes sent")
    if code == EXIT_INCOMPLETE_SNAPSHOT:
        log(f"{messages.errors} files could not be read; the snapshot is incomplete", level="warn")
        return EXIT_DEGRADED
    if code != 0:
        raise AdapterError(f"restic backup exited with {code}")
    return None


def forget_args(host: str, retention: dict) -> list[str]:
    return [
        "forget",
        "--json",
        "--prune",
        "--host",
        host,
        "--tag",
        SNAPSHOT_TAG,
        "--group-by",
        "host",
        "--keep-daily",
        str(retention["daily"]),
        "--keep-weekly",
        str(retention["weekly"]),
        "--keep-monthly",
        str(retention["monthly"]),
    ]


def parse_forget(output: str) -> dict:
    """Kept and removed snapshot ids from `restic forget --json`; prune output may follow the JSON."""
    groups = []
    for line in output.splitlines():
        if line.startswith("["):
            groups = json.loads(line)
            break
    kept = [snapshot["short_id"] for group in groups for snapshot in group.get("keep") or []]
    removed = [snapshot["short_id"] for group in groups for snapshot in group.get("remove") or []]
    return {"kept": len(kept), "removed": len(removed), "kept_ids": kept, "removed_ids": removed}


def retention(context: Context) -> None:
    config = SnapshotConfig.from_config(context.config)
    destination = Destination.from_params(context.params)
    host = context.data.get("host") or os.uname().nodename
    with open_repository(destination, context.secrets, config.engine, config.cache_dir) as repository:
        log(f"retention {config.retention} for host {host}")
        completed = run_engine(repository, *forget_args(host, config.retention))
    outcome = parse_forget(completed.stdout)
    log(f"kept {outcome['kept']} snapshots, removed {outcome['removed']}")
    result(**outcome, policy=config.retention)


def restore_args(snapshot_id: str, target: str, path: str | None) -> list[str]:
    if not target.startswith("/") or Path(target).resolve() == Path("/"):
        raise AdapterError(f"restore target must be an absolute folder other than /, got {target!r}")
    args = ["restore", snapshot_id, "--target", target]
    return args + ["--include", path] if path else args


def restore(context: Context) -> None:
    config = SnapshotConfig.from_config(context.config)
    destination = Destination.from_params(context.params)
    params = context.params
    if not params.get("target"):
        raise AdapterError("restore needs --param target=<folder>")
    snapshot_id = params.get("snapshot", "latest")
    args = restore_args(snapshot_id, params["target"], params.get("path"))
    with open_repository(destination, context.secrets, config.engine, config.cache_dir) as repository:
        log(f"restoring {snapshot_id} {params.get('path') or '(everything)'} into {params['target']}")
        completed = run_engine(repository, *args)
    for line in completed.stdout.splitlines()[-3:]:
        log(line)
    result(snapshot=snapshot_id, target=params["target"], path=params.get("path"))
