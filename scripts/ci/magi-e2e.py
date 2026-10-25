#!/usr/bin/env python3
"""既存の focused e2e を実行し、空振りと CI の NATS ピンのずれを拒否する。"""

import os
import platform
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile

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
    # Stage 4: plan persistence, the shared gate, capability evidence and
    # preflight run against real SQLite databases and the real plan routes.
    yield ["cargo", "test", "-p", "kanade-shared", "--lib", "--",
           "deploy_plan::tests::", "wire::deploy_capabilities::tests::",
           "wire::heartbeat::tests::"]
    yield ["cargo", "test", "-p", "kanade-backend", "--bins", "--",
           "api::deploy_plans::tests::", "deploy_gate::tests::",
           "api::agents::tests::deploy_capabilities",
           "api::agent_releases::tests::agent_self_update_is_not_gated"]
    yield ["cargo", "test", "-p", "kanade-agent", "--bins", "--",
           "deploy_handlers::tests::"]
    # Stage 5a: isolated native CLI installer, including recovery and ACL checks.
    yield ["cargo", "test", "-p", "kanade-agent", "--bins", "--",
           "native_install::"]
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


def nats_version():
    workflow = (ROOT / ".github/workflows/integration.yml").read_text()
    version = re.search(r'^  NATS_SERVER_VERSION: "(\d+\.\d+\.\d+)"$', workflow,
                        re.MULTILINE)
    if version is None:
        raise ValueError("integration.yml の NATS ピンを確認できません。")
    return version[1]


def check_nats(binary="nats-server", version=None, env=None):
    expected = "nats-server: v" + (version or nats_version())
    result = subprocess.run([str(binary), "--version"], env=env, text=True,
                            capture_output=True, check=True)
    if result.stdout.strip() != expected:
        raise ValueError(f"NATS バージョン不一致: expected {expected}, got {result.stdout.strip()}")


def nats_platform():
    system = platform.system().lower()
    arch = {"aarch64": "arm64", "arm64": "arm64",
            "x86_64": "amd64", "amd64": "amd64"}.get(platform.machine().lower())
    if system not in ("darwin", "linux", "windows") or arch is None:
        raise ValueError("このホスト用の NATS 配布物は未対応です。")
    return f"{system}-{arch}"


def download_nats(version, host, destination):
    archive = f"nats-server-v{version}-{host}"
    suffix = ".zip" if host.startswith("windows-") else ".tar.gz"
    url = f"https://github.com/nats-io/nats-server/releases/download/v{version}/{archive}{suffix}"
    bundle = destination.parent / ("nats" + suffix)
    print(f"NATS 検証用ツールを取得: {url}", flush=True)
    with urllib.request.urlopen(url, timeout=30) as response, bundle.open("wb") as out:
        shutil.copyfileobj(response, out)
    member = f"{archive}/{destination.name}"
    if suffix == ".zip":
        with zipfile.ZipFile(bundle) as package, package.open(member) as source:
            with destination.open("wb") as out:
                shutil.copyfileobj(source, out)
    else:
        with tarfile.open(bundle) as package:
            entry = package.getmember(member)
            if not entry.isfile():
                raise ValueError("NATS 配布物の実行ファイルが通常ファイルではありません。")
            with package.extractfile(entry) as source, destination.open("wb") as out:
                shutil.copyfileobj(source, out)
        destination.chmod(0o755)


def prepare_nats(env):
    version = nats_version()
    host = nats_platform()
    name = "nats-server.exe" if host.startswith("windows-") else "nats-server"
    bin_dir = ROOT / "target" / "magi-tools" / "nats" / version / host / "bin"
    binary = bin_dir / name
    source = env.get("KANADE_TEST_NATS_SERVER")
    if source:
        source = Path(source).resolve(strict=True)
        check_nats(source, version, env)
    elif binary.is_file():
        check_nats(binary, version, env)
    else:
        source = shutil.which("nats-server", path=env.get("PATH", ""))
        if source:
            try:
                check_nats(source, version, env)
            except (ValueError, subprocess.SubprocessError):
                source = None
    if source or not binary.is_file():
        bin_dir.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=bin_dir.parent) as staging:
            staged = Path(staging) / name
            if source:
                shutil.copy2(source, staged)
            else:
                download_nats(version, host, staged)
            check_nats(staged, version, env)
            staged.replace(binary)
    env = env.copy()
    env["PATH"] = str(bin_dir) + os.pathsep + env.get("PATH", "")
    # Rust fixture と同じ PATH 解決でも固定版を選ぶことを起動前に検査する。
    check_nats(version=version, env=env)
    return env


def main():
    env = prepare_nats(os.environ.copy())
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
    except (OSError, ValueError, subprocess.SubprocessError,
            tarfile.TarError, zipfile.BadZipFile) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(1)
