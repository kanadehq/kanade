#!/usr/bin/env python3
"""Interleaved same-source comparisons; retain raw samples and provenance."""
import hashlib
import json
import os
import platform
import re
import statistics
import subprocess
import time

import build

WORKLOADS = {
    'BenchmarkIdentityChurn', 'BenchmarkIdentityConnz',
    'BenchmarkIdentityMessagingMonitored', 'BenchmarkIdentityConfigBcrypt',
    'BenchmarkIdentityConfigBcryptDistinct', 'BenchmarkIdentityWSSBcryptChurn',
}
SAMPLES = 5
PROFILES = [('default', '4', 'off'), ('constrained', '1', '768MiB')]


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def parse_sample(output):
    rows = {}
    for line in output.splitlines():
        if not line.startswith('BenchmarkIdentity'):
            continue
        cells = line.split()
        name = re.sub(r'-\d+$', '', cells[0])
        if name not in WORKLOADS or name in rows or int(cells[1]) <= 0:
            raise ValueError('unexpected, duplicate or empty benchmark')
        metrics = {cells[i + 1]: float(cells[i]) for i in range(2, len(cells) - 1, 2)}
        required = {'ns/op', 'B/op', 'allocs/op'}
        if name == 'BenchmarkIdentityConnz':
            required |= {'client_bytes', 'identity_state_bytes', 'response_bytes'}
        if not required <= metrics.keys() or metrics['ns/op'] <= 0:
            raise ValueError('missing benchmark metric')
        if name == 'BenchmarkIdentityConnz' and (
                metrics['client_bytes'] <= 0 or metrics['response_bytes'] <= 0
                or metrics['identity_state_bytes'] < 0):
            raise ValueError('invalid retained-memory or response-size metric')
        rows[name] = metrics
    if rows.keys() != WORKLOADS or not re.search(r'^PASS$', output, re.MULTILINE):
        raise ValueError('benchmark comparison did not execute every workload successfully')
    return rows


def measure():
    # Rebuild from the current sources so provenance cannot describe an older fork.
    build.prepare()
    bench = build.ROOT / 'tools/nats-identity/identity_bench_test.go'
    for variant in ['baseline', 'patched']:
        copied = build.OUT / variant / f'nats-server-{build.VERSION}/server/identity_bench_test.go'
        if sha256(copied) != sha256(bench):
            raise SystemExit('rebuild both variants with identical benchmark code first')
    # Darwin's small ephemeral-port range can be exhausted by repeated churn
    # samples. Let TIME_WAIT expire between processes, outside benchmark timing.
    cooldown_seconds = 0
    if platform.system() == 'Darwin':
        msl_ms = int(subprocess.check_output(['sysctl', '-n', 'net.inet.tcp.msl'], text=True))
        cooldown_seconds = 2 * msl_ms / 1000 + 1
    results = {
        'environment': {'os': platform.system(), 'arch': platform.machine(),
                        'cpu_count': os.cpu_count(),
                        'go': subprocess.check_output(['go', 'version'], text=True).strip()},
        'provenance': {
            'nats_version': build.VERSION, 'source_commit': build.SOURCE_COMMIT,
            'source_url': build.URL, 'source_sha256': build.SHA256,
            'patch_sha256': sha256(build.ROOT / 'tools/nats-identity/producer.patch'),
            'producer_sha256': sha256(build.ROOT / 'tools/nats-identity/auth_identity.go'),
            'benchmark_sha256': sha256(bench),
            'build_driver_sha256': sha256(build.ROOT / 'tools/nats-identity/build.py'),
            'measurement_driver_sha256': sha256(build.ROOT / 'tools/nats-identity/measure.py'),
            'producer_tests_sha256': sha256(build.ROOT / 'tools/nats-identity/auth_identity_test.go'),
            'binary_sha256': {variant: sha256(build.OUT / 'bin' / variant / 'nats-server')
                              for variant in ['baseline', 'patched']},
            'initial_results': 'measurements-initial.json',
            'initial_results_sha256': sha256(build.ROOT / 'tools/nats-identity/measurements-initial.json'),
            'initial_revision': 'd0d12836d4b70facc59691f21346b5a0f920348f',
            'historical_results': 'measurements-2.14.1.json',
            'historical_results_sha256': sha256(build.ROOT / 'tools/nats-identity/measurements-2.14.1.json'),
        },
        'methodology': {
            'samples_per_variant': SAMPLES, 'benchtime': '500ms', 'count_per_process': 1,
            'order': 'within each profile, baseline/patched on rounds 1/3/5; patched/baseline on rounds 2/4',
            'profiles': {name: {'GOMAXPROCS': procs, 'GOMEMLIMIT': limit}
                         for name, procs, limit in PROFILES},
            'cooldown_seconds': cooldown_seconds,
            'cooldown': 'before each process, outside timed regions; Darwin 2*TCP MSL+1s; no retries',
            'cold_config': 'new server/policy per op, hash/certificate generation outside timed regions',
            'wss': 'direct broker TLS with verified disposable P-256 certificate, one bcrypt cost-8 user; no proxy',
            'limits': 'single Darwin host; GOMEMLIMIT is Go soft heap limit, not VM/RSS cap; no production load',
        },
        'runs': [], 'samples': {}, 'medians': {}, 'delta_percent': {},
    }
    for profile, procs, limit in PROFILES:
        for round_index in range(SAMPLES):
            order = ['baseline', 'patched'] if round_index % 2 == 0 else ['patched', 'baseline']
            for variant in order:
                if cooldown_seconds:
                    print(f'{profile}/{variant}: TCP cooldown {cooldown_seconds:g}s outside timing', flush=True)
                    time.sleep(cooldown_seconds)
                env = build.environment()
                env.update(GOMAXPROCS=procs, GOMEMLIMIT=limit)
                src = build.OUT / variant / f'nats-server-{build.VERSION}'
                completed = subprocess.run(
                    ['go', 'test', './server', '-run', '^$', '-bench', '^BenchmarkIdentity',
                     '-benchmem', '-benchtime=500ms', '-count=1'], cwd=src, env=env,
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                if completed.returncode:
                    print(f'{profile}/{variant} round {round_index + 1}: benchmark failed', flush=True)
                    print(completed.stdout, end='', flush=True)
                    completed.check_returncode()
                output = completed.stdout
                rows = parse_sample(output)
                key = profile + '/' + variant
                samples = results['samples'].setdefault(key, {})
                for name, metrics in rows.items():
                    samples.setdefault(name, []).append(metrics)
                results['runs'].append({'profile': profile, 'variant': variant,
                                        'round': round_index + 1, 'raw_output': output})
                print(f'{key} round {round_index + 1}: {len(rows)} workloads passed', flush=True)
        for variant in ['baseline', 'patched']:
            key = profile + '/' + variant
            samples = results['samples'][key]
            if samples.keys() != WORKLOADS or any(len(v) != SAMPLES for v in samples.values()):
                raise SystemExit('missing repeated benchmark samples')
            results['medians'][key] = {
                name: {metric: statistics.median(row[metric] for row in rows)
                       for metric in rows[0]}
                for name, rows in samples.items()}
            print(key, json.dumps(results['medians'][key]), flush=True)
        results['delta_percent'][profile] = {
            name: {metric: (value / results['medians'][profile + '/baseline'][name][metric] - 1) * 100
                   for metric, value in metrics.items()
                   if results['medians'][profile + '/baseline'][name][metric] != 0}
            for name, metrics in results['medians'][profile + '/patched'].items()}
    (build.ROOT / 'tools/nats-identity/measurements.json').write_text(
        json.dumps(results, indent=2) + '\n')


if __name__ == '__main__':
    measure()
