#!/bin/sh
# Local managed/test bundle only; never writes /Applications or touches the agent.
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -sm)" = "Darwin arm64" ] || { echo 'Requires an Apple Silicon Mac' >&2; exit 1; }
export MACOSX_DEPLOYMENT_TARGET=14.0
unset TAURI_CONFIG
(cd crates/kanade-client/web && bun install --frozen-lockfile --ignore-scripts && bun run build)
cargo build --locked --release -p kanade-client --target aarch64-apple-darwin --target-dir target
python3 - <<'PY'
import json, pathlib, plistlib, shutil, tomllib
root = pathlib.Path('.')
base = root / 'crates/kanade-client'
config = json.loads((base / 'tauri.conf.json').read_text())
mac = json.loads((base / 'tauri.macos.conf.json').read_text())
version = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['package']['version']
assert config['version'] == version
assert (base / 'web/dist/assets').is_dir()
app = root / 'target/aarch64-apple-darwin/release/bundle/macos/Kanade Client.app'
if app.exists():
    shutil.rmtree(app)
contents = app / 'Contents'
(contents / 'MacOS').mkdir(parents=True)
(contents / 'Resources').mkdir()
shutil.copy2(root / 'target/aarch64-apple-darwin/release/kanade-client', contents / 'MacOS/kanade-client')
shutil.copy2(base / 'icons/icon.icns', contents / 'Resources/icon.icns')
shutil.copy2(root / 'THIRD-PARTY-NOTICES.md', contents / 'Resources/THIRD-PARTY-NOTICES.md')
with (contents / 'Info.plist').open('wb') as out:
    plistlib.dump(dict(CFBundleName=mac['productName'], CFBundleDisplayName=mac['productName'],
        CFBundleIdentifier=config['identifier'], CFBundleExecutable='kanade-client',
        CFBundleVersion=version, CFBundleShortVersionString=version,
        CFBundlePackageType='APPL', CFBundleIconFile='icon.icns',
        LSMinimumSystemVersion=mac['bundle']['macOS']['minimumSystemVersion'],
        NSHighResolutionCapable=True), out)
print(app)
PY
app='target/aarch64-apple-darwin/release/bundle/macos/Kanade Client.app'
/usr/bin/codesign --force --sign - "$app"
python3 scripts/ci/validate-client-macos.py "$app"
