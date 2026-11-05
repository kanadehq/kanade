#!/usr/bin/env python3
"""Build the pinned reference broker and the complete monitoring producer fork."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'target/nats-identity'
VERSION = '2.14.3'
# Official signed v2.14.3 tag; tag and commit archives have identical file contents.
SOURCE_COMMIT = '9b17a586be69c9ff36e4c84a421ca47da410b561'
SHA256 = 'dba5286035ce9017b897ea24a783551dc28b07ad50c78da5471ead2bcfab3e86'
URL = f'https://github.com/nats-io/nats-server/archive/refs/tags/v{VERSION}.tar.gz'


def environment():
    env = os.environ.copy()
    env.update(GOCACHE=str(OUT / 'cache/build'), GOMODCACHE=str(OUT / 'cache/mod'), GOTOOLCHAIN='local')
    return env


def prepare():
    OUT.mkdir(parents=True, exist_ok=True)
    archive = OUT / f'source-{VERSION}.tar.gz'
    if not archive.exists():
        urllib.request.urlretrieve(URL, archive)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        raise SystemExit('pinned broker source checksum mismatch')
    for variant in ['baseline', 'patched']:
        dest = OUT / variant
        if dest.exists():
            shutil.rmtree(dest)
        dest.mkdir()
        with tarfile.open(archive) as tar:
            if hasattr(tarfile, 'data_filter'):
                tar.extractall(dest, filter='data')
            else:  # older interpreters: reject escaping members manually
                for m in tar.getmembers():
                    if m.name.startswith('/') or '..' in Path(m.name).parts:
                        raise SystemExit('unsafe archive member')
                tar.extractall(dest)
        src = dest / f'nats-server-{VERSION}'
        if variant == 'baseline':
            shutil.copy2(ROOT / 'tools/nats-identity/identity_bench_test.go', src / 'server/identity_bench_test.go')
        if variant == 'patched':
            subprocess.run(['patch', '-s', '-p1', '-i', str(ROOT / 'tools/nats-identity/producer.patch')], cwd=src, check=True)
            for path in (ROOT / 'tools/nats-identity').glob('*.go'):
                shutil.copy2(path, src / 'server' / path.name)
            subprocess.run(['gofmt', '-w', *[str(p) for p in (src / 'server').glob('auth_identity*.go')]], env=environment(), check=True)
        binary = OUT / 'bin' / variant / 'nats-server'
        binary.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(['go', 'build', '-trimpath', '-buildvcs=false', '-o', str(binary), '.'], cwd=src, env=environment(), check=True)
    return OUT


if __name__ == '__main__':
    prepare()
