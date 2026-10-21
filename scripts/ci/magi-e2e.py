#!/usr/bin/env python3
"""既存の focused e2e を実行し、空振りと CI の NATS ピンのずれを拒否する。"""

import os
from pathlib import Path
import re
import shlex
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]


def commands():
    yield ["cargo", "test", "-p", "kanade-backend", "--",
           "api::agent_releases", "api::app_artifacts",
           "api::exec::typed_gate_e2e", "--ignored"]
    for module in ("package_manifest", "publisher_trust", "package_admission",
                   "typed_operation", "wire::typed_command", "wire::envelope",
                   "wire::command"):
        yield ["cargo", "test", "-p", "kanade-shared", "--lib", "--",
               module + "::tests::"]
    # Stage 3: typed deployment is schema-only; every delivery path refuses it.
    yield ["cargo", "test", "-p", "kanade-backend", "--bins", "--",
           "api::exec::tests::typed_deployment_is_refused",
           "api::jobs::tests::job_list_row_for_a_typed"]
    yield ["cargo", "test", "-p", "kanade-agent", "--bins", "--", "typed"]
    yield ["cargo", "test", "-p", "kanade", "--bins", "--", "typed"]
    yield ["cargo", "test", "-p", "kanade", "--test", "app_manifest_cli",
           "--test", "app_trust_cli"]


def executed_tests(output):
    # Cargo が複数の harness を起動する場合も、実行件数と結果を突き合わせる。
    output = re.sub(r"\x1b\[[0-9;]*m", "", output)
    running = re.findall(r"^running (\d+) tests?\s*$", output, re.MULTILINE)
    results = re.findall(
        r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;",
        output, re.MULTILINE)
    passed = sum(int(row[0]) for row in results)
    return (bool(running) and len(running) == len(results) and passed > 0
            and passed == sum(map(int, running))
            and all(int(count) == int(row[0])
                    for count, row in zip(running, results))
            and all(int(failed) == 0 and int(ignored) == 0
                    for _, failed, ignored in results))


def run(command, env):
    print(shlex.join(command), flush=True)
    result = subprocess.run(command, cwd=ROOT, env=env, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    print(result.stdout, end="", flush=True)
    if result.returncode:
        return max(1, result.returncode)
    if not executed_tests(result.stdout):
        print("ERROR: テストが 0 件、ignored を含む、または実行・成功件数が不一致です。",
              file=sys.stderr)
        return 1
    return 0


def check_nats():
    workflow = (ROOT / ".github/workflows/integration.yml").read_text()
    version = re.search(r'^  NATS_SERVER_VERSION: "([^"]+)"$', workflow,
                        re.MULTILINE)
    if version is None:
        raise ValueError("integration.yml の NATS ピンを確認できません。")
    expected = "nats-server: v" + version[1]
    result = subprocess.run(["nats-server", "--version"], text=True,
                            capture_output=True, check=True)
    if result.stdout.strip() != expected:
        raise ValueError(f"NATS バージョン不一致: expected {expected}, got {result.stdout.strip()}")


def main():
    check_nats()
    env = os.environ.copy()
    env["KANADE_APP_ARTIFACT_PUBLICATION"] = "backend-exclusive"
    env["CARGO_TERM_COLOR"] = "never"
    for command in commands():
        code = run(command, env)
        if code:
            return code
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(1)
