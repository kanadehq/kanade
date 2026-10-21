"""外部サービスなしで空振り検出を検証する。"""

import importlib.util
from pathlib import Path
import subprocess
import tempfile
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


class NatsSetupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.cache = self.root / "target/magi-tools/nats/2.14.1/linux-amd64/bin/nats-server"
        patcher = patch.object(e2e, "ROOT", self.root)
        patcher.start()
        self.addCleanup(patcher.stop)
        for target, value in (("nats_version", "2.14.1"), ("nats_platform", "linux-amd64")):
            patcher = patch.object(e2e, target, return_value=value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def download(self, version, host, destination):
        self.assertEqual((version, host), ("2.14.1", "linux-amd64"))
        destination.write_bytes(b"pinned broker")

    def test_wrong_global_version_downloads_and_prepends_only_child_path(self):
        env = {"PATH": "/global/bin"}
        with patch.object(e2e.shutil, "which", return_value="/global/bin/nats-server"), \
                patch.object(e2e, "check_nats", side_effect=[ValueError("wrong version"), None, None]) as check, \
                patch.object(e2e, "download_nats", side_effect=self.download) as download:
            child = e2e.prepare_nats(env)
        download.assert_called_once()
        self.assertEqual(self.cache.read_bytes(), b"pinned broker")
        self.assertEqual(child["PATH"].split(e2e.os.pathsep), [str(self.cache.parent), "/global/bin"])
        self.assertEqual(env, {"PATH": "/global/bin"})
        self.assertEqual(check.call_args.kwargs["env"], child)

    def test_matching_path_binary_is_copied_without_download(self):
        source = self.root / "installed-nats"
        source.write_bytes(b"installed")
        with patch.object(e2e.shutil, "which", return_value=str(source)), \
                patch.object(e2e, "check_nats"), \
                patch.object(e2e, "download_nats") as download:
            e2e.prepare_nats({"PATH": "/global/bin"})
        download.assert_not_called()
        self.assertEqual(self.cache.read_bytes(), b"installed")

    def test_valid_cached_version_needs_no_network_or_global_lookup(self):
        self.cache.parent.mkdir(parents=True)
        self.cache.write_bytes(b"cached")
        with patch.object(e2e, "check_nats"), \
                patch.object(e2e, "download_nats") as download, \
                patch.object(e2e.shutil, "which") as which:
            child = e2e.prepare_nats({"PATH": "/global/bin"})
        download.assert_not_called()
        which.assert_not_called()
        self.assertTrue(child["PATH"].startswith(str(self.cache.parent)))

    def test_explicit_offline_source_is_validated_and_copied(self):
        source = self.root / "offline-nats"
        source.write_bytes(b"offline")
        with patch.object(e2e, "check_nats") as check, \
                patch.object(e2e, "download_nats") as download:
            e2e.prepare_nats({"PATH": "", "KANADE_TEST_NATS_SERVER": str(source)})
        check.assert_any_call(source.resolve(), "2.14.1", {"PATH": "", "KANADE_TEST_NATS_SERVER": str(source)})
        download.assert_not_called()
        self.assertEqual(self.cache.read_bytes(), b"offline")

    def test_wrong_explicit_source_or_cache_fails_without_fallback(self):
        self.cache.parent.mkdir(parents=True)
        self.cache.write_bytes(b"wrong")
        for extra in ({}, {"KANADE_TEST_NATS_SERVER": str(self.cache)}):
            with self.subTest(extra=extra), \
                    patch.object(e2e, "check_nats", side_effect=ValueError("wrong version")), \
                    patch.object(e2e, "download_nats") as download:
                with self.assertRaises(ValueError):
                    e2e.prepare_nats({"PATH": "", **extra})
                download.assert_not_called()

    def test_failed_download_or_wrong_download_version_never_populates_cache(self):
        for failure in (OSError("offline"), ValueError("wrong version")):
            with self.subTest(failure=failure), \
                    patch.object(e2e.shutil, "which", return_value=None), \
                    patch.object(e2e, "check_nats", side_effect=failure), \
                    patch.object(e2e, "download_nats", side_effect=(
                        failure if isinstance(failure, OSError) else self.download)):
                with self.assertRaises(type(failure)):
                    e2e.prepare_nats({"PATH": ""})
                self.assertFalse(self.cache.exists())

    def test_tar_download_extracts_only_the_expected_binary(self):
        import io
        import tarfile
        payload = io.BytesIO()
        with tarfile.open(fileobj=payload, mode="w:gz") as package:
            for name in ("../../unwanted", "nats-server-v2.14.1-linux-amd64/nats-server"):
                entry = tarfile.TarInfo(name)
                entry.size = 6
                package.addfile(entry, io.BytesIO(b"binary"))
        payload.seek(0)
        destination = self.root / "nats-server"
        with patch.object(e2e.urllib.request, "urlopen", return_value=payload) as download:
            e2e.download_nats("2.14.1", "linux-amd64", destination)
        self.assertEqual(destination.read_bytes(), b"binary")
        self.assertEqual(set(self.root.iterdir()), {self.root / "nats.tar.gz", destination})
        self.assertEqual(download.call_args.args[0],
                         "https://github.com/nats-io/nats-server/releases/download/v2.14.1/"
                         "nats-server-v2.14.1-linux-amd64.tar.gz")

    def test_windows_download_uses_zip_and_expected_executable(self):
        import io
        import zipfile
        payload = io.BytesIO()
        with zipfile.ZipFile(payload, "w") as package:
            package.writestr("nats-server-v2.14.1-windows-amd64/nats-server.exe", b"binary")
        payload.seek(0)
        destination = self.root / "nats-server.exe"
        with patch.object(e2e.urllib.request, "urlopen", return_value=payload):
            e2e.download_nats("2.14.1", "windows-amd64", destination)
        self.assertEqual(destination.read_bytes(), b"binary")


if __name__ == "__main__":
    unittest.main()
