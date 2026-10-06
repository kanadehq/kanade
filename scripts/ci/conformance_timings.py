#!/usr/bin/env python3
"""Summarise the timings the role conformance suite writes.

Reads the JSON lines named by the first argument (one per wait the suite
performed, plus one per whole-suite run that the workflow adds) and writes a
Markdown table per operating system to the file named by $GITHUB_STEP_SUMMARY,
or to stdout when that is unset. Standard library only.

Failed and timed-out waits are listed in their own column rather than dropped:
a distribution made of the successful runs alone under-reports the slow ones.
"""

import json
import math
import os
import sys
from collections import defaultdict


def percentile(sorted_values, p):
    if not sorted_values:
        return math.nan
    rank = max(0, math.ceil(p / 100 * len(sorted_values)) - 1)
    return sorted_values[rank]


def fmt(x):
    return "-" if math.isnan(x) else f"{x:.2f}"


def main(path):
    rows = []
    try:
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if line:
                    rows.append(json.loads(line))
    except FileNotFoundError:
        pass

    out = ["## NATS role conformance timings", ""]
    if not rows:
        out.append("No timings were recorded.")
    suites = [r for r in rows if r.get("kind") == "suite"]
    waits = [r for r in rows if r.get("kind") != "suite"]

    groups = defaultdict(list)
    for r in waits:
        groups[(r["os"], r["kind"], r.get("switch", ""), r["step"])].append(r)

    for os_name in sorted({k[0] for k in groups} | {r["os"] for r in suites}):
        out += [f"### {os_name}", ""]
        mine = [r for r in suites if r["os"] == os_name]
        if mine:
            secs = sorted(r["secs"] for r in mine)
            failed = sum(1 for r in mine if r["outcome"] != "ok")
            out.append(
                f"Whole suite (seconds): runs {len(mine)}, failed {failed}, "
                f"min {fmt(secs[0])}, p50 {fmt(percentile(secs, 50))}, "
                f"p95 {fmt(percentile(secs, 95))}, max {fmt(secs[-1])}."
            )
            out.append("")
        out += [
            "| kind | switch | step | n | failed | min | p50 | p95 | max | max concurrent tests |",
            "|---|---|---|--:|--:|--:|--:|--:|--:|--:|",
        ]
        for key in sorted(k for k in groups if k[0] == os_name):
            rs = groups[key]
            ok = sorted(r["secs"] for r in rs if r["outcome"] not in ("failed",))
            failed = sum(1 for r in rs if r["outcome"] == "failed")
            worst_failed = max((r["secs"] for r in rs if r["outcome"] == "failed"), default=None)
            mx = ok[-1] if ok else math.nan
            cell_max = fmt(mx) if worst_failed is None else f"{fmt(mx)} (failed at {worst_failed:.2f})"
            conc = max(r.get("concurrent", 0) for r in rs)
            out.append(
                f"| {key[1]} | {key[2] or '-'} | {key[3]} | {len(rs)} | {failed} | "
                f"{fmt(ok[0] if ok else math.nan)} | {fmt(percentile(ok, 50))} | "
                f"{fmt(percentile(ok, 95))} | {cell_max} | {conc} |"
            )
        out.append("")

    text = "\n".join(out) + "\n"
    target = os.environ.get("GITHUB_STEP_SUMMARY")
    if target:
        with open(target, "a", encoding="utf-8") as fh:
            fh.write(text)
    print(text)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: conformance_timings.py <timings.jsonl>")
    main(sys.argv[1])
