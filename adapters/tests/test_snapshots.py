import contextlib
import io
import json
import stat
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "ubuntu-os"))

import snapshots
from upkeep_adapter import AdapterError

DESTINATION = {
    "name": "laptop",
    "user": "backup",
    "address": "10.0.2.2",
    "port": 22,
    "repository": "/backups/upkeep/vm-a",
    "auth": "key",
    "host_keys": ["ssh-ed25519 AAAAC3Nza", "ssh-rsa AAAAB3Nz"],
}
PROC_SWAPS = """Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority
/swap.img                               file\t\t2097148\t\t0\t\t-2
/dev/nvme0n1p3                          partition\t16777212\t0\t\t-3
/var/swap2                              file\t\t1024\t\t0\t\t-4
"""
FORGET_OUTPUT = (
    '[{"tags":null,"host":"vm-a","paths":null,'
    '"keep":[{"short_id":"aaaa1111"},{"short_id":"bbbb2222"}],'
    '"remove":[{"short_id":"cccc3333"}],"reasons":[]}]\n'
    "1 snapshots have been removed, running prune\n"
)


class ConfigTest(unittest.TestCase):
    def test_defaults(self):
        config = snapshots.SnapshotConfig.from_config({})
        self.assertEqual(config.retention, {"daily": 7, "weekly": 4, "monthly": 12})
        self.assertIn("/proc/*", config.exclude)
        self.assertEqual(config.extra_mounts, ["/boot", "/boot/efi"])

    def test_overrides_and_partial_retention(self):
        config = snapshots.SnapshotConfig.from_config(
            {"snapshot": {"extra_exclude": ["/srv/scratch"], "extra_mounts": ["/data"], "retention": {"daily": 3}}}
        )
        self.assertEqual(config.extra_exclude, ["/srv/scratch"])
        self.assertEqual(config.extra_mounts, ["/data"])
        self.assertEqual(config.retention, {"daily": 3, "weekly": 4, "monthly": 12})

    def test_rejects_unknown_fields_and_bad_counts(self):
        for table in [{"excludes": []}, {"retention": {"yearly": 1}}, {"retention": {"daily": -1}}]:
            with self.assertRaises(AdapterError, msg=table):
                snapshots.SnapshotConfig.from_config({"snapshot": table})


class ExcludeTest(unittest.TestCase):
    def test_swap_files_only(self):
        self.assertEqual(snapshots.swap_files(PROC_SWAPS), ["/swap.img", "/var/swap2"])
        self.assertEqual(snapshots.swap_files("Filename Type Size Used Priority\n"), [])

    def test_excludes_add_swap_cache_and_secrets_without_duplicates(self):
        config = snapshots.SnapshotConfig(extra_exclude=["/srv/tmp/*"], cache_dir="/var/cache/upkeep/restic")
        excludes = snapshots.build_excludes(config, ["/swap.img", "/var/swap2"], ["/var/lib/upkeep/secrets"])

        self.assertEqual(excludes[: len(snapshots.DEFAULT_EXCLUDES)], snapshots.DEFAULT_EXCLUDES)
        self.assertEqual(
            excludes[len(snapshots.DEFAULT_EXCLUDES) :],
            ["/srv/tmp/*", "/var/swap2", "/var/cache/upkeep/restic", "/var/lib/upkeep/secrets"],
        )

    def test_kernel_folders_keep_their_mount_point(self):
        for pattern in ["/proc/*", "/sys/*", "/dev/*", "/run/*", "/tmp/*"]:
            self.assertIn(pattern, snapshots.DEFAULT_EXCLUDES)
        self.assertNotIn("/proc", snapshots.DEFAULT_EXCLUDES)

    def test_backup_paths_keep_only_mounted_extra_mounts(self):
        mounted = {"/boot/efi", "/data"}
        paths = snapshots.backup_paths(["/boot", "/boot/efi", "/data", "/"], mounted.__contains__)
        self.assertEqual(paths, ["/", "/boot/efi", "/data"])


class SshTest(unittest.TestCase):
    def test_key_auth_command(self):
        destination = snapshots.Destination.from_params({"destination": DESTINATION})
        command = snapshots.ssh_command(destination, Path("/t/known_hosts"), Path("/t/ssh-key"))
        text = snapshots.shell_join(command)
        self.assertTrue(text.startswith("ssh -F /dev/null -p 22 -l backup -o UserKnownHostsFile=/t/known_hosts"))
        self.assertIn("-o StrictHostKeyChecking=yes -o HostKeyAlias=upkeep-destination", text)
        self.assertIn("-i /t/ssh-key -o IdentitiesOnly=yes -o BatchMode=yes", text)
        self.assertTrue(text.endswith("10.0.2.2 -s sftp"))

    def test_password_auth_command_has_no_password(self):
        destination = snapshots.Destination.from_params({"destination": DESTINATION})
        text = snapshots.shell_join(snapshots.ssh_command(destination, Path("/t/known_hosts"), None))
        self.assertIn("PubkeyAuthentication=no", text)
        self.assertNotIn("BatchMode", text)
        self.assertNotIn(",", text, "restic splits -o values on commas")

    def test_shell_join_quotes_spaces(self):
        self.assertEqual(snapshots.shell_join(["ssh", "-i", "/my keys/id"]), 'ssh -i "/my keys/id"')

    def test_destination_must_be_complete(self):
        with self.assertRaises(AdapterError):
            snapshots.Destination.from_params({})
        with self.assertRaises(AdapterError):
            snapshots.Destination.from_params({"destination": {"user": "x"}})

    def test_repository_files_are_private_and_removed(self):
        destination = snapshots.Destination.from_params({"destination": DESTINATION})
        secrets = {"repository_key": "k" * 64, "ssh_password": "pa55word"}

        with snapshots.open_repository(destination, secrets, sys.executable, "/tmp/cache") as repository:
            folder = repository.folder
            modes = {path.name: stat.S_IMODE(path.stat().st_mode) for path in folder.iterdir()}
            known_hosts = (folder / "known_hosts").read_text()
            command = repository.command("snapshots")
            env = repository.env

        self.assertEqual(
            modes, {"repository-key": 0o600, "known_hosts": 0o600, "ssh-password": 0o600, "askpass": 0o700}
        )
        self.assertFalse(folder.exists())
        self.assertEqual(known_hosts, "upkeep-destination ssh-ed25519 AAAAC3Nza\nupkeep-destination ssh-rsa AAAAB3Nz\n")
        self.assertEqual(command[:3], [sys.executable, "-r", "sftp:upkeep-destination:/backups/upkeep/vm-a"])
        self.assertNotIn("pa55word", " ".join(command) + json.dumps(env))
        self.assertEqual(env["SSH_ASKPASS_REQUIRE"], "force")

    def test_repository_needs_the_secrets(self):
        destination = snapshots.Destination.from_params({"destination": DESTINATION})
        for secrets in [{}, {"repository_key": "k"}]:
            with self.assertRaises(AdapterError), snapshots.open_repository(destination, secrets, "x", "/c"):
                pass


class BackupOutputTest(unittest.TestCase):
    def test_keeps_the_summary_counts_errors_and_throttles_progress(self):
        ticks = iter([0.0, 0.0, 10.0, 40.0, 40.0])
        messages = snapshots.BackupMessages(clock=lambda: next(ticks))
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            for status in (0.1, 0.2, 0.9):
                messages({"message_type": "status", "percent_done": status, "bytes_done": 3 << 20, "files_done": 5})
            messages({"message_type": "error", "error": {"message": "permission denied"}, "item": "/x"})
            messages({"message_type": "summary", "snapshot_id": "abc", "data_added": 10, "data_added_packed": 4})

        lines = [json.loads(line) for line in stdout.getvalue().splitlines()]
        self.assertEqual([line["percent"] for line in lines if line["type"] == "progress"], [10, 90])
        self.assertEqual(messages.errors, 1)
        self.assertEqual(snapshots.summary_result(messages.summary)["data_added_packed"], 4)

    def test_tags_name_the_job_and_plan(self):
        self.assertEqual(snapshots.snapshot_tags(12, "os-update"), ["upkeep", "job=12", "plan=os-update"])


class RetentionTest(unittest.TestCase):
    def test_forget_groups_by_host_and_keeps_the_policy(self):
        args = snapshots.forget_args("vm-a", {"daily": 7, "weekly": 4, "monthly": 12})
        self.assertEqual(args[:3], ["forget", "--json", "--prune"])
        self.assertIn("--group-by host", " ".join(args))
        self.assertIn("--tag upkeep", " ".join(args))
        self.assertIn("--keep-daily 7 --keep-weekly 4 --keep-monthly 12", " ".join(args))

    def test_parses_kept_and_removed(self):
        self.assertEqual(
            snapshots.parse_forget(FORGET_OUTPUT),
            {"kept": 2, "removed": 1, "kept_ids": ["aaaa1111", "bbbb2222"], "removed_ids": ["cccc3333"]},
        )
        self.assertEqual(snapshots.parse_forget("")["kept"], 0)


class RestoreTest(unittest.TestCase):
    def test_restore_args(self):
        self.assertEqual(
            snapshots.restore_args("abc", "/root/restore", "/var/lib/mysql"),
            ["restore", "abc", "--target", "/root/restore", "--include", "/var/lib/mysql"],
        )
        self.assertEqual(snapshots.restore_args("latest", "/r", None), ["restore", "latest", "--target", "/r"])

    def test_refuses_to_restore_over_the_running_system(self):
        for target in ["/", "/.", "relative"]:
            with self.assertRaises(AdapterError, msg=target):
                snapshots.restore_args("abc", target, None)


if __name__ == "__main__":
    unittest.main()
