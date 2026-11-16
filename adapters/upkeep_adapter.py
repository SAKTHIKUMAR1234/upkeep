"""Shared helper for Upkeep adapters: the stdin context, JSON line output, commands and health checks."""

import fnmatch
import json
import os
import re
import subprocess
import sys
import tomllib
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import TextIO

EXIT_OK = 0
EXIT_REBOOT = 10
EXIT_DEGRADED = 20
EXIT_FAILED = 1
EXIT_UNKNOWN_ACTION = 2

MANIFEST_FILE = "adapter.toml"
CONFIG_FILE = "adapter.conf.toml"
DEFAULT_CHECK_TIMEOUT = 10.0


class AdapterError(Exception):
    """A failure the adapter reports as its final error line before exiting with EXIT_FAILED."""


class CommandError(AdapterError):
    def __init__(self, command: list[str], code: int, output: str):
        last_lines = "\n".join(output.strip().splitlines()[-5:])
        super().__init__(f"{' '.join(command)} exited with {code}" + (f": {last_lines}" if last_lines else ""))
        self.code = code
        self.output = output


@dataclass
class Context:
    """What one action runs with: the runner's stdin context and the adapter folder's files."""

    data: dict
    adapter_dir: Path
    manifest: dict = field(default_factory=dict)
    config: dict = field(default_factory=dict)

    @property
    def state_dir(self) -> Path:
        return Path(self.data.get("state_dir") or os.environ.get("UPKEEP_STATE_DIR") or ".")


def emit(kind: str, **fields) -> None:
    print(json.dumps({"type": kind, **fields}), flush=True)


def log(message: str, level: str = "info") -> None:
    emit("log", level=level, message=message)


def progress(percent: int, message: str) -> None:
    emit("progress", percent=percent, message=message)


def result(**fields) -> None:
    emit("result", **fields)


def read_context(stream: TextIO | None = None) -> dict:
    stream = stream or sys.stdin
    if stream.isatty():
        return {}
    text = stream.read().strip()
    return json.loads(text) if text else {}


def read_toml(path: Path) -> dict:
    if not path.exists():
        return {}
    with path.open("rb") as file:
        return tomllib.load(file)


def load_context(adapter_dir: Path, stream: TextIO | None = None) -> Context:
    return Context(
        data=read_context(stream),
        adapter_dir=adapter_dir,
        manifest=read_toml(adapter_dir / MANIFEST_FILE),
        config=read_toml(adapter_dir / CONFIG_FILE),
    )


def run(
    command: list[str],
    *,
    env: dict | None = None,
    cwd: Path | None = None,
    check: bool = True,
    quiet: bool = False,
) -> str:
    """Runs a command, logs every output line unless quiet, and returns stdout and stderr together."""
    if not quiet:
        log(f"$ {' '.join(command)}")
    lines = []
    with subprocess.Popen(
        command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        errors="replace",
        env={**os.environ, **env} if env else None,
        cwd=cwd,
    ) as process:
        for line in process.stdout:
            lines.append(line)
            if not quiet:
                log(line.rstrip("\n"))
    code = process.returncode
    output = "".join(lines)
    if not quiet:
        log(f"exit code {code}")
    if check and code != 0:
        raise CommandError(command, code, output)
    return output


def main(actions: dict[str, Callable[[Context], int | None]], adapter_dir: Path | None = None) -> None:
    """Runs the action named in argv[1] and exits with its code; errors become a log line and exit 1."""
    action = sys.argv[1] if len(sys.argv) > 1 else ""
    handler = actions.get(action)
    if handler is None:
        log(f"unknown action {action!r}; known: {', '.join(sorted(actions))}", level="error")
        sys.exit(EXIT_UNKNOWN_ACTION)

    try:
        context = load_context(adapter_dir or Path.cwd())
        code = handler(context) or EXIT_OK
    except AdapterError as error:
        log(str(error), level="error")
        code = EXIT_FAILED
    sys.exit(code)


def parse_duration(value: str | float | None, default: float = DEFAULT_CHECK_TIMEOUT) -> float:
    """Seconds in a value like 30, "10s", "5m" or "1h30m"."""
    if value is None:
        return default
    if isinstance(value, (int, float)):
        return float(value)
    units = {"ms": 0.001, "s": 1, "m": 60, "h": 3600}
    parts = re.findall(r"(\d+)\s*(ms|s|m|h)", value)
    if not parts or re.sub(r"(\d+)\s*(ms|s|m|h)|\s", "", value):
        raise AdapterError(f"invalid duration {value!r}")
    return sum(int(number) * units[unit] for number, unit in parts)


def matches_any(name: str, patterns: list[str]) -> bool:
    return any(fnmatch.fnmatchcase(name, pattern) for pattern in patterns)


def check_service(service: dict) -> str | None:
    """Checks one manifest [[service]]; returns why it failed, or None."""
    kind = service.get("kind")
    commands = {
        "systemd": lambda: ["systemctl", "is-active", "--quiet", service["unit"]],
        "systemd-user": lambda: [
            "systemctl",
            "--user",
            f"--machine={service['user']}@",
            "is-active",
            "--quiet",
            service["unit"],
        ],
        "docker": lambda: ["docker", "inspect", "--format", "{{.State.Running}}", service["container"]],
        "process": lambda: ["pgrep", "-f", service["pattern"]],
    }
    if kind not in commands:
        return f"unknown service kind {kind!r}"
    code, output = run_check(commands[kind](), DEFAULT_CHECK_TIMEOUT)
    if code != 0:
        return output.strip() or f"exit code {code}"
    if kind == "docker" and output.strip() != "true":
        return "container is not running"
    return None


def check_ping(ping: dict, cwd: Path) -> str | None:
    """Runs one manifest [[ping]]; returns why it failed, or None."""
    timeout = parse_duration(ping.get("timeout"))
    if "http" in ping:
        expect = ping.get("expect", 200)
        try:
            with urllib.request.urlopen(ping["http"], timeout=timeout) as response:
                status = response.status
        except urllib.error.HTTPError as error:
            status = error.code
            error.close()
        except (urllib.error.URLError, OSError) as error:
            return str(error)
        return None if status == expect else f"expected HTTP {expect}, got {status}"

    code, output = run_check(["sh", "-c", ping["command"]], timeout, cwd)
    return None if code == 0 else output.strip() or f"exit code {code}"


def run_check(command: list[str], timeout: float, cwd: Path | None = None) -> tuple[int, str]:
    try:
        completed = subprocess.run(
            command,
            check=False,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=cwd,
        )
    except subprocess.TimeoutExpired:
        return 124, f"timed out after {timeout:g}s"
    except FileNotFoundError as error:
        return 127, str(error)
    return completed.returncode, completed.stderr + completed.stdout


def check_manifest_health(context: Context) -> list[str]:
    """Checks the manifest's services and pings, logs each one and returns the failures."""
    checks = [("service", service, check_service) for service in context.manifest.get("service", [])]
    checks += [
        ("ping", ping, lambda ping: check_ping(ping, context.adapter_dir)) for ping in context.manifest.get("ping", [])
    ]

    failures = []
    for kind, item, check in checks:
        name = item["name"]
        problem = check(item)
        if problem:
            failures.append(f"{kind} {name}: {problem}")
            log(f"{kind} {name}: FAILED ({problem})", level="error")
        else:
            log(f"{kind} {name}: ok")
    return failures
