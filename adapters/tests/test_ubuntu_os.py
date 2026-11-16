import contextlib
import io
import sys
import tomllib
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "ubuntu-os"))

import ubuntu_os
from upkeep_adapter import AdapterError, Context

FIXTURES = Path(__file__).parent / "fixtures"


def fixture(name: str) -> str:
    return (FIXTURES / name).read_text()


class SimulationTest(unittest.TestCase):
    def test_parses_upgrades_installs_and_removals(self):
        plan = ubuntu_os.parse_simulation(fixture("apt-simulate.txt"))

        names = [package["name"] for package in plan["upgrades"]]
        self.assertEqual(
            names,
            ["curl", "libcurl4t64", "linux-image-7.0.0-44-generic", "mariadb-server", "openssh-server", "systemd"],
        )
        self.assertEqual(
            plan["upgrades"][0],
            {"name": "curl", "current": "8.14.1-2ubuntu1", "new": "8.14.1-2ubuntu1.1", "risky": False},
        )
        self.assertIsNone(plan["upgrades"][2]["current"])
        self.assertEqual(plan["removals"], [{"name": "libfoo1", "current": "1.2-1", "risky": False}])

    def test_marks_risky_packages(self):
        plan = ubuntu_os.parse_simulation(fixture("apt-simulate.txt"))
        risky = [package["name"] for package in plan["upgrades"] if package["risky"]]
        self.assertEqual(risky, ["linux-image-7.0.0-44-generic", "mariadb-server", "openssh-server", "systemd"])

    def test_nothing_to_do(self):
        self.assertEqual(
            ubuntu_os.parse_simulation("0 upgraded, 0 newly installed.\n"), {"upgrades": [], "removals": []}
        )


class HoldTest(unittest.TestCase):
    def test_holds_only_matching_packages_not_already_held(self):
        installed = ["curl", "docker-ce", "docker-ce-cli", "mariadb-server", "mariadb-client"]
        held = ubuntu_os.packages_to_hold(["mariadb-*", "docker-ce", "postgresql*"], installed, {"mariadb-client"})
        self.assertEqual(held, ["docker-ce", "mariadb-server"])


class UnitsTest(unittest.TestCase):
    def test_failed_units_skip_the_ignore_list(self):
        with mock.patch.object(ubuntu_os, "run", return_value=fixture("systemctl-failed.txt")):
            self.assertEqual(ubuntu_os.failed_units(["fwupd-*"]), ["motd-news.service"])

    def test_verify_fails_on_a_failed_unit(self):
        context = Context(data={}, adapter_dir=Path("."), config={"ignore_failed_units": ["fwupd-*"]})
        with (
            mock.patch.object(ubuntu_os, "run", return_value=fixture("systemctl-failed.txt")),
            contextlib.redirect_stdout(io.StringIO()),
            self.assertRaises(AdapterError) as raised,
        ):
            ubuntu_os.verify(context)
        self.assertIn("failed unit motd-news.service", str(raised.exception))


class DiscoverTest(unittest.TestCase):
    def test_drafts_services_from_units_and_containers(self):
        system = ubuntu_os.parse_unit_names(fixture("systemctl-running.txt"))
        user = ubuntu_os.parse_unit_names(fixture("systemctl-user-running.txt"))

        services = ubuntu_os.draft_services(system, {"frappe": user}, ["mariadb", "relay-hub"])

        self.assertEqual(
            services,
            [
                {"kind": "systemd", "unit": "mariadb.service", "name": "mariadb"},
                {"kind": "systemd", "unit": "nginx.service", "name": "nginx"},
                {"kind": "systemd", "unit": "ssh.service", "name": "ssh"},
                {"kind": "systemd-user", "unit": "bench-web.service", "user": "frappe", "name": "bench-web"},
                {"kind": "systemd-user", "unit": "nginx.service", "user": "frappe", "name": "frappe-nginx"},
                {"kind": "docker", "container": "mariadb", "name": "docker-mariadb"},
                {"kind": "docker", "container": "relay-hub", "name": "relay-hub"},
            ],
        )

    def test_rendered_draft_is_valid_manifest_toml(self):
        services = ubuntu_os.draft_services(["ssh.service"], {"frappe": ["bench-web.service"]}, ["relay-hub"])
        parsed = tomllib.loads(ubuntu_os.render_services(services))
        self.assertEqual(parsed["service"], services)


if __name__ == "__main__":
    unittest.main()
