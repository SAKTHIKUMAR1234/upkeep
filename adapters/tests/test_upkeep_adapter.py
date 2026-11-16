import contextlib
import http.server
import io
import json
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

import upkeep_adapter
from upkeep_adapter import AdapterError, CommandError, Context


def lines_of(output: str) -> list[dict]:
    return [json.loads(line) for line in output.splitlines()]


class OutputTest(unittest.TestCase):
    def test_writes_one_json_object_per_line(self):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            upkeep_adapter.log("disk low", level="warn")
            upkeep_adapter.progress(40, "copy")
            upkeep_adapter.result(snapshot="snap-1")

        self.assertEqual(
            lines_of(stdout.getvalue()),
            [
                {"type": "log", "level": "warn", "message": "disk low"},
                {"type": "progress", "percent": 40, "message": "copy"},
                {"type": "result", "snapshot": "snap-1"},
            ],
        )


class ContextTest(unittest.TestCase):
    def test_reads_stdin_context_and_adapter_files(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            (folder / "adapter.toml").write_text(
                'name = "os"\n[[service]]\nname = "ssh"\nkind = "systemd"\nunit = "ssh.service"\n'
            )
            (folder / "adapter.conf.toml").write_text('hold = ["mariadb-*"]\n')
            stdin = io.StringIO(json.dumps({"job_id": 7, "state_dir": "/var/lib/upkeep/adapters/os"}))

            context = upkeep_adapter.load_context(folder, stdin)

        self.assertEqual(context.data["job_id"], 7)
        self.assertEqual(context.state_dir, Path("/var/lib/upkeep/adapters/os"))
        self.assertEqual(context.manifest["service"][0]["unit"], "ssh.service")
        self.assertEqual(context.config["hold"], ["mariadb-*"])

    def test_empty_stdin_and_missing_files_give_empty_values(self):
        with tempfile.TemporaryDirectory() as directory:
            context = upkeep_adapter.load_context(Path(directory), io.StringIO(""))
        self.assertEqual((context.data, context.manifest, context.config), ({}, {}, {}))
        self.assertEqual((context.params, context.secrets), ({}, {}))

    def test_reads_params_and_the_secrets_file(self):
        with tempfile.TemporaryDirectory() as directory:
            secrets_file = Path(directory) / "7.json"
            secrets_file.write_text(json.dumps({"repository_key": "k"}))
            data = {"params": {"destination": {"name": "laptop"}}, "secrets_file": str(secrets_file)}
            context = Context(data=data, adapter_dir=Path(directory))

            self.assertEqual(context.params["destination"]["name"], "laptop")
            self.assertEqual(context.secrets, {"repository_key": "k"})
            secrets_file.unlink()
            self.assertEqual(context.secrets, {})


class RunTest(unittest.TestCase):
    def test_logs_command_output_and_exit_code(self):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            output = upkeep_adapter.run(["sh", "-c", "echo one; echo two >&2"])

        self.assertEqual(output, "one\ntwo\n")
        messages = [line["message"] for line in lines_of(stdout.getvalue())]
        self.assertEqual(messages, ["$ sh -c echo one; echo two >&2", "one", "two", "exit code 0"])

    def test_failure_raises_with_code_and_output(self):
        with contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(CommandError) as raised:
                upkeep_adapter.run(["sh", "-c", "echo broken; exit 3"])
            self.assertEqual(upkeep_adapter.run(["sh", "-c", "exit 4"], check=False, quiet=True), "")

        self.assertEqual(raised.exception.code, 3)
        self.assertIn("exited with 3: broken", str(raised.exception))

    def test_quiet_logs_nothing(self):
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            self.assertEqual(upkeep_adapter.run(["echo", "hi"], quiet=True), "hi\n")
        self.assertEqual(stdout.getvalue(), "")


class MainTest(unittest.TestCase):
    def run_main(self, action: str, actions: dict) -> tuple[int, list[dict]]:
        stdout = io.StringIO()
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.object(sys, "argv", ["adapter", action]),
            mock.patch.object(sys, "stdin", io.StringIO("{}")),
            contextlib.redirect_stdout(stdout),
            self.assertRaises(SystemExit) as exited,
        ):
            upkeep_adapter.main(actions, Path(directory))
        return exited.exception.code, lines_of(stdout.getvalue())

    def test_exit_codes(self):
        def fail(context):
            raise AdapterError("backup failed")

        actions = {"ok": lambda context: None, "reboot": lambda context: 10, "fail": fail}
        self.assertEqual(self.run_main("ok", actions)[0], 0)
        self.assertEqual(self.run_main("reboot", actions)[0], 10)

        code, lines = self.run_main("fail", actions)
        self.assertEqual(code, 1)
        self.assertEqual(lines[-1], {"type": "log", "level": "error", "message": "backup failed"})

        code, lines = self.run_main("nope", actions)
        self.assertEqual(code, 2)
        self.assertIn("unknown action 'nope'", lines[0]["message"])


class DurationTest(unittest.TestCase):
    def test_parses_numbers_and_units(self):
        self.assertEqual(upkeep_adapter.parse_duration(None, default=3), 3)
        self.assertEqual(upkeep_adapter.parse_duration(30), 30)
        self.assertEqual(upkeep_adapter.parse_duration("10s"), 10)
        self.assertEqual(upkeep_adapter.parse_duration("1h30m"), 5400)
        self.assertEqual(upkeep_adapter.parse_duration("500ms"), 0.5)

    def test_rejects_unknown_text(self):
        for value in ("soon", "5 days", ""):
            with self.assertRaises(AdapterError):
                upkeep_adapter.parse_duration(value)


class StatusHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200 if self.path == "/ok" else 503)
        self.end_headers()

    def log_message(self, *args):
        pass


class HealthTest(unittest.TestCase):
    def test_command_and_http_pings(self):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), StatusHandler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        base = f"http://127.0.0.1:{server.server_port}"
        cwd = Path(".")

        self.assertIsNone(upkeep_adapter.check_ping({"http": f"{base}/ok"}, cwd))
        self.assertEqual(upkeep_adapter.check_ping({"http": f"{base}/down"}, cwd), "expected HTTP 200, got 503")
        self.assertIsNone(upkeep_adapter.check_ping({"http": f"{base}/down", "expect": 503}, cwd))
        self.assertIsNone(upkeep_adapter.check_ping({"command": "true"}, cwd))
        self.assertEqual(upkeep_adapter.check_ping({"command": "echo nope; exit 1"}, cwd), "nope")
        self.assertEqual(
            upkeep_adapter.check_ping({"command": "sleep 5", "timeout": "200ms"}, cwd), "timed out after 0.2s"
        )

    def test_reports_each_failed_check(self):
        manifest = {
            "service": [{"name": "ghost", "kind": "process", "pattern": "no-such-process-[x]yz"}],
            "ping": [{"name": "yes", "command": "true"}, {"name": "no", "command": "exit 1"}],
        }
        context = Context(data={}, adapter_dir=Path("."), manifest=manifest)
        with contextlib.redirect_stdout(io.StringIO()):
            failures = upkeep_adapter.check_manifest_health(context)

        self.assertEqual(failures, ["service ghost: exit code 1", "ping no: exit code 1"])

    def test_a_stopped_systemd_unit_reports_its_state(self):
        def systemctl(command, timeout):
            return 3, "" if "--quiet" in command else "failed\n"

        with mock.patch.object(upkeep_adapter, "run_check", side_effect=systemctl):
            failure = upkeep_adapter.check_service({"kind": "systemd", "unit": "testapp.service"})
        self.assertEqual(failure, "failed")

    def test_unknown_service_kind(self):
        self.assertEqual(upkeep_adapter.check_service({"kind": "windows"}), "unknown service kind 'windows'")


if __name__ == "__main__":
    unittest.main()
