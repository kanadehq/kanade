#!/usr/bin/env python3
"""Same-source, repeated reference/fork workloads; only aggregate metrics leave."""
import json
import os
import platform
import re
import statistics
import subprocess
from pathlib import Path
import build


def measure():
    # Build first with build.py. Go benchmark sources are identical in both trees.
    results = {'environment': {'os': platform.system(), 'arch': platform.machine(),
                               'cpu_count': os.cpu_count(), 'go': subprocess.check_output(['go','version'], text=True).strip()},
               'samples': {}, 'medians': {}}
    for profile, procs, limit in [('default','4','off'), ('constrained','1','768MiB')]:
        for variant in ['baseline','patched']:
            env = build.environment()
            env.update(GOMAXPROCS=procs, GOMEMLIMIT=limit)
            src = build.OUT / variant / f'nats-server-{build.VERSION}'
            output = subprocess.check_output(['go','test','./server','-run','^$',
                '-bench','^BenchmarkIdentity','-benchmem','-benchtime=500ms','-count=5'], cwd=src, env=env, text=True)
            key = profile + '/' + variant
            rows = {}
            for line in output.splitlines():
                if not line.startswith('BenchmarkIdentity'): continue
                cells=line.split(); name=re.sub(r'-\d+$','',cells[0])
                metrics={cells[i+1]:float(cells[i]) for i in range(2,len(cells)-1,2)}
                rows.setdefault(name,[]).append(metrics)
            if len(rows)!=4 or any(len(v)!=5 for v in rows.values()):
                raise SystemExit('benchmark comparison did not execute every sample')
            results['samples'][key]=rows
            results['medians'][key]={name:{metric:statistics.median(s[metric] for s in samples)
                for metric in samples[0]} for name,samples in rows.items()}
            print(key, json.dumps(results['medians'][key]), flush=True)
    dest=build.ROOT/'tools/nats-identity/measurements.json'
    dest.write_text(json.dumps(results,indent=2)+'\n')


if __name__=='__main__': measure()
