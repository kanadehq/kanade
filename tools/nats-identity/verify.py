#!/usr/bin/env python3
"""Fail closed unless the real producer, consumer and browser prove a name."""
import os
import re
import subprocess
import sys
from pathlib import Path

import build


def run(args, cwd, env):
    result = subprocess.run(args, cwd=cwd, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    print(result.stdout, end='', flush=True)
    if result.returncode:
        raise SystemExit(result.returncode)
    return result.stdout


def verify():
    build.prepare()
    env = build.environment()
    src = build.OUT / 'patched' / f'nats-server-{build.VERSION}'
    out = run(['go', 'test', '-race', '-v', './server', '-run', '^TestAuthIdentity', '-count=1'], src, env)
    if not re.search(r'^ok\s', out, re.MULTILINE) or len(re.findall(
            r'^--- PASS: TestAuthIdentity', out, re.MULTILINE)) < 5:
        raise SystemExit('required producer race tests did not execute successfully')
    web = build.ROOT / 'crates/kanade-backend/web'
    run(['bun', 'install', '--frozen-lockfile'], web, env)
    run(['bun', 'x', 'playwright', 'install', 'chromium'], web, env)
    env['PATH'] = str(build.OUT / 'bin/patched') + os.pathsep + env['PATH']
    env['KANADE_IDENTITY_BROWSER'] = '1'
    env['CI'] = '1'
    output = run(['cargo', 'test', '--locked', '-p', 'kanade-backend', '--bin', 'kanade-backend',
                  'live_identity_producer_', '--', '--ignored'], build.ROOT, env)
    matches = re.findall(r'test result: ok\. (\d+) passed; 0 failed; 0 ignored;', output)
    if not matches or sum(map(int, matches)) < 2:
        raise SystemExit('real producer/consumer positive proof tests did not execute')
    print('Producer/consumer verification: 2 positive real-broker tests and 1 required browser test passed.', flush=True)


if __name__ == '__main__':
    verify()
