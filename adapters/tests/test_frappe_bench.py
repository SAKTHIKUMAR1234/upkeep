import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "frappe-bench"))

import frappe_bench
from upkeep_adapter import AdapterError, CommandError, Context

FIXTURES = Path(__file__).parent / "fixtures"
CONFIG = {"pilot": "pilot", "bench": "main", "bench_dir": "/benches/main", "sites": ["a.local", "b.local"]}


class FakeShell:
    """Answers commands by their trailing arguments and records every command."""

    def __init__(self, answers: dict[tuple, str]):
        self.answers = answers
        self.commands: list[list[str]] = []

    def __call__(self, command: list[str], **kwargs) -> str:
        self.commands.append(command)
        for suffix, answer in self.answers.items():
            if tuple(command[-len(suffix) :]) == suffix:
                if isinstance(answer, Exception):
                    raise answer
                return answer
        return ""

    def frappe_calls(self) -> list[tuple]:
        return [tuple(command[command.index("--site") + 1 :]) for command in self.commands if "--site" in command]


def show_config(site: str, maintenance_mode: int) -> str:
    return json.dumps({site: {"db_name": "x", "maintenance_mode": maintenance_mode}})


class ParseTest(unittest.TestCase):
    def test_backup_summary_paths_are_absolute(self):
        files = frappe_bench.parse_backup_summary(
            (FIXTURES / "frappe-backup.txt").read_text(), Path("/benches/main/sites")
        )
        self.assertEqual(sorted(files), ["config", "database", "private", "public"])
        self.assertEqual(
            files["database"],
            "/benches/main/sites/site.local/private/backups/20261116_140210-site_local-database.sql.gz",
        )

    def test_upstream_and_behind_counts(self):
        self.assertEqual(
            frappe_bench.parse_upstream("version-15\torigin\trefs/heads/version-15\n"),
            ("version-15", "origin", "refs/heads/version-15"),
        )
        self.assertIsNone(frappe_bench.parse_upstream("feature\t\t\n"))
        self.assertEqual(frappe_bench.parse_ahead_behind("2\t17\n"), (2, 17))


class SchedulerStatusTest(unittest.TestCase):
    def test_unreadable_status_fails_loudly(self):
        shell = FakeShell({("status", "--format", "json"): "Traceback: no database\n"})
        with mock.patch.object(frappe_bench, "run", shell), self.assertRaises(AdapterError) as raised:
            frappe_bench.scheduler_status(frappe_bench.Bench(CONFIG), "a.local")
        self.assertIn("cannot read the scheduler status", str(raised.exception))


class ConfigTest(unittest.TestCase):
    def test_missing_keys_are_named(self):
        with self.assertRaises(AdapterError) as raised:
            frappe_bench.Bench({"pilot": "pilot"})
        self.assertIn("bench, bench_dir, sites", str(raised.exception))

    def test_current_user_needs_no_prefix(self):
        self.assertEqual(frappe_bench.user_prefix(None), [])
        current = frappe_bench.pwd.getpwuid(frappe_bench.os.geteuid()).pw_name
        self.assertEqual(frappe_bench.user_prefix(current), [])

    def test_commands_go_through_pilot(self):
        shell = FakeShell({})
        with mock.patch.object(frappe_bench, "run", shell):
            bench = frappe_bench.Bench({**CONFIG, "pilot": "uv run pilot"})
            bench.frappe("a.local", "migrate")
            bench.pilot("build")
        self.assertEqual(
            shell.commands,
            [
                ["uv", "run", "pilot", "--bench", "main", "frappe", "--site", "a.local", "migrate"],
                ["uv", "run", "pilot", "--bench", "main", "--yes", "build"],
            ],
        )


class MaintenanceTest(unittest.TestCase):
    def setUp(self):
        self.state_dir = Path(tempfile.mkdtemp())
        self.context = Context(data={"state_dir": str(self.state_dir)}, adapter_dir=Path("."), config=CONFIG)

    def run_action(self, action, shell: FakeShell):
        with mock.patch.object(frappe_bench, "run", shell), contextlib.redirect_stdout(io.StringIO()):
            action(self.context)

    def test_scheduler_and_maintenance_mode_are_restored_per_site(self):
        on = FakeShell(
            {
                ("a.local", "scheduler", "status", "--format", "json"): '{"status": "enabled", "site": "a.local"}\n',
                ("b.local", "scheduler", "status", "--format", "json"): '{"status": "disabled", "site": "b.local"}\n',
                ("a.local", "show-config", "--format", "json"): show_config("a.local", maintenance_mode=0),
                ("b.local", "show-config", "--format", "json"): show_config("b.local", maintenance_mode=1),
            }
        )
        self.run_action(frappe_bench.maintenance_on, on)
        self.assertIn(("a.local", "scheduler", "disable"), on.frappe_calls())
        self.assertNotIn(("b.local", "scheduler", "disable"), on.frappe_calls())
        self.assertIn(("a.local", "set-maintenance-mode", "on"), on.frappe_calls())
        self.assertIn(("b.local", "set-maintenance-mode", "on"), on.frappe_calls())

        off = FakeShell({})
        self.run_action(frappe_bench.maintenance_off, off)
        self.assertEqual(
            off.frappe_calls(),
            [("a.local", "scheduler", "enable"), ("a.local", "set-maintenance-mode", "off")],
        )
        self.assertFalse((self.state_dir / "site_state.json").exists())

    def test_maintenance_off_leaves_sites_without_remembered_state(self):
        state = {"a.local": {"scheduler": "disabled", "maintenance_mode": False}}
        (self.state_dir / "site_state.json").write_text(json.dumps(state))
        off = FakeShell({})
        self.run_action(frappe_bench.maintenance_off, off)
        self.assertEqual(off.frappe_calls(), [("a.local", "set-maintenance-mode", "off")])

    def test_a_second_maintenance_on_keeps_the_first_state(self):
        state = {
            "a.local": {"scheduler": "enabled", "maintenance_mode": False},
            "b.local": {"scheduler": "disabled", "maintenance_mode": True},
        }
        (self.state_dir / "site_state.json").write_text(json.dumps(state))
        shell = FakeShell({})
        self.run_action(frappe_bench.maintenance_on, shell)

        self.assertFalse(any("status" in call or "show-config" in call for call in shell.frappe_calls()))
        self.assertEqual(json.loads((self.state_dir / "site_state.json").read_text()), state)

    def test_unreadable_maintenance_mode_fails_without_leaking_config(self):
        shell = FakeShell({("show-config", "--format", "json"): 'Traceback: db_password = "secret"'})
        with mock.patch.object(frappe_bench, "run", shell), self.assertRaises(AdapterError) as raised:
            frappe_bench.is_maintenance_mode_on(frappe_bench.Bench(CONFIG), "a.local")
        self.assertNotIn("secret", str(raised.exception))

    def test_waits_for_jobs_and_fails_on_other_errors(self):
        not_ready = CommandError(
            ["ready-for-migration"], 1, "NOT READY for migration: site a.local has pending background jobs"
        )
        status = '{"status": "disabled", "site": "a.local"}'
        config = ("show-config", "--format", "json")
        shell = FakeShell(
            {
                ("ready-for-migration",): not_ready,
                ("status", "--format", "json"): status,
                config: json.dumps({"a.local": {}, "b.local": {}}),
            }
        )
        self.context.config = {**CONFIG, "job_wait_timeout": "1ms"}
        with mock.patch.object(frappe_bench.time, "sleep"), self.assertRaises(AdapterError) as raised:
            self.run_action(frappe_bench.maintenance_on, shell)
        self.assertIn("background jobs still running", str(raised.exception))

        refused = CommandError(["ready-for-migration"], 1, "redis refused")
        broken = FakeShell(
            {
                ("ready-for-migration",): refused,
                ("status", "--format", "json"): status,
                config: json.dumps({"a.local": {}, "b.local": {}}),
            }
        )
        with self.assertRaises(CommandError):
            self.run_action(frappe_bench.maintenance_on, broken)


class UpdateTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.state_dir = Path(temporary.name)

    def context(self, **config) -> Context:
        return Context(data={"state_dir": str(self.state_dir)}, adapter_dir=Path("."), config={**CONFIG, **config})

    def write_state(self, state: dict) -> None:
        (self.state_dir / frappe_bench.UPDATE_STATE_FILE).write_text(json.dumps(state))

    def run_action(self, action, shell: FakeShell, context: Context) -> None:
        with mock.patch.object(frappe_bench, "run", shell), contextlib.redirect_stdout(io.StringIO()):
            action(context)

    def test_refuses_to_update_without_a_backup(self):
        shell = FakeShell({("list-apps",): "frappe\n"})
        with self.assertRaises(AdapterError):
            self.run_action(frappe_bench.update, shell, self.context())
        self.assertFalse(any("pull" in command for command in shell.commands))

    def test_stops_at_the_first_failed_pull(self):
        self.write_state({"databases": {"a.local": "a.sql.gz"}})
        pull_failed = CommandError(["git", "pull", "--ff-only"], 128, "fatal: Not possible to fast-forward")
        shell = FakeShell({("list-apps",): "frappe\nhrms\n", ("pull", "--ff-only"): pull_failed})
        with self.assertRaises(CommandError):
            self.run_action(frappe_bench.update, shell, self.context())

        self.assertEqual(sum("pull" in command for command in shell.commands), 1)
        self.assertEqual(shell.frappe_calls(), [])

    def test_skipped_apps_are_not_pulled(self):
        self.write_state({"databases": {"a.local": "a.sql.gz"}})
        shell = FakeShell({("list-apps",): "frappe\nprivate_app\nhrms\n"})
        self.run_action(frappe_bench.update, shell, self.context(skip_apps=["private_app"]))

        pulled = [command[command.index("-C") + 1] for command in shell.commands if "pull" in command]
        self.assertEqual(pulled, ["/benches/main/apps/frappe", "/benches/main/apps/hrms"])

    def test_rollback_resets_apps_and_restores_the_backup(self):
        password_file = self.state_dir / "root-password"
        password_file.write_text("s3cret\n")
        self.write_state({"databases": {"a.local": "a/private/backups/db.sql.gz"}, "apps": {"frappe": "abc123"}})
        shell = FakeShell({})
        context = self.context(restore={"db_root_password_file": str(password_file)}, restart=False)
        self.run_action(frappe_bench.rollback, shell, context)

        self.assertIn(["git", "-C", "/benches/main/apps/frappe", "reset", "--hard", "abc123"], shell.commands)
        restore = shell.frappe_calls()[0]
        self.assertEqual(restore[:4], ("a.local", "restore", "a/private/backups/db.sql.gz", "--force"))
        self.assertFalse((self.state_dir / frappe_bench.UPDATE_STATE_FILE).exists())

    def test_failed_restore_hides_the_root_password(self):
        password_file = self.state_dir / "root-password"
        password_file.write_text("s3cret")
        self.write_state({"databases": {"a.local": "db.sql.gz"}, "apps": {}})
        failed = CommandError(["pilot", "restore", "--db-root-password", "s3cret"], 1, "Access denied, used s3cret")
        shell = FakeShell({("--db-root-password", "s3cret"): failed})
        context = self.context(restore={"db_root_password_file": str(password_file)})
        with self.assertRaises(AdapterError) as raised:
            self.run_action(frappe_bench.rollback, shell, context)
        self.assertNotIn("s3cret", str(raised.exception))

    def test_rollback_without_an_update_changes_nothing(self):
        shell = FakeShell({})
        self.run_action(frappe_bench.rollback, shell, self.context())
        self.assertEqual(shell.commands, [])


if __name__ == "__main__":
    unittest.main()
