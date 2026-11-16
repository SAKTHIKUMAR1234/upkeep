#!/usr/bin/env python3
"""Restores an Upkeep snapshot onto an empty disk from a rescue or live system, so that the disk boots on its own."""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import snapshots
from upkeep_adapter import AdapterError

BIOS_BOOT = "21686148-6449-6E6F-744E-656564454649"
EFI_SYSTEM = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
LINUX_FILESYSTEM = "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
RESTORE_NOTE = "# upkeep-restore:"
NETPLAN_FILE = "etc/netplan/90-upkeep-restore.yaml"
DHCP_NETPLAN = """network:
  version: 2
  ethernets:
    upkeep-restore-en:
      match:
        name: "en*"
      dhcp4: true
    upkeep-restore-eth:
      match:
        name: "eth*"
      dhcp4: true
"""
CLOUD_INIT_NETWORK_OFF = "network: {config: disabled}\n"
EMPTY_FOLDERS = {"proc": 0o555, "sys": 0o555, "dev": 0o755, "run": 0o755, "tmp": 0o1777, "var/tmp": 0o1777}


def sfdisk_script(esp_mib: int) -> str:
    """GPT with a BIOS boot partition, an EFI system partition and the root filesystem, so both firmwares boot."""
    return (
        "label: gpt\n"
        f"size=1MiB, type={BIOS_BOOT}, name=bios-boot\n"
        f"size={esp_mib}MiB, type={EFI_SYSTEM}, name=esp\n"
        f"type={LINUX_FILESYSTEM}, name=root\n"
    )


def partition_path(disk: str, number: int) -> str:
    """/dev/vdb + 2 is /dev/vdb2; /dev/nvme1n1 + 2 is /dev/nvme1n1p2."""
    return f"{disk}p{number}" if disk[-1].isdigit() else f"{disk}{number}"


def rewrite_fstab(text: str, root_uuid: str, esp_uuid: str) -> str:
    """Points / and /boot/efi at the new filesystems, drops /boot and swap, and makes other mounts optional."""
    lines, seen = [], set()
    for line in text.splitlines():
        fields = line.split()
        if not fields or line.lstrip().startswith("#") or len(fields) < 4:
            lines.append(line)
            continue
        mount, filesystem, options = fields[1], fields[2], fields[3]
        if mount == "/":
            options = options if filesystem == "ext4" else "defaults,errors=remount-ro"
            lines.append(f"UUID={root_uuid} / ext4 {options} 0 1")
        elif mount == "/boot/efi":
            options = options if filesystem == "vfat" else "umask=0077"
            lines.append(f"UUID={esp_uuid} /boot/efi vfat {options} 0 1")
        elif mount == "/boot":
            lines.append(f"{RESTORE_NOTE} /boot is on the root filesystem now: {line}")
        elif filesystem == "swap":
            lines.append(f"{RESTORE_NOTE} swap is not restored: {line}")
        else:
            if "nofail" not in options.split(","):
                fields[3] = f"{options},nofail"
            lines.append(" ".join(fields))
        seen.add(mount)
    if "/" not in seen:
        lines.append(f"UUID={root_uuid} / ext4 defaults,errors=remount-ro 0 1")
    if "/boot/efi" not in seen:
        lines.append(f"UUID={esp_uuid} /boot/efi vfat umask=0077 0 1")
    return "\n".join(lines) + "\n"


def disable_force_partuuid(text: str) -> str:
    """Cloud images boot a fixed PARTUUID without an initrd; the new disk has another one."""
    return "\n".join(
        f"{RESTORE_NOTE} {line}" if line.strip().startswith("GRUB_FORCE_PARTUUID=") else line
        for line in text.splitlines()
    ) + ("\n" if text.endswith("\n") else "")


def execute(command: list[str], **options) -> str:
    print(f"$ {' '.join(command)}", flush=True)
    completed = subprocess.run(command, check=False, text=True, stdout=subprocess.PIPE, **options)
    if completed.stdout:
        print(completed.stdout, end="", flush=True)
    if completed.returncode != 0:
        raise AdapterError(f"{command[0]} exited with {completed.returncode}")
    return completed.stdout


def filesystem_uuid(device: str) -> str:
    return execute(["blkid", "-s", "UUID", "-o", "value", device]).strip()


def check_empty_disk(disk: str) -> None:
    kind = execute(["lsblk", "-dno", "TYPE", disk]).strip()
    if kind != "disk":
        raise AdapterError(f"{disk} is a {kind or 'missing device'}, not a whole disk")
    mounted = [line for line in execute(["lsblk", "-nro", "MOUNTPOINTS", disk]).splitlines() if line.strip()]
    if mounted:
        raise AdapterError(f"{disk} has mounted filesystems: {', '.join(mounted)}")


def partition(disk: str, esp_mib: int) -> tuple[str, str]:
    """Partitions and formats the disk; returns the ESP and root devices."""
    execute(["wipefs", "--all", disk])
    execute(["sfdisk", "--wipe", "always", "--wipe-partitions", "always", disk], input=sfdisk_script(esp_mib))
    execute(["udevadm", "settle"])
    esp, root = partition_path(disk, 2), partition_path(disk, 3)
    execute(["mkfs.vfat", "-F", "32", "-n", "UEFI", esp])
    execute(["mkfs.ext4", "-F", "-q", "-L", "root", root])
    execute(["udevadm", "settle"])
    return esp, root


def fix_system(target: Path, root_uuid: str, esp_uuid: str, new_machine_id: bool, network: str) -> None:
    for folder, mode in EMPTY_FOLDERS.items():
        (target / folder).mkdir(parents=True, exist_ok=True)
        os.chmod(target / folder, mode)

    fstab = target / "etc/fstab"
    original = fstab.read_text() if fstab.exists() else ""
    fstab.write_text(rewrite_fstab(original, root_uuid, esp_uuid))
    if any(line.split()[2:3] == ["swap"] for line in original.splitlines() if not line.startswith("#")):
        (target / "etc/initramfs-tools/conf.d").mkdir(parents=True, exist_ok=True)
        (target / "etc/initramfs-tools/conf.d/resume").write_text("RESUME=none\n")

    for config in [target / "etc/default/grub", *sorted((target / "etc/default/grub.d").glob("*.cfg"))]:
        if config.exists():
            config.write_text(disable_force_partuuid(config.read_text()))

    if new_machine_id:
        (target / "etc/machine-id").write_text("")
        dbus_id = target / "var/lib/dbus/machine-id"
        if dbus_id.is_file() and not dbus_id.is_symlink():
            dbus_id.unlink()

    if network == "dhcp":
        for config in (target / "etc/netplan").glob("*.yaml"):
            config.rename(config.with_name(config.name + ".upkeep-restore"))
        snapshots.write_private(target / NETPLAN_FILE, DHCP_NETPLAN)
        if (target / "etc/cloud/cloud.cfg.d").is_dir():
            (target / "etc/cloud/cloud.cfg.d/99-upkeep-restore.cfg").write_text(CLOUD_INIT_NETWORK_OFF)


def install_boot_loader(target: Path, disk: str) -> None:
    binds = ["dev", "dev/pts", "proc", "sys", "run"]
    mounted = []
    try:
        for folder in binds:
            execute(["mount", "--bind", f"/{folder}", str(target / folder)])
            mounted.append(target / folder)
        chroot = ["chroot", str(target)]
        if (target / "usr/lib/grub/i386-pc").is_dir():
            execute([*chroot, "grub-install", "--target=i386-pc", disk])
        if (target / "usr/lib/grub/x86_64-efi").is_dir():
            execute(
                [*chroot, "grub-install", "--target=x86_64-efi", "--efi-directory=/boot/efi", "--removable"]
                + ["--no-nvram"]
            )
        execute([*chroot, "update-initramfs", "-u", "-k", "all"])
        execute([*chroot, "update-grub"])
    finally:
        for folder in reversed(mounted):
            subprocess.run(["umount", "--lazy", str(folder)], check=False)


def read_secret(path: str | None) -> str | None:
    return Path(path).read_text() if path else None


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--disk", required=True, help="empty disk to restore onto, for example /dev/vdb")
    parser.add_argument("--ssh", required=True, help="user@address of the destination")
    parser.add_argument("--port", type=int, default=22)
    parser.add_argument("--repository", required=True, help="the host's repository folder on the destination")
    parser.add_argument("--host-key", action="append", required=True, help='"<type> <base64>", repeatable')
    auth = parser.add_mutually_exclusive_group(required=True)
    auth.add_argument("--key-file", help="SSH private key file")
    auth.add_argument("--password-file", help="file with the SSH password")
    parser.add_argument("--repository-key-file", required=True, help="the destination's repository key")
    parser.add_argument("--snapshot", default="latest")
    parser.add_argument("--engine", default=snapshots.DEFAULT_ENGINE)
    parser.add_argument("--esp-size", type=int, default=512, help="EFI system partition size in MiB")
    parser.add_argument("--new-machine-id", action="store_true", help="a new machine id on first boot")
    parser.add_argument("--network", choices=["dhcp", "keep"], default="dhcp")
    parser.add_argument("--yes", action="store_true", help="erase the disk without asking")
    return parser.parse_args(argv)


def restore_system(args: argparse.Namespace) -> None:
    if os.geteuid() != 0:
        raise AdapterError("run as root")
    check_empty_disk(args.disk)
    user, _, address = args.ssh.partition("@")
    destination = snapshots.Destination(user, address, args.port, args.repository, args.host_key)
    secrets = {
        "repository_key": read_secret(args.repository_key_file),
        "ssh_key": read_secret(args.key_file),
        "ssh_password": read_secret(args.password_file),
    }
    with (
        tempfile.TemporaryDirectory(prefix="upkeep-restore-cache-") as cache,
        snapshots.open_repository(destination, secrets, args.engine, cache) as repository,
    ):
        found = json.loads(snapshots.run_engine(repository, "snapshots", "--json", args.snapshot).stdout)
        if len(found) != 1:
            raise AdapterError(f"snapshot {args.snapshot} not found")
        snapshot = found[0]
        print(f"snapshot {snapshot['short_id']} of {snapshot['hostname']} from {snapshot['time']}: {snapshot['paths']}")
        if not args.yes and input(f"erase {args.disk} and restore onto it? type the disk name: ") != args.disk:
            raise AdapterError("not confirmed")

        esp, root = partition(args.disk, args.esp_size)
        with tempfile.TemporaryDirectory(prefix="upkeep-restore-") as mount_point:
            target = Path(mount_point)
            execute(["mount", root, str(target)])
            try:
                (target / "boot/efi").mkdir(parents=True)
                execute(["mount", esp, str(target / "boot/efi")])
                execute(
                    repository.command("restore", snapshot["id"], "--target", str(target)),
                    env=snapshots.engine_env(repository),
                )
                fix_system(target, filesystem_uuid(root), filesystem_uuid(esp), args.new_machine_id, args.network)
                install_boot_loader(target, args.disk)
            finally:
                subprocess.run(["sync"], check=False)
                subprocess.run(["umount", "--recursive", str(target)], check=False)
    print(f"restored snapshot {snapshot['short_id']} onto {args.disk}; boot from it")


def main(argv: list[str]) -> int:
    try:
        restore_system(parse_arguments(argv))
    except AdapterError as error:
        print(f"restore failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
