# Benchmarks

`bench/run.py` times disksweep and 9 other disk usage tools on the same
directories, checks every tool's total against `du`, and writes the results
to `bench/results/<machine>/`. The machine label comes from the hardware and
OS only, for example `m4-10c-24g-macos27`.

## Setup

```sh
bench/install.sh          # hyperfine and the other tools
```

Datasets are made by a seeded generator: the same ID gives the same files.
It refuses a non-empty DIR.

```sh
just bench-gen S1 /tmp/disksweep-bench/S1
```

| ID | shape | files |
| -- | ----- | ----- |
| S1 | binary tree, 12 levels, 4,095 dirs, 100 files each (the dumac README tree) | 409,500 |
| S2 | one dir | 1,000,000 |
| S3 | 1,000 x 1,000 dirs, 1 file each | 1,000,000 |
| S4 | a chain of 4,096 nested dirs, 1 file each | 4,096 |
| S5 | 100,000 files, each hard-linked into 10 dirs | 100,000 |
| S6 | 1,000 files of 1 MiB, each cloned with `cp -c` (macOS) | 1,000 |
| R1 | a real dev workspace: pass its path, results keep only the label | - |
| R2 | the whole data disk: `/System/Volumes/Data` on macOS, `/` on Linux | - |

Most files are empty or under 16 KiB, so S1 to S5 each allocate under 2 GB on
APFS. S3 on ext4 allocates 4 GiB more, for its 1M dirs of 4 KiB each.

## Running

```sh
just bench S1 /tmp/disksweep-bench/S1     # warm, 10 runs per config
just bench R1 "$BENCH_R1" --runs 3
just bench R2 /System/Volumes/Data   # macOS; `/` on Linux
just bench-cold S1 /tmp/disksweep-bench/S1
just bench-report                         # README table and gates
just bench-check                          # disksweep regressions
just bench-orb                            # S1 to S5 on Linux, in OrbStack
```

Warm runs time each tool with hyperfine (3 warmup runs, 10 timed), at its
default threads and, if it has a threads flag, at 1x, 2x and 4x the cores.
One more run per tool, under `/usr/bin/time`, records its peak memory.

Cold runs are run by hand, because they need sudo: `sudo purge` (macOS) or
dropping the page cache (Linux) runs before each of 5 runs, at default threads.

## Validity

The reference total is `du -skx` (macOS) or `du -s -B1 -x` (Linux):
allocated bytes, one filesystem, hard links once. A tool's total is valid
within 0.1% of it (dumac prints rounded units, so within its last digit too).
Where du fails for any reason but unreadable dirs (macOS du cannot read past
PATH_MAX, S4), run.py sums the same thing itself. Invalid totals are
recorded, not dropped. diskus, dumac and diskscour cannot stay on one
filesystem, so they are invalid on R2.

On macOS, R2 is `/System/Volumes/Data`, not `/`. There `/`, the Data volume and
firmlinks like `/Users` share one device id, so `du -skx /` counts the Data
volume twice and would be a wrong reference.

## Gates

`report` evaluates these per dataset, from its newest warm, cold and memory
results, and exits 1 if any fails. League: "tree" tools build a browsable
tree, "totals" tools (diskus, dumac, du) only print a sum. disksweep must win
both.

| gate | passes if |
| ---- | --------- |
| warm | disksweep at default threads has a 95% interval (mean +/- 1.96 sd / sqrt(n)) entirely below the fastest valid rival at its best threads |
| cold | disksweep's median is at most 1.05x the best rival's; a cold set counts only if its median is at least 2x the warm one |
| memory | disksweep has the lowest peak RSS of the tree tools |
| totals | every disksweep total is valid |

`check` fails if a disksweep warm median is more than 10% above the last
committed result for the same dataset and machine.
