#!/usr/bin/env python3
"""Regression tests for client_macos_pkg.py (run: python3 -I scripts/ci/test_client_macos_pkg.py).

Pure parsing tests run everywhere. Tests that build a synthetic arm64 app and a
real .pkg need an Apple Silicon Mac with Xcode tools; elsewhere they are reported
as skipped, and a skip is not a pass. Nothing is installed or launched.
"""
import os
import io
import stat
import struct
import zlib
import xml.etree.ElementTree as ET
import pathlib
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import client_macos_pkg as m  # noqa: E402

NATIVE = sys.platform == "darwin" and os.uname().machine == "arm64" and shutil.which("pkgbuild") and shutil.which("cc")
needs_mac = unittest.skipUnless(NATIVE, "requires Apple Silicon macOS with pkgbuild and cc")
VERSION = m.expected_version()


def sh(*cmd):
    subprocess.run([str(c) for c in cmd], check=True, capture_output=True)


def make_app(base, *, target="14.0", version=VERSION, bundle_id=m.BUNDLE_ID, arch="arm64", sign=True):
    app = base / m.APP_NAME
    (app / "Contents/MacOS").mkdir(parents=True)
    (app / "Contents/Resources").mkdir()
    src = base / "main.c"
    src.write_text("int main(void){return 0;}\n")
    sh("cc", "-arch", arch, f"-mmacosx-version-min={target}", src, "-o", app / "Contents/MacOS" / m.EXECUTABLE)
    (app / "Contents/Resources/icon.icns").write_bytes(b"icns")
    (app / "Contents/Resources/THIRD-PARTY-NOTICES.md").write_text("notices\n")
    with (app / "Contents/Info.plist").open("wb") as f:
        plistlib.dump(dict(CFBundleName=m.PRODUCT_NAME, CFBundleDisplayName=m.PRODUCT_NAME,
                           CFBundleIdentifier=bundle_id, CFBundleExecutable=m.EXECUTABLE,
                           CFBundleVersion=version, CFBundleShortVersionString=version,
                           CFBundlePackageType="APPL", CFBundleIconFile="icon.icns",
                           LSMinimumSystemVersion=m.MIN_OS), f)
    if sign:
        sh("codesign", "--force", "--sign", "-", app)
    return app


class Pure(unittest.TestCase):
    def test_artifact_key_matches_shared_identity(self):
        self.assertEqual(m.artifact_key("0.96.0"), "client/0.96.0/macos-aarch64-darwin14.pkg")
        for bad in ("1.2", "v1.2.3", "1.2.3-rc.1", "1.2.3+b", "01.2.3", ""):
            with self.assertRaises(m.PkgError):
                m.artifact_key(bad)

    def test_workspace_tauri_versions_are_synchronized(self):
        self.assertRegex(VERSION, r"^\d+\.\d+\.\d+$")

    def test_rejects_dangerous_paths(self):
        for bad in ("/abs", "../x", "a/../b", "a//b", "a/./b", "a\nb", "a/._b", "", "a/"):
            with self.assertRaises(m.PkgError, msg=bad):
                m.check_rel_path(bad)
        m.check_rel_path("Contents/MacOS/kanade-client")

    def _bom(self, **over):
        expected = {rel: (True, False) for rel in m.APP_DIRS}
        expected.update({rel: (False, rel == f"Contents/MacOS/{m.EXECUTABLE}") for rel in m.APP_FILES})
        rows = [(".", 0o40755, "root", "wheel"), (f"./{m.APP_NAME}", 0o40755, "root", "wheel")]
        for rel, (d, mac) in expected.items():
            rows.append((f"./{m.APP_NAME}/{rel}", (0o40000 if d else 0o100000) | m.expected_mode(rel, d, mac), "root", "wheel"))
        return rows, expected

    def test_bom_accepts_exact_tree_and_rejects_deviations(self):
        rows, expected = self._bom()
        m.check_bom(rows, expected)
        cases = {
            "owner": [(p, md, "501", g) if i == 3 else (p, md, u, g) for i, (p, md, u, g) in enumerate(rows)],
            "mode": [(p, 0o100777, u, g) if i == 3 else (p, md, u, g) for i, (p, md, u, g) in enumerate(rows)],
            "symlink": rows + [(f"./{m.APP_NAME}/Contents/link", 0o120755, "root", "wheel")],
            "traversal": rows + [("./../etc/passwd", 0o100644, "root", "wheel")],
            "absolute": rows + [("/etc/passwd", 0o100644, "root", "wheel")],
            "duplicate": rows + [rows[2]],
            "missing": rows[:-1],
            "extra-script": rows + [("./Scripts/postinstall", 0o100755, "root", "wheel")],
        }
        for name, bad in cases.items():
            with self.assertRaises(m.PkgError, msg=name):
                m.check_bom(bad, expected)

    def test_bom_parser_rejects_malformed_rows(self):
        with self.assertRaises(m.PkgError):
            m.parse_bom("./a\t100644\troot")
        with self.assertRaises(m.PkgError):
            m.parse_bom("./a\tb\t100644\troot\twheel")

    def test_distribution_text_is_arm64_macos14_system_only(self):
        with tempfile.TemporaryDirectory() as t:
            path = pathlib.Path(t) / "Distribution"
            path.write_text(m.distribution_xml(VERSION, 10))
            self.assertEqual(m.check_distribution(path, VERSION), m.COMPONENT_FILE)
            with self.assertRaises(m.PkgError):
                m.check_distribution(path, "9.9.9")
            for old, new in (('hostArchitectures="arm64"', 'hostArchitectures="x86_64"'),
                             ('min="14.0"', 'min="13.0"'),
                             ('enable_currentUserHome="false"', 'enable_currentUserHome="true"'),
                             ("</installer-gui-script>", "<installation-check script='x()'/></installer-gui-script>")):
                path.write_text(m.distribution_xml(VERSION, 10).replace(old, new))
                with self.assertRaises(m.PkgError, msg=new):
                    m.check_distribution(path, VERSION)

    def test_component_plist_is_not_relocatable(self):
        (item,) = plistlib.loads(m.component_plist())
        self.assertFalse(item["BundleIsRelocatable"])
        self.assertTrue(item["BundleHasStrictIdentifier"])
        self.assertEqual(item["RootRelativeBundlePath"], m.APP_NAME)


class ArchiveParsing(unittest.TestCase):
    def setUp(self):
        self.rows, self.expected = Pure()._bom()

    def cpio(self, rows=None, overrides=None):
        result = bytearray()
        for name, mode, _, _ in rows or self.rows:
            name = name.encode() + b"\0"
            fields = [1, len(result), mode, 0, 0, 1, 0, 0, len(name), 0 if stat.S_ISDIR(mode) else 1]
            if overrides:
                overrides(name, fields)
            result += b"070707" + b"".join(f"{v:0{w}o}".encode() for v, w in zip(fields, (6, 6, 6, 6, 6, 6, 6, 11, 6, 11)))
            result += name
            result += b"x" * min(fields[-1], 1)
        result += b"070707" + b"000000" * 7 + b"00000000000" + b"000013" + b"00000000000" + b"TRAILER!!!\0"
        return bytes(result)

    def test_cpio_roundtrip(self):
        entries = m.payload_entries(io.BytesIO(self.cpio()), self.expected)
        with tempfile.TemporaryDirectory() as t:
            root = pathlib.Path(t) / "restored"
            m.restore_payload(entries, root)
            self.assertEqual((root / m.APP_NAME / "Contents/Resources/icon.icns").read_bytes(), b"x")

    def test_cpio_rejects_paths_types_metadata_and_limits(self):
        for index, value in ((2, 0o120644), (2, 0o020644), (2, 0o100777),
                             (3, 501), (4, 20), (5, 2), (8, 5000), (9, m.MAX_PAYLOAD_BYTES + 1)):
            def change(name, fields):
                if name.endswith(b"Info.plist\0"):
                    fields[index] = value
            with self.subTest(index=index, value=value), self.assertRaises(m.PkgError):
                m.payload_entries(io.BytesIO(self.cpio(overrides=change)), self.expected)
        for rows in (self.rows + [("./../escape", 0o100644, "root", "wheel")],
                     self.rows + [("/absolute", 0o100644, "root", "wheel")],
                     self.rows + [self.rows[-1]], self.rows[1:],
                     self.rows + [("./._orphan", 0o100644, "root", "wheel")]):
            with self.assertRaises(m.PkgError):
                m.payload_entries(io.BytesIO(self.cpio(rows)), self.expected)
        for data in (self.cpio()[:-1], self.cpio() + b"evil", self.cpio().replace(b"070707", b"070708", 1)):
            with self.assertRaises(m.PkgError):
                m.payload_entries(io.BytesIO(data), self.expected)

    def test_cpio_requires_directory_parents_before_restoration(self):
        rows = [(".", 0o40755, "root", "wheel"), ("./file/child", 0o100644, "root", "wheel")]
        # Isolate parent validation from the stricter expected-bundle check.
        from unittest.mock import patch
        with patch.object(m, "check_bom"), self.assertRaisesRegex(m.PkgError, "directory parent"):
            m.payload_entries(io.BytesIO(self.cpio(rows)), self.expected)

    def test_cpio_aggregate_limits_and_appledouble_metadata(self):
        from unittest.mock import patch
        for key, value in (("MAX_PAYLOAD_BYTES", 2), ("MAX_FILES", 1)):
            with patch.object(m, key, value), self.assertRaisesRegex(m.PkgError, "expansion limit"):
                m.payload_entries(io.BytesIO(self.cpio()), self.expected)
        rows = self.rows + [(f"./{m.APP_NAME}/Contents/._Info.plist", 0o100644, "root", "wheel")]
        entries = m.payload_entries(io.BytesIO(self.cpio(rows)), self.expected)
        self.assertNotIn(f"{m.APP_NAME}/Contents/._Info.plist", entries)
        def change(name, fields):
            if name.endswith(b"._Info.plist\0"):
                fields[5] = 2
        with self.assertRaisesRegex(m.PkgError, "AppleDouble link count"):
            m.payload_entries(io.BytesIO(self.cpio(rows, change)), self.expected)

    def xar(self, mutate=lambda toc: None):
        root = ET.Element("xar")
        toc = ET.SubElement(root, "toc")
        payload = bytearray()
        def member(parent, name, directory=False):
            node = ET.SubElement(parent, "file")
            ET.SubElement(node, "name").text = name
            ET.SubElement(node, "type").text = "directory" if directory else "file"
            if not directory:
                data = ET.SubElement(node, "data")
                for key, value in (("length", 1), ("size", 1), ("offset", len(payload))):
                    ET.SubElement(data, key).text = str(value)
                ET.SubElement(data, "encoding", style="application/octet-stream")
                payload.extend(b"x")
            return node
        member(toc, "Distribution")
        component = member(toc, m.COMPONENT_FILE, True)
        for name in ("Bom", "Payload", "PackageInfo"):
            member(component, name)
        mutate(toc)
        xml = ET.tostring(root)
        compressed = zlib.compress(xml)
        return struct.pack(">4sHHQQI", b"xar!", 28, 1, len(compressed), len(xml), 0) + compressed + payload

    def test_xar_roundtrip_and_rejects_before_writing(self):
        with tempfile.TemporaryDirectory() as t:
            pkg = pathlib.Path(t) / "test.pkg"
            pkg.write_bytes(self.xar())
            dest = pathlib.Path(t) / "good"
            m.inspect_xar(pkg, dest)
            self.assertEqual((dest / m.COMPONENT_FILE / "Payload").read_bytes(), b"x")
            mutations = [
                lambda toc: setattr(toc.find("file/name"), "text", "../escape"),
                lambda toc: setattr(toc.find("file/type"), "text", "symlink"),
                lambda toc: toc.find("file/type").set("link", "1"),
                lambda toc: ET.SubElement(toc.find("file"), "link"),
                lambda toc: setattr(toc.find("file/data/size"), "text", str(m.MAX_PAYLOAD_BYTES + 1)),
                lambda toc: setattr(toc.find("file/data/offset"), "text", "999999"),
                lambda toc: setattr(toc.find("file/data/size"), "text", "-1"),
                lambda toc: setattr(ET.SubElement(toc.find("file"), "name"), "text", "elsewhere"),
                lambda toc: toc.append(toc.find("file")),
                lambda toc: toc.remove(toc.find("file")),
            ]
            for mutate in mutations:
                pkg.write_bytes(self.xar(mutate))
                dest = pathlib.Path(t) / "bad"
                with self.assertRaises(m.PkgError):
                    m.inspect_xar(pkg, dest)
                self.assertFalse(dest.exists())

    def test_bounded_zlib_rejects_bombs_truncation_and_trailing_data(self):
        compressed = zlib.compress(b"a" * 10000)
        self.assertEqual(m.bounded_zlib(compressed, 10000), b"a" * 10000)
        for data, size in ((compressed, 10), (compressed[:-1], 10000), (compressed + b"extra", 10000)):
            with self.assertRaises(m.PkgError):
                m.bounded_zlib(data, size)

    def test_analyzed_components_must_be_exactly_one_expected_bundle(self):
        for bad in ([], {}, [{"RootRelativeBundlePath": "Other.app"}],
                    [{"RootRelativeBundlePath": m.APP_NAME}] * 2):
            with self.assertRaises(m.PkgError):
                m.component_plist(bad)
        result = plistlib.loads(m.component_plist([dict(RootRelativeBundlePath=m.APP_NAME, Extra="retained")]))
        self.assertEqual(result[0]["Extra"], "retained")
        self.assertFalse(result[0]["BundleIsVersionChecked"])


@needs_mac
class Native(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = pathlib.Path(tempfile.mkdtemp(prefix="kanade-pkgtest-"))
        cls.app = make_app(cls.tmp)
        cls.out = cls.tmp / "out"
        cls.pkg, cls.record = m.build(cls.app, cls.out, production=False)

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def fresh(self, **kw):
        d = pathlib.Path(tempfile.mkdtemp(dir=self.tmp))
        return make_app(d, **kw)

    def test_good_app_and_pkg_validate(self):
        m.validate_app(self.app)
        self.assertEqual(m.validate_pkg(self.pkg, self.app), VERSION)
        self.assertEqual(self.pkg.relative_to(self.out).as_posix(), m.artifact_key(VERSION))
        self.assertFalse(self.record["gatekeeper_trusted"])
        self.assertEqual(self.record["installer_signature"], "none")
        self.assertEqual(len(self.record["sha256"]), 64)

    def test_pkg_has_no_scripts_and_root_wheel_payload(self):
        members = subprocess.run(["xar", "-tf", self.pkg], capture_output=True, text=True).stdout.split()
        self.assertFalse([x for x in members if "Scripts" in x])
        with tempfile.TemporaryDirectory() as t:
            dest = pathlib.Path(t) / "x"
            sh("pkgutil", "--expand", self.pkg, dest)
            bom = subprocess.run(["lsbom", "-p", "fmUG", dest / m.COMPONENT_FILE / "Bom"], capture_output=True, text=True).stdout
        self.assertTrue(all(row.endswith("root\twheel") for row in bom.splitlines()))

    def test_pkgutil_metadata(self):
        out = subprocess.run(["pkgutil", "--payload-files", self.pkg], capture_output=True, text=True).stdout
        self.assertIn(f"./{m.APP_NAME}/Contents/MacOS/{m.EXECUTABLE}", out.splitlines())

    def test_version_and_identifier_mismatch(self):
        with self.assertRaises(m.PkgError):
            m.validate_app(self.fresh(version="0.0.1"))
        with self.assertRaises(m.PkgError):
            m.validate_app(self.fresh(bundle_id="com.example.other"))

    def test_wrong_architecture_and_deployment_target(self):
        with self.assertRaises(m.PkgError):
            m.validate_app(self.fresh(arch="x86_64"))
        with self.assertRaises(m.PkgError):
            m.validate_app(self.fresh(target="15.0"))

    def test_signature_integrity(self):
        with self.assertRaises(m.PkgError):
            m.validate_app(self.fresh(sign=False))
        app = self.fresh()
        with open(app / "Contents/Resources/icon.icns", "ab") as f:
            f.write(b"tamper")
        with self.assertRaises(m.PkgError):
            m.validate_app(app)

    def test_missing_resources(self):
        for rel in ("Contents/Resources/icon.icns", "Contents/Resources/THIRD-PARTY-NOTICES.md"):
            app = self.fresh()
            (app / rel).unlink()
            with self.assertRaises(m.PkgError, msg=rel):
                m.validate_app(app)

    def test_symlink_and_extra_files_rejected(self):
        app = self.fresh()
        os.symlink("/etc/passwd", app / "Contents/Resources/link")
        with self.assertRaises(m.PkgError):
            m.walk_app(app)
        app = self.fresh()
        (app / "Contents/Resources/run.sh").write_text("#!/bin/sh\n")
        with self.assertRaises(m.PkgError):
            m.walk_app(app)
        app = self.fresh()
        os.symlink(app / "Contents/MacOS", app / "Contents/Alias")
        with self.assertRaises(m.PkgError):
            m.walk_app(app)

    def test_pkg_validated_against_a_different_app_fails(self):
        other = self.fresh()
        (other / "Contents/Resources/icon.icns").write_bytes(b"different-icon")
        sh("codesign", "--force", "--sign", "-", other)
        with self.assertRaises(m.PkgError):
            m.validate_pkg(self.pkg, other)

    def test_pkg_with_scripts_is_rejected(self):
        with tempfile.TemporaryDirectory(dir=self.tmp) as t:
            t = pathlib.Path(t)
            scripts = t / "scripts"
            scripts.mkdir()
            (scripts / "postinstall").write_text("#!/bin/sh\nexit 0\n")
            os.chmod(scripts / "postinstall", 0o755)
            payload = t / "payload"
            payload.mkdir()
            m.stage(self.app, payload)
            (t / "c.plist").write_bytes(m.component_plist())
            parts = t / "parts"
            parts.mkdir()
            sh("pkgbuild", "--root", payload, "--component-plist", t / "c.plist", "--identifier", m.COMPONENT_ID,
               "--version", VERSION, "--install-location", m.INSTALL_LOCATION, "--ownership", "recommended",
               "--scripts", scripts, parts / m.COMPONENT_FILE)
            (t / "Distribution").write_text(m.distribution_xml(VERSION, 10))
            bad = t / "out" / m.artifact_key(VERSION)
            bad.parent.mkdir(parents=True)
            sh("productbuild", "--distribution", t / "Distribution", "--package-path", parts, bad)
            with self.assertRaises(m.PkgError):
                m.validate_pkg(bad, self.app)

    def test_pkg_with_symlink_payload_is_rejected(self):
        with tempfile.TemporaryDirectory(dir=self.tmp) as t:
            t = pathlib.Path(t)
            payload = t / "payload"
            payload.mkdir()
            staged = m.stage(self.app, payload)
            os.symlink("/etc", staged / "Contents/Resources/escape")
            (t / "c.plist").write_bytes(m.component_plist())
            parts = t / "parts"
            parts.mkdir()
            sh("pkgbuild", "--root", payload, "--component-plist", t / "c.plist", "--identifier", m.COMPONENT_ID,
               "--version", VERSION, "--install-location", m.INSTALL_LOCATION, "--ownership", "recommended",
               parts / m.COMPONENT_FILE)
            (t / "Distribution").write_text(m.distribution_xml(VERSION, 10))
            bad = t / "out" / m.artifact_key(VERSION)
            bad.parent.mkdir(parents=True)
            sh("productbuild", "--distribution", t / "Distribution", "--package-path", parts, bad)
            with self.assertRaises(m.PkgError):
                m.validate_pkg(bad, self.app)

    def test_wrong_artifact_name_rejected(self):
        wrong = self.tmp / "client" / VERSION / "other.pkg"
        wrong.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(self.pkg, wrong)
        with self.assertRaises(m.PkgError):
            m.validate_pkg(wrong, self.app)


if __name__ == "__main__":
    unittest.main(verbosity=2)
