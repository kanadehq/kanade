#!/usr/bin/env python3
"""Validate the production .app without launching it or requiring a dev server."""
import json
import pathlib
import plistlib
import re
import subprocess
import sys
import tomllib

root = pathlib.Path(__file__).resolve().parents[2]
app = pathlib.Path(sys.argv[1])
config = json.loads((root / "crates/kanade-client/tauri.conf.json").read_text())
version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
with (app / "Contents/Info.plist").open("rb") as file:
    info = plistlib.load(file)
assert info["CFBundleIdentifier"] == config["identifier"]
assert info["CFBundleName"] == "Kanade Client"
assert info["CFBundleVersion"] == info["CFBundleShortVersionString"] == config["version"] == version
assert info["LSMinimumSystemVersion"] == "14.0"
assert "CFBundleURLTypes" not in info, "Deep-link registration belongs to a later stage"
binary = app / "Contents/MacOS" / info["CFBundleExecutable"]
arch = subprocess.check_output(["lipo", "-archs", binary], text=True).strip()
assert arch == "arm64", arch
libraries = subprocess.check_output(["otool", "-L", binary], text=True)
assert "WebKit.framework" in libraries, libraries
commands = subprocess.check_output(["otool", "-l", binary], text=True)
assert "minos 14.0" in commands, commands
# Tauri embeds the SPA bytes in the executable; the stub contains no asset names.
html = (root / "crates/kanade-client/web/dist/index.html").read_text()
assert '/assets/' in html and 'src="/src/main.ts"' not in html
embedded = binary.read_bytes()
assets = re.findall(r'"/(assets/[^" ]+)"', html)
assert assets, "Production asset references missing"
for asset in assets:
    assert asset.encode() in embedded, f"Embedded production asset missing: {asset}"
assert (app / "Contents/Resources/icon.icns").is_file()
assert (app / "Contents/Resources/THIRD-PARTY-NOTICES.md").is_file()
subprocess.run(["codesign", "--verify", "--deep", "--strict", app], check=True)
signature = subprocess.run(["codesign", "-d", "--verbose=4", app], capture_output=True, text=True, check=True).stderr
assert "Signature=adhoc" in signature, signature
print("Validated arm64 production Client.app: identity, versions, macOS 14, WebKit, SPA, ad-hoc signature")
