#!/usr/bin/env python3
"""A/B test diskuse builds: interleaved runs, medians, identical output.

    python3 bench/ab.py --bin base=path/to/a --bin new=path/to/b DIR...

Each round runs every build once, in a shuffled order, so drift in load
hits all builds alike, and all rounds of a dir run before the next dir's.
The first round only warms the caches. Reports medians and quartiles of wall time and CPU (user + sys), and
median instructions retired and peak memory from `/usr/bin/time -l`
(macOS), as a change against the first build, and fails if any build's output differs.
"""

import argparse
import hashlib
import os
import random
import re
import resource
import statistics
import subprocess
import sys
import time

# from `/usr/bin/time -l`; wall and CPU are measured here, finer
FIELDS = {
    "instr": r"(\d+)\s+instructions retired",
    "mem": r"(\d+)\s+peak memory footprint",
}


def run(binary: str, path: str, args: list[str]) -> tuple[dict, str]:
    cmd = ["/usr/bin/time", "-l", binary, "scan", path, *args]
    # a decimal comma would not parse
    env = {**os.environ, "LC_ALL": "C"}
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    start = time.perf_counter()
    p = subprocess.run(cmd, capture_output=True, text=True, check=True, env=env)
    wall = time.perf_counter() - start
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    found = {k: re.search(v, p.stderr) for k, v in FIELDS.items()}
    stats = {k: float(m.group(1)) for k, m in found.items() if m}
    assert len(stats) == len(FIELDS), p.stderr
    cpu = (after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime)
    stats |= {"wall": wall, "cpu": cpu}
    return stats, hashlib.sha256(p.stdout.encode()).hexdigest()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", action="append", required=True, help="NAME=PATH")
    ap.add_argument("--rounds", type=int, default=7)
    ap.add_argument("--args", default="--json --depth 3")
    ap.add_argument("dirs", nargs="+")
    a = ap.parse_args()
    bins = [b.split("=", 1) for b in a.bin]
    args = a.args.split()
    runs = {(d, n): [] for d in a.dirs for n, _ in bins}
    outs = {}
    # one dir at a time: on a small machine, another dir's scan evicts
    # this one's metadata from the cache
    for d in a.dirs:
        for r in range(a.rounds + 1):
            for name, path in random.sample(bins, len(bins)):
                stats, out = run(path, d, args)
                outs.setdefault(d, {})[name] = out
                if r > 0:
                    runs[d, name].append(stats)
        print(f"{d} done", file=sys.stderr)
    ok = True
    for d in a.dirs:
        print(f"\n{d}")
        print(f"  {'build':<10}{'wall ms':>18}{'cpu ms':>18}{'instr G':>9}{'mem MiB':>9}")
        base = None
        for name, _ in bins:
            rs = runs[d, name]

            def q(k: str, rs: list[dict] = rs) -> tuple[float, float, float]:
                return tuple(statistics.quantiles([r[k] for r in rs], n=4, method="inclusive"))

            w, c = q("wall"), q("cpu")
            instr = statistics.median(r["instr"] for r in rs) / 1e9
            mem = statistics.median(r["mem"] for r in rs) / 2**20
            line = (
                f"  {name:<10}{w[1] * 1e3:>8.1f} [{w[0] * 1e3:.0f}-{w[2] * 1e3:.0f}]"
                f"{c[1] * 1e3:>8.1f} [{c[0] * 1e3:.0f}-{c[2] * 1e3:.0f}]"
                f"{instr:>9.3f}{mem:>9.2f}"
            )
            row = [w[1], c[1], instr, mem]
            if base is None:
                base = row
            else:
                line += "  " + " ".join(f"{(v / max(b, 1e-9) - 1) * 100:+.1f}%" for v, b in zip(row, base))
            print(line)
        if len(set(outs[d].values())) > 1:
            print("  OUTPUT DIFFERS")
            ok = False
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
