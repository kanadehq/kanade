#!/usr/bin/env python3
"""Build and validate the payload-only Kanade Client .pkg (Apple Silicon, macOS 14+).

    client_macos_pkg.py build --app <Kanade Client.app> --out <dir>
    client_macos_pkg.py validate --pkg <file.pkg> --app <Kanade Client.app>

The package carries exactly /Applications/Kanade Client.app: no scripts, no
LaunchAgent, no session helper. It is NOT signed (Developer ID Installer and
notarization are deferred); the ad-hoc signature covers the app bundle only.
Every check raises PkgError so callers and tests get a precise failure.
"""
import argparse
import datetime
import gzip
import hashlib
import json
import os
import pathlib
import plistlib
import re
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import tomllib
import xml.etree.ElementTree as ET
import zlib

ROOT = pathlib.Path(__file__).resolve().parents[2]

BUNDLE_ID = "com.yukimemi.kanade-client"
PRODUCT_NAME = "Kanade Client"
APP_NAME = "Kanade Client.app"
EXECUTABLE = "kanade-client"
COMPONENT_ID = BUNDLE_ID + ".pkg"
PRODUCT_ID = BUNDLE_ID + ".product"
COMPONENT_FILE = "kanade-client-component.pkg"
INSTALL_LOCATION = "/Applications"
MIN_OS = "14.0"
# Shared ArtifactIdentity: role=client os=macos arch=aarch64 distro_abi=darwin14 format=pkg.
IDENTITY = dict(role="client", os="macos", arch="aarch64", distro_abi="darwin14", format="pkg")
ARTIFACT_BASENAME = "macos-aarch64-darwin14.pkg"
VERSION_RE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
MAX_FILES = 64
MAX_PAYLOAD_BYTES = 256 * 1024 * 1024
MACHO_MAGIC = {b"\xcf\xfa\xed\xfe", b"\xce\xfa\xed\xfe", b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca"}
APP_FILES = {
    "Contents/Info.plist",
    f"Contents/MacOS/{EXECUTABLE}",
    "Contents/Resources/icon.icns",
    "Contents/Resources/THIRD-PARTY-NOTICES.md",
    "Contents/_CodeSignature/CodeResources",
}
APP_DIRS = {"Contents", "Contents/MacOS", "Contents/Resources", "Contents/_CodeSignature"}


class PkgError(Exception):
    pass


def require(cond, message):
    if not cond:
        raise PkgError(message)


def run(*cmd, check=True):
    done = subprocess.run([str(c) for c in cmd], capture_output=True, text=True)
    if check and done.returncode != 0:
        raise PkgError(f"{cmd[0]} failed ({done.returncode}): {done.stderr.strip() or done.stdout.strip()}")
    return done


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


# --- identity -----------------------------------------------------------------


def expected_version(root=ROOT):
    """Workspace version, which must equal the Tauri version; numeric x.y.z only."""
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    config = json.loads((root / "crates/kanade-client/tauri.conf.json").read_text())
    require(config["identifier"] == BUNDLE_ID, f"tauri identifier drifted: {config['identifier']}")
    require(config["version"] == workspace, f"tauri version {config['version']} != workspace {workspace}")
    require(VERSION_RE.fullmatch(workspace), f"version {workspace!r} is not plain numeric x.y.z (prerelease/build metadata unsupported)")
    return workspace


def artifact_key(version):
    require(VERSION_RE.fullmatch(version), f"unsupported version {version!r}")
    return f"client/{version}/{ARTIFACT_BASENAME}"


# --- path safety --------------------------------------------------------------


def check_rel_path(rel):
    """A payload-relative path must be plain, normalized and free of control characters."""
    require(isinstance(rel, str) and rel, "empty path")
    require(not rel.startswith("/"), f"absolute path: {rel!r}")
    require(not any(ord(c) < 32 or ord(c) == 127 for c in rel), f"control character in path: {rel!r}")
    parts = rel.split("/")
    require(all(p not in ("", ".", "..") for p in parts), f"non-normalized path: {rel!r}")
    require(not any(p.startswith("._") or p == ".DS_Store" for p in parts), f"AppleDouble/Finder metadata: {rel!r}")


# macOS archivers store extended attributes as `._name` AppleDouble companions. A
# build host may stamp com.apple.provenance on every file (it cannot be removed),
# so those companions are tolerated only as xattr carriers: each must sit beside
# the real entry it annotates, and the restored tree must hold no `._` files and
# no xattr other than com.apple.provenance.
ALLOWED_XATTRS = {"com.apple.provenance"}


def split_carriers(paths):
    """Return (real, carriers) for payload-relative paths; orphan `._` entries are rejected."""
    paths = list(paths)
    names = set(paths)
    carriers = [p for p in paths if p.rsplit("/", 1)[-1].startswith("._")]
    for c in carriers:
        head, _, tail = c.rpartition("/")
        target = f"{head}/{tail[2:]}" if head else tail[2:]
        require(len(tail) > 2 and not tail[2:].startswith("._") and target in names, f"orphan AppleDouble entry: {c!r}")
    return [p for p in paths if p not in set(carriers)], carriers


def check_restored_xattrs(tree):
    out = run("xattr", "-r", tree).stdout.splitlines()
    for line in out:
        require(line.rsplit(": ", 1)[-1] in ALLOWED_XATTRS, f"unexpected extended attribute: {line!r}")


def expected_mode(rel, is_dir, is_macho):
    if is_dir:
        return 0o755
    return 0o755 if is_macho and rel.startswith("Contents/MacOS/") else 0o644


# --- app bundle validation ----------------------------------------------------


def _is_macho(path):
    with open(path, "rb") as f:
        return f.read(4) in MACHO_MAGIC


def walk_app(app):
    """Return {rel: (is_dir, is_macho)} after rejecting links, specials and unexpected paths."""
    require(app.name == APP_NAME and app.is_dir() and not app.is_symlink(), f"not a real {APP_NAME} directory: {app}")
    seen = {}
    for path in sorted(app.rglob("*")):
        rel = path.relative_to(app).as_posix()
        check_rel_path(rel)
        st = path.lstat()
        if stat.S_ISDIR(st.st_mode):
            seen[rel] = (True, False)
        elif stat.S_ISREG(st.st_mode):
            require(st.st_nlink == 1, f"hard link: {rel}")
            with open(path, "rb") as f:
                require(f.read(2) != b"#!", f"script payload not allowed: {rel}")
            seen[rel] = (False, _is_macho(path))
        else:
            raise PkgError(f"symlink or special file: {rel}")
    require(set(seen) == APP_FILES | APP_DIRS, f"unexpected bundle contents: {sorted(set(seen) ^ (APP_FILES | APP_DIRS))}")
    require(len(seen) <= MAX_FILES, "too many entries")
    return seen


def macho_facts(binary):
    archs = run("lipo", "-archs", binary).stdout.split()
    require(archs == ["arm64"], f"{binary.name}: architectures {archs}, expected ['arm64']")
    load = run("otool", "-l", binary).stdout
    minos = re.findall(r"^\s+minos (\S+)$", load, re.M) + re.findall(r"LC_VERSION_MIN_MACOSX[\s\S]*?^\s+version (\S+)$", load, re.M)
    require(minos, f"{binary.name}: no macOS deployment target")
    for value in minos:
        require(tuple(int(x) for x in value.split(".")) <= tuple(int(x) for x in MIN_OS.split(".")),
                f"{binary.name}: deployment target {value} is newer than macOS {MIN_OS}")
    require(re.search(r"^\s+platform (1|MACOS)$", load, re.M) or "LC_VERSION_MIN_MACOSX" in load, f"{binary.name}: not a macOS binary")
    libs = [l.split()[0] for l in run("otool", "-L", binary).stdout.splitlines()[1:] if l.strip()]
    for lib in libs:
        require(lib.startswith(("/usr/lib/", "/System/Library/")), f"{binary.name}: external dependency {lib}")
    return minos, libs


def validate_app(app, version=None, *, root=ROOT):
    """Check identity, versions, every Mach-O, resources and the ad-hoc signature of an .app."""
    app = pathlib.Path(app)
    version = version or expected_version(root)
    tree = walk_app(app)
    with (app / "Contents/Info.plist").open("rb") as f:
        info = plistlib.load(f)
    require(info.get("CFBundleIdentifier") == BUNDLE_ID, f"bundle id {info.get('CFBundleIdentifier')!r}")
    require(info.get("CFBundleName") == PRODUCT_NAME, "bundle name")
    require(info.get("CFBundlePackageType") == "APPL", "package type")
    require(info.get("CFBundleExecutable") == EXECUTABLE, "executable name")
    require(info.get("CFBundleVersion") == info.get("CFBundleShortVersionString") == version,
            f"bundle versions {info.get('CFBundleVersion')!r}/{info.get('CFBundleShortVersionString')!r} != {version!r}")
    require(info.get("LSMinimumSystemVersion") == MIN_OS, f"LSMinimumSystemVersion {info.get('LSMinimumSystemVersion')!r}")
    require("CFBundleURLTypes" not in info, "URL scheme registration belongs to a later stage")
    machos = [rel for rel, (is_dir, macho) in tree.items() if macho]
    require(machos == [f"Contents/MacOS/{EXECUTABLE}"], f"Mach-O files: {machos}")
    for rel in machos:
        macho_facts(app / rel)
    for rel in APP_FILES - {"Contents/_CodeSignature/CodeResources"}:
        require((app / rel).stat().st_size > 0, f"empty resource: {rel}")
    run("codesign", "--verify", "--deep", "--strict", app)
    detail = run("codesign", "-d", "--verbose=4", app).stderr
    require("Signature=adhoc" in detail, "app is not ad-hoc signed")
    require(f"Identifier={BUNDLE_ID}" in detail, "signature identifier differs from bundle id")
    return info


def validate_production_app(app):
    """Production-only checks (SPA embedded, WebKit): the existing validator."""
    done = subprocess.run([sys.executable, "-I", str(ROOT / "scripts/ci/validate-client-macos.py"), str(app)],
                          capture_output=True, text=True)
    require(done.returncode == 0, f"validate-client-macos.py failed: {(done.stderr or done.stdout).strip()[-400:]}")


# --- staging and build --------------------------------------------------------


def tree_digest(app):
    return {rel: (is_dir, None if is_dir else sha256(app / rel)) for rel, (is_dir, _) in walk_app(app).items()}


def stage(app, dest):
    """Copy the app into dest/Kanade Client.app with normalized modes and no xattrs."""
    tree = walk_app(app)
    out = dest / APP_NAME
    out.mkdir(mode=0o755)
    for rel, (is_dir, macho) in sorted(tree.items()):
        target = out / rel
        if is_dir:
            target.mkdir(mode=0o755, exist_ok=True)
        else:
            shutil.copyfile(app / rel, target)
        os.chmod(target, expected_mode(rel, is_dir, macho))
    os.chmod(out, 0o755)
    run("xattr", "-cr", out)
    return out


def component_plist(analyzed=None):
    # Relocation off so the bundle cannot be installed elsewhere. Version checking
    # is off so an operator re-run of an older package can recover. None of this
    # protects an existing unmanaged bundle: the typed handler must check first.
    values = analyzed if analyzed is not None else [dict(RootRelativeBundlePath=APP_NAME)]
    require(isinstance(values, list) and len(values) == 1 and isinstance(values[0], dict)
            and values[0].get("RootRelativeBundlePath") == APP_NAME, "unexpected analyzed component set")
    values[0].update(BundleHasStrictIdentifier=True, BundleIsRelocatable=False,
                     BundleIsVersionChecked=False, BundleOverwriteAction="upgrade")
    return plistlib.dumps(values)


def distribution_xml(version, installed_kb):
    return f"""<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>{PRODUCT_NAME}</title>
    <options customize="never" require-scripts="false" hostArchitectures="arm64"/>
    <allowed-os-versions>
        <os-version min="{MIN_OS}"/>
    </allowed-os-versions>
    <domains enable_anywhere="false" enable_currentUserHome="false" enable_localSystem="true"/>
    <product id="{PRODUCT_ID}" version="{version}"/>
    <choices-outline>
        <line choice="default"/>
    </choices-outline>
    <choice id="default" title="{PRODUCT_NAME}">
        <pkg-ref id="{COMPONENT_ID}"/>
    </choice>
    <pkg-ref id="{COMPONENT_ID}" version="{version}" installKBytes="{installed_kb}">{COMPONENT_FILE}</pkg-ref>
</installer-gui-script>
"""


def git_commit():
    done = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "HEAD"], capture_output=True, text=True)
    return done.stdout.strip() if done.returncode == 0 else None


def build(app, out_dir, *, production=True):
    """Validate, stage, pkgbuild + productbuild, then re-validate the result."""
    app = pathlib.Path(app)
    out_dir = pathlib.Path(out_dir)
    version = expected_version()
    validate_app(app, version)
    if production:
        validate_production_app(app)
    pkg = out_dir / artifact_key(version)
    if pkg.exists():
        pkg.unlink()
    pkg.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="kanade-pkg-") as tmp:
        tmp = pathlib.Path(tmp)
        payload = tmp / "payload"
        payload.mkdir(mode=0o755)
        staged = stage(app, payload)
        require(tree_digest(staged) == tree_digest(app), "staged bundle differs from input app")
        validate_app(staged, version)
        components = tmp / "component.plist"
        run("pkgbuild", "--analyze", "--root", payload, components)
        components.write_bytes(component_plist(plistlib.loads(components.read_bytes())))
        parts = tmp / "parts"
        parts.mkdir()
        run("pkgbuild", "--root", payload, "--component-plist", tmp / "component.plist",
            "--identifier", COMPONENT_ID, "--version", version, "--install-location", INSTALL_LOCATION,
            "--ownership", "recommended", parts / COMPONENT_FILE)
        kb = sum(p.stat().st_size for p in staged.rglob("*") if p.is_file()) // 1024 + 1
        (tmp / "Distribution").write_text(distribution_xml(version, kb))
        run("productbuild", "--distribution", tmp / "Distribution", "--package-path", parts,
            "--identifier", PRODUCT_ID, "--version", version, pkg)
    validate_pkg(pkg, app)
    record = dict(
        artifact_key=artifact_key(version), identity=dict(IDENTITY, version=version),
        sha256=sha256(pkg), size=pkg.stat().st_size, source_commit=git_commit(),
        bundle_id=BUNDLE_ID, component_id=COMPONENT_ID, product_id=PRODUCT_ID,
        install_location=f"{INSTALL_LOCATION}/{APP_NAME}", app_signature="adhoc",
        installer_signature="none", notarized=False, gatekeeper_trusted=False,
        signed_manifest=False,
        note="Build record only; not a signed package manifest. Reproducible payload, not byte-identical pkg.",
        built_at=datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    )
    (out_dir / "build-record.json").write_text(json.dumps(record, indent=2) + "\n")
    return pkg, record


# --- package validation -------------------------------------------------------


def bounded_zlib(data, size):
    decoder = zlib.decompressobj()
    result = decoder.decompress(data, size + 1)
    require(len(result) == size and decoder.eof and not decoder.unused_data
            and not decoder.unconsumed_tail, "invalid or oversized zlib stream")
    return result


def inspect_xar(pkg, dest):
    """Parse bounded XAR TOC and members before writing; no native extraction."""
    limit = MAX_PAYLOAD_BYTES
    require(pkg.stat().st_size <= limit, "package too large")
    with pkg.open("rb") as source:
        header = source.read(28)
        require(len(header) == 28, "short xar header")
        magic, size, version, compressed, expanded, checksum = struct.unpack(">4sHHQQI", header)
        require(magic == b"xar!" and size == 28 and version == 1 and checksum in (0, 1, 2), "invalid xar header")
        require(0 < compressed <= 1024 * 1024 and 0 < expanded <= 4 * 1024 * 1024, "xar TOC size limit")
        xml = bounded_zlib(source.read(compressed), expanded)
        require(b"<!DOCTYPE" not in xml and b"<!ENTITY" not in xml, "xar XML declarations forbidden")
        toc = ET.fromstring(xml)
        require(toc.tag == "xar" and len(toc.findall("toc")) == 1, "invalid xar TOC")
        require(toc.find(".//signature") is None, "expected unsigned installer")
        allowed = {"Distribution", COMPONENT_FILE, f"{COMPONENT_FILE}/Bom",
                   f"{COMPONENT_FILE}/PackageInfo", f"{COMPONENT_FILE}/Payload"}
        entries = {}
        total = 0
        streams = []
        heap = 28 + compressed

        def data_record(data, name):
            nonlocal total
            require(all(len(data.findall(tag)) == 1 for tag in ("length", "size", "offset", "encoding")),
                    "missing/ambiguous xar data fields")
            length = int(data.findtext("length", "-1"))
            expanded_size = int(data.findtext("size", "-1"))
            offset = int(data.findtext("offset", "-1"))
            require(0 <= length <= limit and 0 <= expanded_size <= limit and offset >= 0
                    and heap + offset + length <= pkg.stat().st_size, "xar data range/size limit")
            total += expanded_size
            require(total <= limit, "xar expansion limit")
            streams.append((name, offset, length, expanded_size, data.find("encoding")))

        def visit(node, parent=""):
            for item in node.findall("file"):
                leaf = item.findtext("name", "")
                # productbuild can emit identical name elements twice.
                require(item.findall("name") and all(n.text == leaf for n in item.findall("name"))
                        and len(item.findall("type")) == 1, "ambiguous xar name/type")
                require("/" not in leaf, "xar name must be one component")
                name = parent + leaf
                check_rel_path(name)
                require(name in allowed and name not in entries, "unexpected/duplicate xar member")
                directory = name == COMPONENT_FILE
                require(item.findtext("type") == ("directory" if directory else "file"), "xar link/special file")
                require(item.find("link") is None and item.find("type").get("link") is None, "xar hardlink")
                entries[name] = directory
                if directory:
                    require(item.find("data") is None, "xar directory has data")
                else:
                    require(not item.findall("file"), "xar file has children")
                    require(len(item.findall("data")) == 1, "missing/ambiguous xar data")
                    data_record(item.find("data"), name)
                attrs = item.findall("ea")
                require(len(attrs) <= 1, "too many xar attributes")
                for attr in attrs:
                    require(len(attr.findall("name")) == 1 and attr.findtext("name") in ALLOWED_XATTRS,
                            "unexpected xar extended attribute")
                    require(0 <= int(attr.findtext("size", "-1")) <= 4096, "xar attribute size limit")
                    data_record(attr, None)
                visit(item, name + "/")

        visit(toc.find("toc"))
        require(set(entries) == allowed, "incomplete xar package")
        # Decode every member before writing anything into the scratch directory.
        decoded = {}
        for name, offset, length, size, encoding in streams:
            source.seek(heap + offset)
            raw = source.read(length)
            style = encoding.get("style")
            if style == "application/x-gzip":
                value = bounded_zlib(raw, size)
            else:
                require(style == "application/octet-stream" and len(raw) == size, "unsupported xar encoding/size")
                value = raw
            if name is not None:
                decoded[name] = value
    dest.mkdir()
    (dest / COMPONENT_FILE).mkdir()
    for name, data in decoded.items():
        (dest / name).write_bytes(data)


def payload_entries(stream, expected):
    """Parse odc cpio with bounded reads; validate all paths and metadata first."""
    entries = {}
    total = 0
    link_counts = {}
    while True:
        header = stream.read(76)
        require(len(header) == 76 and header[:6] == b"070707", "expected odc cpio")
        fields, offset = [], 6
        for width in (6, 6, 6, 6, 6, 6, 6, 11, 6, 11):
            field = header[offset:offset + width]
            require(all(c in b"01234567" for c in field), "invalid cpio octal field")
            fields.append(int(field, 8))
            offset += width
        dev, ino, mode, uid, gid, links, rdev, mtime, namesize, size = fields
        require(0 < namesize <= 4096 and size <= MAX_PAYLOAD_BYTES, "cpio entry size limit")
        raw = stream.read(namesize)
        require(len(raw) == namesize and raw[-1:] == b"\0" and b"\0" not in raw[:-1], "invalid cpio name")
        name = raw[:-1].decode("utf-8")
        if name == "TRAILER!!!":
            require(size == 0, "invalid cpio trailer")
            tail = stream.read(1024 * 1024)
            require(not tail.strip(b"\0") and not stream.read(1), "unexpected cpio tail")
            break
        require(name == "." or name.startswith("./"), "cpio path not relative")
        name = "" if name == "." else name[2:]
        # Validate AppleDouble targets with the same path rules as normal files.
        head, _, leaf = name.rpartition("/")
        carrier = leaf.startswith("._")
        clean = f"{head}/{leaf[2:]}" if head else leaf[2:]
        if name:
            check_rel_path(clean if carrier else name)
        require(name not in entries, "duplicate cpio path")
        directory = stat.S_ISDIR(mode)
        require(directory or stat.S_ISREG(mode), "cpio link/special file")
        require(directory or carrier or links == 1, "cpio hardlink")
        require(uid == gid == 0, "cpio owner must be root:wheel")
        executable = name == f"{APP_NAME}/Contents/MacOS/{EXECUTABLE}"
        require(stat.S_IMODE(mode) == (0o755 if directory or executable else 0o644), "unexpected cpio mode")
        require(not directory or size == 0, "cpio directory has data")
        require(not carrier or not directory, "AppleDouble carrier must be a file")
        total += size
        require(total <= MAX_PAYLOAD_BYTES and len(entries) < MAX_FILES + 2, "cpio expansion limit")
        data = stream.read(size)
        require(len(data) == size, "truncated cpio data")
        entries[name] = (mode, data)
        link_counts[name] = links
    real, carriers = split_carriers(entries)
    for name in carriers:
        head, _, leaf = name.rpartition("/")
        target = f"{head}/{leaf[2:]}" if head else leaf[2:]
        # Apple's packer copies a directory's nlink onto its AppleDouble carrier.
        require(link_counts[name] == link_counts[target], "AppleDouble link count differs from target")
    rows = [("." if not name else "./" + name, mode, "root", "wheel")
            for name, (mode, _) in entries.items()]
    check_bom(rows, expected)
    for name in entries:
        if name:
            parent = name.rpartition("/")[0]
            require(parent in entries and stat.S_ISDIR(entries[parent][0]), "missing cpio directory parent")
    return {name: entries[name] for name in real}


def restore_payload(entries, dest):
    """Restore only validated regular files/directories, excluding xattr carriers."""
    dest.mkdir()
    for name in sorted(entries, key=lambda p: (p.count("/"), p)):
        mode, data = entries[name]
        path = dest / name
        if stat.S_ISDIR(mode):
            path.mkdir(exist_ok=True)
        else:
            with path.open("xb") as target:
                target.write(data)
        path.chmod(stat.S_IMODE(mode))


def parse_bom(text):
    """Parse `lsbom -p fmUG` lines (path, mode, user, group); malformed rows are rejected."""
    entries = []
    for line in text.splitlines():
        fields = line.split("\t")
        require(len(fields) == 4, f"malformed BOM row: {line!r}")
        entries.append((fields[0], int(fields[1], 8), fields[2], fields[3]))
    return entries


def check_bom(entries, expected):
    """expected: {rel: (is_dir, is_macho)} of the app. Everything else is rejected."""
    seen = {}
    for path, _, user, group in entries:
        require((user, group) == ("root", "wheel"), f"{path}: owner {user}:{group}, expected root:wheel")
    names = [e[0][2:] for e in entries if e[0].startswith("./")]
    _, carriers = split_carriers(names)
    entries = [e for e in entries if e[0][2:] not in carriers]
    for path, mode, user, group in entries:
        if path == ".":
            rel = ""
        else:
            require(path.startswith("./"), f"BOM path not relative: {path!r}")
            rel = path[2:]
            check_rel_path(rel)
        require(rel not in seen, f"duplicate BOM path: {path!r}")
        require((user, group) == ("root", "wheel"), f"{path}: owner {user}:{group}, expected root:wheel")
        seen[rel] = mode
    root_app = APP_NAME
    want = {"": 0o40755, root_app: 0o40755}
    for rel, (is_dir, macho) in expected.items():
        full = f"{root_app}/{rel}"
        want[full] = (0o40000 if is_dir else 0o100000) | expected_mode(rel, is_dir, macho)
    require(set(seen) == set(want), f"BOM paths differ from app: {sorted(set(seen) ^ set(want))}")
    for rel, mode in want.items():
        require(seen[rel] == mode, f"{rel or '.'}: mode {mode_str(seen[rel])}, expected {mode_str(mode)}")


def mode_str(mode):
    return oct(mode)


def check_distribution(path, version):
    root = ET.parse(path).getroot()
    require(root.tag == "installer-gui-script", "Distribution root")
    options = root.find("options")
    require(options is not None and options.get("hostArchitectures") == "arm64", "Distribution must require arm64")
    require(options.get("require-scripts") == "false", "Distribution must not require scripts")
    os_versions = root.findall("./allowed-os-versions/os-version")
    require([o.get("min") for o in os_versions] == [MIN_OS], f"allowed OS versions {[o.attrib for o in os_versions]}")
    domains = root.find("domains")
    require(domains is not None and domains.get("enable_localSystem") == "true"
            and domains.get("enable_anywhere") == "false" and domains.get("enable_currentUserHome") == "false",
            "Distribution domains must be local system only")
    product = root.find("product")
    require(product is not None and (product.get("id"), product.get("version")) == (PRODUCT_ID, version), "product identity")
    refs = [r for r in root.iter("pkg-ref") if (r.text or "").strip()]
    require(len(refs) == 1 and (refs[0].get("id"), refs[0].get("version")) == (COMPONENT_ID, version), "pkg-ref identity")
    require({r.get("id") for r in root.iter("pkg-ref")} == {COMPONENT_ID}, "unexpected pkg-ref ids")
    for bundle in root.iter("bundle"):
        require((bundle.get("id"), bundle.get("path"), bundle.get("CFBundleShortVersionString"), bundle.get("CFBundleVersion"))
                == (BUNDLE_ID, APP_NAME, version, version), f"Distribution bundle record {bundle.attrib}")
    for tag in ("installation-check", "volume-check", "script", "scripts"):
        require(root.find(f".//{tag}") is None, f"Distribution contains <{tag}>")
    return refs[0].text.strip().lstrip("#")


def check_package_info(path, version, expected):
    root = ET.parse(path).getroot()
    require(root.tag == "pkg-info", "PackageInfo root")
    require(root.get("identifier") == COMPONENT_ID, f"component identifier {root.get('identifier')!r}")
    require(root.get("version") == version, f"component version {root.get('version')!r}")
    require(root.get("install-location") == INSTALL_LOCATION, f"install-location {root.get('install-location')!r}")
    require(root.get("relocatable") == "false", "package must not be relocatable")
    require(root.get("auth") == "root", "package auth must be root")
    for tag in ("scripts", "preinstall", "postinstall", "preflight", "postflight"):
        require(root.find(tag) is None, f"PackageInfo declares <{tag}>")
    require(root.get("postinstall-action") in (None, "none"), "postinstall-action must be none")
    bundles = root.findall("bundle")
    require(len(bundles) == 1, f"expected one bundle, found {len(bundles)}")
    b = bundles[0]
    require((b.get("path"), b.get("id"), b.get("CFBundleShortVersionString"), b.get("CFBundleVersion"))
            == (f"./{APP_NAME}", BUNDLE_ID, version, version), f"bundle record {b.attrib}")
    payload = root.find("payload")
    require(payload is not None and int(payload.get("numberOfFiles")) <= MAX_FILES
            and int(payload.get("installKBytes")) * 1024 <= MAX_PAYLOAD_BYTES, "payload size/count out of bounds")


def validate_pkg(pkg, app, *, root=ROOT):
    """Inspect XAR/cpio before restoring, then compare metadata and both restored bundles."""
    pkg = pathlib.Path(pkg)
    app = pathlib.Path(app)
    version = expected_version(root)
    require(pkg.is_file() and not pkg.is_symlink() and pkg.name == ARTIFACT_BASENAME, f"artifact name must be {ARTIFACT_BASENAME}")
    require(pkg.parent.as_posix().endswith(f"client/{version}"), f"artifact directory must end in client/{version}")
    expected = walk_app(app)
    with tempfile.TemporaryDirectory(prefix="kanade-pkgcheck-") as tmp:
        dest = pathlib.Path(tmp) / "x"
        inspect_xar(pkg, dest)
        comp = dest / COMPONENT_FILE
        with gzip.open(comp / "Payload", "rb") as stream:
            entries = payload_entries(stream, expected)
        # All cpio entries are checked before either manual or native restoration.
        manual = pathlib.Path(tmp) / "manual"
        restore_payload(entries, manual)
        validate_app(manual / APP_NAME, version, root=root)
        require(tree_digest(manual / APP_NAME) == tree_digest(app), "parsed payload differs from input app")
        require(not (comp / "Scripts").exists() and not (dest / "Resources").exists(), "installer scripts/resources present")
        require(sorted(p.name for p in dest.iterdir()) == sorted(["Distribution", COMPONENT_FILE]), "expanded layout")
        require(sorted(p.name for p in comp.iterdir()) == ["Bom", "PackageInfo", "Payload"], "component layout")
        require(check_distribution(dest / "Distribution", version) == COMPONENT_FILE, "pkg-ref file name")
        check_package_info(comp / "PackageInfo", version, expected)
        check_bom(parse_bom(run("lsbom", "-p", "fmUG", comp / "Bom").stdout), expected)
        # 3. restore the payload into a fresh private directory and compare with the input app
        full = pathlib.Path(tmp) / "full"
        run("pkgutil", "--expand-full", pkg, full)
        restored = full / COMPONENT_FILE / "Payload"
        require(sorted(p.name for p in restored.iterdir()) == [APP_NAME], "payload root must contain only the app")
        check_restored_xattrs(restored / APP_NAME)
        validate_app(restored / APP_NAME, version, root=root)
        require(tree_digest(restored / APP_NAME) == tree_digest(app), "restored payload differs from input app")
    return version


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("--app", required=True)
    b.add_argument("--out", required=True)
    v = sub.add_parser("validate")
    v.add_argument("--pkg", required=True)
    v.add_argument("--app", required=True)
    args = ap.parse_args(argv)
    try:
        if args.cmd == "build":
            pkg, record = build(args.app, args.out)
            print(f"{pkg}\nsha256={record['sha256']} size={record['size']} key={record['artifact_key']}")
        else:
            print("Validated", validate_pkg(args.pkg, args.app))
    except PkgError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
