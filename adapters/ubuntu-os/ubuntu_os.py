#!/usr/bin/env python3
"""Upkeep adapter for an Ubuntu host: apt updates, failed units, service discovery and snapshots."""

import re
import shutil
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import snapshots
from upkeep_adapter import (
    EXIT_REBOOT,
    AdapterError,
    Context,
    check_manifest_health,
    log,
    main,
    matches_any,
    result,
    run,
)

RISKY_PREFIXES = (
    "mariadb",
    "mysql",
    "postgresql",
    "docker",
    "containerd",
    "linux-image",
    "systemd",
    "openssh",
    "nginx",
    "python3",
)
APT_ENV = {"DEBIAN_FRONTEND": "noninteractive", "NEEDRESTART_MODE": "l"}
APT_OPTIONS = [
    "-o",
    "Dpkg::Options::=--force-confdef",
    "-o",
    "Dpkg::Options::=--force-confold",
    "-o",
    "DPkg::Lock::Timeout=600",
]
AUTO_UPDATE_UNITS = ["apt-daily.timer", "apt-daily-upgrade.timer", "unattended-upgrades.service"]
DPKG_UPDATES_DIR = Path("/var/lib/dpkg/updates")
REBOOT_REQUIRED = Path("/var/run/reboot-required")
LINGER_DIR = Path("/var/lib/systemd/linger")
SKIPPED_UNITS = (
    "systemd-*",
    "getty@*",
    "serial-getty@*",
    "user@*",
    "dbus*",
    "polkit*",
    "cron*",
    "rsyslog*",
    "upkeep*",
    "multipathd*",
    "udisks2*",
    "ModemManager*",
    "unattended-upgrades*",
    "snapd*",
)

INSTALL_LINE = re.compile(r"^Inst (\S+) (?:\[([^\]]*)\] )?\((\S+)")
REMOVE_LINE = re.compile(r"^Remv (\S+)(?: \[([^\]]*)\])?")


def is_risky(package: str) -> bool:
    return package.startswith(RISKY_PREFIXES)


def parse_simulation(output: str) -> dict:
    """Packages to upgrade, install and remove from `apt-get -s dist-upgrade` output."""
    upgrades, removals = [], []
    for line in output.splitlines():
        if match := INSTALL_LINE.match(line):
            name, current, new = match.groups()
            upgrades.append({"name": name, "current": current, "new": new, "risky": is_risky(name)})
        elif match := REMOVE_LINE.match(line):
            name, current = match.groups()
            removals.append({"name": name, "current": current, "risky": is_risky(name)})
    return {"upgrades": upgrades, "removals": removals}


def simulate() -> dict:
    run(["apt-get", "update"], env=APT_ENV)
    plan = parse_simulation(run(["apt-get", "-s", "dist-upgrade"], env=APT_ENV, quiet=True))
    risky = [package["name"] for package in plan["upgrades"] + plan["removals"] if package["risky"]]
    log(f"{len(plan['upgrades'])} to upgrade or install, {len(plan['removals'])} to remove")
    log(f"risky: {', '.join(risky) or 'none'}")
    return {**plan, "risky": risky}


def check_updates(context: Context) -> None:
    plan = simulate()
    held = sorted(
        package["name"] for package in plan["upgrades"] if matches_any(package["name"], context.config.get("hold", []))
    )
    if held:
        log(f"held during update: {', '.join(held)}")
    result(**plan, held=held)


def held_packages() -> set[str]:
    return set(run(["apt-mark", "showhold"], quiet=True).split())


def installed_packages() -> list[str]:
    return run(["dpkg-query", "-W", "-f", "${Package}\\n"], quiet=True).split()


def packages_to_hold(patterns: list[str], installed: list[str], already_held: set[str]) -> list[str]:
    return sorted(name for name in installed if matches_any(name, patterns) and name not in already_held)


def needs_reboot(upgrades: list[dict]) -> bool:
    """Ubuntu flags a reboot in /run/reboot-required; plain Debian does so only with unattended-upgrades."""
    return REBOOT_REQUIRED.exists() or any(package["name"].startswith("linux-image-") for package in upgrades)


def is_active(unit: str) -> bool:
    return run(["systemctl", "is-active", unit], check=False, quiet=True).strip() == "active"


def update(context: Context) -> int | None:
    if DPKG_UPDATES_DIR.is_dir() and any(DPKG_UPDATES_DIR.iterdir()):
        log("dpkg was interrupted: running dpkg --configure -a", level="warn")
        run(["dpkg", "--configure", "-a"], env=APT_ENV)

    stopped = [unit for unit in AUTO_UPDATE_UNITS if is_active(unit)]
    if stopped:
        run(["systemctl", "stop", *stopped])
    held = packages_to_hold(context.config.get("hold", []), installed_packages(), held_packages())
    try:
        if held:
            run(["apt-mark", "hold", *held])
        plan = simulate()
        run(["apt-get", "-y", *APT_OPTIONS, "dist-upgrade"], env=APT_ENV)
    finally:
        if held:
            run(["apt-mark", "unhold", *held])
        if stopped:
            run(["systemctl", "start", *stopped])

    reboot = needs_reboot(plan["upgrades"])
    result(upgraded=plan["upgrades"], removed=plan["removals"], held=held, reboot_required=reboot)
    if reboot:
        log("reboot required", level="warn")
        return EXIT_REBOOT
    return None


def failed_units(ignore: list[str]) -> list[str]:
    output = run(["systemctl", "list-units", "--state=failed", "--plain", "--no-legend"], quiet=True)
    return [unit for unit in parse_unit_names(output) if not matches_any(unit, ignore)]


def verify(context: Context) -> None:
    problems = [f"failed unit {unit}" for unit in failed_units(context.config.get("ignore_failed_units", []))]
    for problem in problems:
        log(problem, level="error")
    problems += check_manifest_health(context)
    if problems:
        raise AdapterError(f"verify failed: {'; '.join(problems)}")
    log("all checks passed")


def parse_unit_names(output: str) -> list[str]:
    """Unit names from `systemctl list-units --plain --no-legend` output."""
    return [line.split()[0] for line in output.splitlines() if line.strip() and not line.startswith(" ")]


def draft_services(system_units: list[str], user_units: dict[str, list[str]], containers: list[str]) -> list[dict]:
    """Draft [[service]] entries; base system units are left out."""
    services = [{"kind": "systemd", "unit": unit} for unit in system_units if not matches_any(unit, SKIPPED_UNITS)]
    services += [
        {"kind": "systemd-user", "unit": unit, "user": user} for user, units in user_units.items() for unit in units
    ]
    services += [{"kind": "docker", "container": name} for name in containers]

    used = set()
    for service in services:
        base = re.sub(r"\.service$", "", service.get("unit") or service.get("container"))
        name = base if base not in used else f"{service.get('user', 'docker')}-{base}"
        used.add(name)
        service["name"] = name
    return services


def render_services(services: list[dict]) -> str:
    blocks = []
    for service in services:
        keys = [key for key in ("name", "kind", "unit", "user", "container") if key in service]
        lines = ["[[service]]"] + [f'{key} = "{service[key]}"' for key in keys]
        blocks.append("\n".join(lines))
    return "\n\n".join(blocks) + "\n"


def running_units(*scope: str) -> list[str]:
    command = ["systemctl", *scope, "list-units", "--type=service", "--state=running", "--plain", "--no-legend"]
    return parse_unit_names(run(command, quiet=True, check=False))


def docker_containers() -> list[str]:
    if shutil.which("docker") is None:
        return []
    return run(["docker", "ps", "--format", "{{.Names}}"], quiet=True, check=False).split()


def discover(context: Context) -> None:
    lingering = sorted(path.name for path in LINGER_DIR.iterdir()) if LINGER_DIR.is_dir() else []
    user_units = {user: running_units("--user", f"--machine={user}@") for user in lingering}
    services = draft_services(running_units(), user_units, docker_containers())
    toml = render_services(services)
    for line in toml.splitlines():
        log(line)
    result(services=services, toml=toml)


if __name__ == "__main__":
    main(
        {
            "check_updates": check_updates,
            "update": update,
            "verify": verify,
            "discover": discover,
            "snapshot": snapshots.snapshot,
            "retention": snapshots.retention,
            "restore": snapshots.restore,
        }
    )
