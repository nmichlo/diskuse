#!/usr/bin/env python3
"""A/B test disksweep builds: interleaved runs, medians, identical output.

    python3 bench/ab.py --bin base=path/to/a --bin new=path/to/b DIR...

Each round runs every build once on every dir, in a shuffled order, so
drift in load or caches hits all builds alike. The first round only warms
the caches. Reports medians of wall time, CPU (user + sys), instructions
retired and peak memory from `/usr/bin/time -l` (macOS), as a change
against the first build, and fails if any build's output differs.
"""

import argparse
import hashlib
import os
import random
import re
import statistics
import subprocess
import sys

FIELDS = {
    "wall": r"([\d.]+) real",
    "user": r"([\d.]+) user",
    "sys": r"([\d.]+) sys",
    "instr": r"(\d+)\s+instructions retired",
    "mem": r"(\d+)\s+peak memory footprint",
}


def run(binary: str, path: str, args: list[str]) -> tuple[dict, str]:
    cmd = ["/usr/bin/time", "-l", binary, "scan", path, *args]
    # a decimal comma would not parse
    env = {**os.environ, "LC_ALL": "C"}
    p = subprocess.run(cmd, capture_output=True, text=True, check=True, env=env)
    stats = {k: float(re.search(v, p.stderr).group(1)) for k, v in FIELDS.items()}
    stats["cpu"] = stats.pop("user") + stats.pop("sys")
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
    for r in range(a.rounds + 1):
        for d in a.dirs:
            for name, path in random.sample(bins, len(bins)):
                stats, out = run(path, d, args)
                outs.setdefault(d, {})[name] = out
                if r > 0:
                    runs[d, name].append(stats)
        print(f"round {r} done", file=sys.stderr)
    ok = True
    for d in a.dirs:
        print(f"\n{d}")
        print(
            f"  {'build':<12}{'wall s':>10}{'cpu s':>10}{'instr G':>10}{'mem MiB':>10}"
        )
        base = None
        for name, _ in bins:
            m = {
                k: statistics.median(s[k] for s in runs[d, name])
                for k in runs[d, name][0]
            }
            row = [m["wall"], m["cpu"], m["instr"] / 1e9, m["mem"] / 2**20]
            line = f"  {name:<12}" + "".join(f"{v:>10.3f}" for v in row)
            if base is None:
                base = row
            else:
                line += "   " + " ".join(
                    f"{(v / max(b, 1e-9) - 1) * 100:+.1f}%" for v, b in zip(row, base)
                )
            print(line)
        if len(set(outs[d].values())) > 1:
            print("  OUTPUT DIFFERS")
            ok = False
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
