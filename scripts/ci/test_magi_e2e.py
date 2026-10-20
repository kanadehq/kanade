"""外部サービスなしで空振り検出を検証する。"""

import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "magi_e2e", Path(__file__).with_name("magi-e2e.py"))
e2e = importlib.util.module_from_spec(spec)
spec.loader.exec_module(e2e)


def output(running, passed, ignored=0):
    return (f"running {running} tests\n"
            f"test result: ok. {passed} passed; 0 failed; {ignored} ignored;\n")


class E2eTests(unittest.TestCase):
    def test_empty_ignored_only_and_mismatched_counts_fail(self):
        for text in ("", output(0, 0), output(5, 0, 5), output(5, 4),
                     output(2, 3), output(1, 1) + "running 2 tests\n",
                     "test result: ok. 2 passed; 0 failed; 0 ignored;\n"):
            with self.subTest(text=text):
                self.assertFalse(e2e.executed_tests(text))

    def test_multiple_harness_counts_are_aggregated(self):
        self.assertTrue(e2e.executed_tests(output(2, 2) + output(6, 6)))
        self.assertTrue(e2e.executed_tests(output(0, 0) + output(3, 3)))
        self.assertFalse(e2e.executed_tests(output(2, 2) + output(6, 0, 6)))
        self.assertFalse(e2e.executed_tests(output(2, 3) + output(6, 5)))

    def test_child_failure_is_preserved(self):
        with patch.object(e2e.subprocess, "run", return_value=
                          subprocess.CompletedProcess([], 17, output(2, 2))):
            self.assertEqual(e2e.run(["cargo"], {}), 17)

    def test_success_exit_with_no_execution_fails(self):
        with patch.object(e2e.subprocess, "run", return_value=
                          subprocess.CompletedProcess([], 0, output(0, 0))):
            self.assertEqual(e2e.run(["cargo"], {}), 1)

    def test_stage_filters_and_socket_opt_in(self):
        commands = list(e2e.commands())
        self.assertIn("--ignored", commands[0])
        for module in ("package_manifest", "publisher_trust", "package_admission"):
            self.assertTrue(any(module + "::tests::" in cmd for cmd in commands))

    def test_nats_version_matches_actual_ci_pin(self):
        for version, valid in (("2.14.1", True), ("0.0.0", False)):
            with self.subTest(version=version), patch.object(
                e2e.subprocess, "run", return_value=subprocess.CompletedProcess(
                    [], 0, f"nats-server: v{version}\n")):
                if valid:
                    e2e.check_nats()
                else:
                    with self.assertRaises(ValueError):
                        e2e.check_nats()


if __name__ == "__main__":
    unittest.main()
