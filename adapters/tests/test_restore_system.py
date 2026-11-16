import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "ubuntu-os"))

import restore_system

CLOUD_FSTAB = """# CLOUD_IMG: This file was created/modified by the Cloud Image build process
LABEL=cloudimg-rootfs\t/\t ext4\tdiscard,commit=30,errors=remount-ro\t0 1
LABEL=BOOT\t/boot\text4\tdefaults\t0 2
LABEL=UEFI\t/boot/efi\tvfat\tumask=0077\t0 1
/swap.img\tnone\tswap\tsw\t0 0
UUID=1111-data /srv/data xfs defaults 0 2
"""


class LayoutTest(unittest.TestCase):
    def test_gpt_has_bios_boot_esp_and_root(self):
        script = restore_system.sfdisk_script(512)
        self.assertTrue(script.startswith("label: gpt\n"))
        self.assertIn(f"size=1MiB, type={restore_system.BIOS_BOOT}", script)
        self.assertIn(f"size=512MiB, type={restore_system.EFI_SYSTEM}", script)
        self.assertTrue(script.endswith(f"type={restore_system.LINUX_FILESYSTEM}, name=root\n"))

    def test_partition_paths(self):
        self.assertEqual(restore_system.partition_path("/dev/vdb", 3), "/dev/vdb3")
        self.assertEqual(restore_system.partition_path("/dev/nvme1n1", 2), "/dev/nvme1n1p2")


class FstabTest(unittest.TestCase):
    def test_rewrites_root_and_esp_and_drops_boot_and_swap(self):
        lines = restore_system.rewrite_fstab(CLOUD_FSTAB, "root-uuid", "ESP-UUID").splitlines()
        self.assertEqual(
            lines,
            [
                "# CLOUD_IMG: This file was created/modified by the Cloud Image build process",
                "UUID=root-uuid / ext4 discard,commit=30,errors=remount-ro 0 1",
                "# upkeep-restore: /boot is on the root filesystem now: LABEL=BOOT\t/boot\text4\tdefaults\t0 2",
                "UUID=ESP-UUID /boot/efi vfat umask=0077 0 1",
                "# upkeep-restore: swap is not restored: /swap.img\tnone\tswap\tsw\t0 0",
                "UUID=1111-data /srv/data xfs defaults,nofail 0 2",
            ],
        )

    def test_adds_missing_root_and_esp_and_replaces_other_root_filesystems(self):
        text = restore_system.rewrite_fstab("/dev/mapper/vg-root / xfs defaults 0 1\n", "R", "E")
        self.assertEqual(text, "UUID=R / ext4 defaults,errors=remount-ro 0 1\nUUID=E /boot/efi vfat umask=0077 0 1\n")
        self.assertEqual(
            restore_system.rewrite_fstab("", "R", "E"),
            "UUID=R / ext4 defaults,errors=remount-ro 0 1\nUUID=E /boot/efi vfat umask=0077 0 1\n",
        )


class SystemFixTest(unittest.TestCase):
    def test_comments_out_the_forced_partuuid(self):
        text = 'GRUB_FORCE_PARTUUID="abc-1"\nGRUB_TIMEOUT=0\n'
        self.assertEqual(
            restore_system.disable_force_partuuid(text),
            '# upkeep-restore: GRUB_FORCE_PARTUUID="abc-1"\nGRUB_TIMEOUT=0\n',
        )

    def test_fix_system_writes_network_fstab_and_machine_id(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            for folder in ["etc/netplan", "etc/cloud/cloud.cfg.d", "etc/default/grub.d", "var/lib/dbus"]:
                (target / folder).mkdir(parents=True)
            (target / "etc/fstab").write_text(CLOUD_FSTAB)
            (target / "etc/netplan/50-cloud-init.yaml").write_text("network: {}\n")
            (target / "etc/default/grub.d/40-force-partuuid.cfg").write_text('GRUB_FORCE_PARTUUID="x"\n')
            (target / "etc/machine-id").write_text("0123456789abcdef\n")
            (target / "var/lib/dbus/machine-id").write_text("0123456789abcdef\n")

            restore_system.fix_system(target, "R", "E", new_machine_id=True, network="dhcp")

            self.assertEqual(
                sorted(path.name for path in (target / "etc/netplan").iterdir()),
                ["50-cloud-init.yaml.upkeep-restore", "90-upkeep-restore.yaml"],
            )
            self.assertIn('name: "en*"', (target / restore_system.NETPLAN_FILE).read_text())
            self.assertEqual((target / restore_system.NETPLAN_FILE).stat().st_mode & 0o777, 0o600)
            self.assertEqual(
                (target / "etc/cloud/cloud.cfg.d/99-upkeep-restore.cfg").read_text(), "network: {config: disabled}\n"
            )
            self.assertIn("UUID=R / ext4", (target / "etc/fstab").read_text())
            self.assertEqual((target / "etc/initramfs-tools/conf.d/resume").read_text(), "RESUME=none\n")
            self.assertTrue((target / "etc/default/grub.d/40-force-partuuid.cfg").read_text().startswith("#"))
            self.assertEqual((target / "etc/machine-id").read_text(), "")
            self.assertFalse((target / "var/lib/dbus/machine-id").exists())
            self.assertEqual((target / "tmp").stat().st_mode & 0o7777, 0o1777)

    def test_keep_network_and_machine_id(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            (target / "etc/netplan").mkdir(parents=True)
            (target / "etc/netplan/50-cloud-init.yaml").write_text("network: {}\n")
            (target / "etc/machine-id").write_text("abc\n")

            restore_system.fix_system(target, "R", "E", new_machine_id=False, network="keep")

            self.assertEqual([path.name for path in (target / "etc/netplan").iterdir()], ["50-cloud-init.yaml"])
            self.assertEqual((target / "etc/machine-id").read_text(), "abc\n")


class ArgumentsTest(unittest.TestCase):
    def test_needs_one_auth_and_the_repository_key(self):
        base = ["--disk", "/dev/vdb", "--ssh", "u@h", "--repository", "/r/vm-a", "--host-key", "ssh-ed25519 AAAA"]
        args = restore_system.parse_arguments(base + ["--key-file", "k", "--repository-key-file", "rk"])
        self.assertEqual((args.snapshot, args.network, args.new_machine_id), ("latest", "dhcp", False))
        for extra in [["--repository-key-file", "rk"], ["--key-file", "k", "--password-file", "p"]]:
            with self.assertRaises(SystemExit), unittest.mock.patch("sys.stderr"):
                restore_system.parse_arguments(base + extra)


if __name__ == "__main__":
    unittest.main()
