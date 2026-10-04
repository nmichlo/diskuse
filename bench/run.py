"""Benchmarks diskuse against other disk usage tools. See bench/README.md.

python3 bench/run.py warm <dataset> <path> [--runs N]
python3 bench/run.py cold <dataset> <path> [--runs N]
python3 bench/run.py report
python3 bench/run.py check
"""

import argparse
import datetime
import json
import math
import os
import platform
import re
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
from collections.abc import Callable
from dataclasses import dataclass
from dataclasses import field
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RESULTS = REPO / "bench" / "results"
MACOS = sys.platform == "darwin"
CORES = os.cpu_count() or 1
TODAY = str(datetime.datetime.now().astimezone().date())
TOLERANCE = 0.001  # a total is valid within 0.1% of the reference


# --- tools -------------------------------------------------------------------


# colour codes: some tool versions (dua 2.34) colour output even into a pipe
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def first_int(out: str) -> int:
    m = re.search(r"\d+", ANSI.sub("", out))
    assert m, out
    return int(m.group())


def last_line_int(out: str) -> int:
    # dua prints each child, then a total line if there is more than one
    return first_int(out.strip().splitlines()[-1])


def diskus_bytes(out: str) -> int:
    # a terminal gets "1.23 GB (1,234,567 bytes)", a pipe the bare number
    m = re.search(r"\(([\d,]+) bytes\)", out)
    return int(m.group(1).replace(",", "")) if m else first_int(out)


def ncdu_bytes(out: str) -> int:
    total, seen = 0, set()

    def walk(node):
        nonlocal total
        item, children = (node[0], node[1:]) if isinstance(node, list) else (node, [])
        if item.get("hlnkc"):
            if item["ino"] in seen:
                return
            seen.add(item["ino"])
        total += item.get("dsize", 0)
        for c in children:
            walk(c)

    walk(json.loads(out)[3])
    return total


UNITS = {"B": 1, "K": 1 << 10, "M": 1 << 20, "G": 1 << 30, "T": 1 << 40}


def human(out: str) -> tuple[int, int]:
    """`697.9M` -> bytes, and the slack its rounding allows: one last digit."""
    m = re.match(r"\s*([\d.]+)([BKMGT]?)", out)
    assert m, out
    num, unit = m.groups()
    scale = UNITS[unit or "B"]
    decimals = len(num.partition(".")[2])
    return round(float(num) * scale), scale // 10**decimals


@dataclass(frozen=True)
class Tool:
    name: str
    exe: str
    league: str  # "tree" builds a browsable tree, "totals" only sums
    args: Callable[[str, int | None], list[str]]  # (path, threads) -> argv[1:]
    parse: Callable[[str], int | tuple[int, int]]  # -> bytes, or (bytes, slack)
    threads: bool = True
    one_fs: bool = True  # has a stay-on-one-filesystem option
    check_args: list[str] = field(default_factory=list)  # extra, for the total
    version: list[str] | None = field(default_factory=lambda: ["--version"])
    macos_only: bool = False


def opt(flag: str, threads: int | None) -> list[str]:
    return [] if threads is None else [flag, str(threads)]


TOOLS = [
    Tool(
        "diskuse",
        str(REPO / "target" / "release" / "diskuse"),
        "tree",
        lambda p, t: ["scan", p, *opt("--threads", t)],
        lambda out: json.loads(out)["size"],
        # times the plain text output, reads the exact total from the JSON
        check_args=["--json", "--depth", "0"],
    ),
    Tool(
        "dua",
        "dua",
        "tree",
        lambda p, t: ["-x", *opt("-t", t), "--format", "bytes", "aggregate"] + ["--no-sort", p],
        last_line_int,
    ),
    Tool(
        "gdu",
        "gdu-go",
        "tree",
        lambda p, t: ["-n", "-s", "-p", "--no-prefix", "-x", *opt("-m", t), p],
        first_int,
    ),
    Tool(
        "dust",
        "dust",
        "tree",
        lambda p, t: ["-d0", "-P", "-c", "-b", "-o", "b", "-x", *opt("-T", t), p],
        first_int,
    ),
    Tool(
        "pdu",
        "pdu",
        "tree",
        lambda p, t: ["-x", "-H", "-b", "plain", "--json-output"] + [*opt("--threads", t), p],
        lambda out: json.loads(out)["tree"]["size"],
    ),
    Tool(
        "ncdu",
        "ncdu",
        # `-o -` streams the export without building a browsable tree, so it
        # competes with the totals-only tools, not the tree builders
        "totals",
        lambda p, t: ["-0", "-x", *opt("-t", t), "-o", "-", p],
        ncdu_bytes,
        version=["-v"],
    ),
    Tool(
        "diskscour",
        "diskscour",
        "tree",
        lambda p, t: ["scan", "--full", "--json", p],
        lambda out: json.loads(out)["total_bytes"],
        threads=False,
        one_fs=False,
        macos_only=True,
    ),
    Tool(
        "diskus",
        "diskus",
        "totals",
        lambda p, t: [*opt("-j", t), p],
        diskus_bytes,
        one_fs=False,
    ),
    Tool(
        "dumac",
        "dumac",
        "totals",
        lambda p, t: [p],
        human,
        threads=False,
        one_fs=False,
        version=None,
        macos_only=True,
    ),
    Tool(
        "du",
        "/usr/bin/du" if MACOS else "du",
        "totals",
        (lambda p, t: ["-skx", p]) if MACOS else (lambda p, t: ["-s", "-B1", "-x", p]),
        (lambda out: first_int(out) * 1024) if MACOS else first_int,
        threads=False,
        version=None if MACOS else ["--version"],
    ),
]
DU = TOOLS[-1]


def installed() -> tuple[list[Tool], list[str]]:
    found, missing = [], []
    for tool in TOOLS:
        if tool.macos_only and not MACOS:
            continue
        (found if shutil.which(tool.exe) else missing).append(tool)
    for tool in missing:
        print(f"note: skipping {tool.name}: {tool.exe} not found", file=sys.stderr)
    return found, [t.name for t in missing]


def version(tool: Tool) -> str:
    """The version number, plus the git rev for git builds."""
    if tool.version is None:
        # cargo-installed git builds have no --version; the list has the rev
        has_cargo = shutil.which("cargo")
        out = run(["cargo", "install", "--list"]).stdout if has_cargo else ""
        out = next((x for x in out.splitlines() if x.startswith(tool.name + " ")), "")
    else:
        out = run([tool.exe, *tool.version]).stdout
    if tool.name == "diskuse":
        out += " #" + git("describe", "--always", "--dirty").strip()
    num = re.search(r"\d+\.\d+[\w.]*", out)
    rev = re.search(r"#([\w-]+)", out)
    return (num.group() if num else "system") + (f"@{rev.group(1)}" if rev else "")


# --- measuring ---------------------------------------------------------------


def run(argv: list[str], **kw) -> subprocess.CompletedProcess:
    return subprocess.run(argv, capture_output=True, text=True, check=False, **kw)


def walk_bytes(path: str) -> int:
    """Allocated bytes below `path`, one device, hard links once, like du.

    Only used when du fails: BSD du cannot read past PATH_MAX (S4). Each dir
    is opened relative to its parent and listed from an explicit stack, so
    neither depth nor Python's recursion limit is a problem.
    """
    root = os.lstat(path)
    total, seen = root.st_blocks * 512, set()
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    stack = [os.open(path, os.O_RDONLY | os.O_DIRECTORY)]
    while stack:
        fd = stack.pop()
        try:
            names = os.listdir(fd)
        except OSError:
            names = []
        for name in names:
            try:
                st = os.stat(name, dir_fd=fd, follow_symlinks=False)
            except OSError:
                continue
            if st.st_dev != root.st_dev:
                continue  # a mount point: skip it, like du -x
            if stat.S_ISDIR(st.st_mode):
                try:
                    stack.append(os.open(name, flags, dir_fd=fd))
                except OSError:
                    pass  # unreadable: its own blocks still count
            elif st.st_nlink > 1:
                if st.st_ino in seen:
                    continue
                seen.add(st.st_ino)
            total += st.st_blocks * 512
        os.close(fd)
    return total


def reference(path: str, label: str) -> dict:
    """du's total, unless du hit errors other than unreadable dirs, which
    every tool skips the same way."""
    out = run([DU.exe, *DU.args(path, None)])
    denied = ("Permission denied", "Operation not permitted")
    errors = [e for e in out.stderr.splitlines() if not e.endswith(denied)]
    if not errors:
        command = recorded(DU, path, label, None)
        return {"command": command, "bytes": DU.parse(out.stdout)}
    error = errors[0].rsplit(": ", 1)[-1]  # the reason, not the long path
    print(f"note: du failed ({error}), summing in Python instead", file=sys.stderr)
    return {"command": "bench/run.py walk_bytes", "bytes": walk_bytes(path)}


def recorded(tool: Tool, path: str, label: str, threads: int | None) -> str:
    """The command with the tool's name and the dataset label, never a path."""
    args = [label if a == path else a for a in tool.args(path, threads)]
    return shlex.join([tool.name, *args])


def total(tool: Tool, path: str, threads: int | None, ref: int) -> dict:
    out = run([tool.exe, *tool.args(path, threads), *tool.check_args])
    try:
        parsed = tool.parse(out.stdout)
    except (AttributeError, IndexError, KeyError, ValueError, TypeError) as e:
        stderr = out.stderr.strip().replace(path, "<path>")[-200:]
        return {
            "valid": False,
            "note": f"exit {out.returncode}, unparsed: {e!r} {stderr}",
        }
    bytes_, slack = parsed if isinstance(parsed, tuple) else (parsed, 0)
    valid = abs(bytes_ - ref) <= TOLERANCE * ref + slack
    res = {"total": bytes_, "valid": valid}
    if out.returncode:
        res["note"] = f"exit {out.returncode}"
    return res


def hyperfine(argv: list[str], extra: list[str]) -> dict:
    with tempfile.NamedTemporaryFile(suffix=".json") as f:
        # -N: no shell, so nothing to subtract; -i: failures are timed too
        cmd = ["hyperfine", "-N", "-i", "--style", "none", "--export-json", f.name]
        subprocess.run([*cmd, *extra, shlex.join(argv)], check=True)
        r = json.loads(Path(f.name).read_text())["results"][0]
    keys = ("mean", "stddev", "median", "min", "max", "times")
    return {k: r[k] for k in keys} | {"failed_runs": sum(c != 0 for c in r["exit_codes"])}


def peak_rss(argv: list[str]) -> int:
    inner = shlex.join(argv) + " >/dev/null 2>&1"
    flag, pattern, scale = (
        ("-l", r"(\d+)\s+maximum resident set size", 1)
        if MACOS
        else ("-v", r"Maximum resident set size \(kbytes\): (\d+)", 1024)
    )
    out = run(["/usr/bin/time", flag, "sh", "-c", inner], env=os.environ | {"LC_ALL": "C"})
    m = re.search(pattern, out.stderr)
    assert m, out.stderr
    return int(m.group(1)) * scale


def configs(tool: Tool, sweep: bool) -> list[int | None]:
    """Default threads (None), then 1x, 2x, 4x cores if the tool takes a flag."""
    return [None] + ([CORES, 2 * CORES, 4 * CORES] if sweep and tool.threads else [])


def bench(args, kind: str) -> None:
    path, label = args.path, args.dataset
    if not os.path.isdir(path):
        sys.exit(f"{path}: not a directory")
    if not Path(TOOLS[0].exe).exists():
        sys.exit("diskuse is not built: run `cargo build --release`")
    tools, skipped = installed()
    file = RESULTS / machine(args) / f"{TODAY}-{label}.json"
    data = json.loads(file.read_text()) if file.exists() else {}
    cache = tempfile.TemporaryDirectory()
    # the tools that save scans save them here, never in the user's cache
    os.environ["DISKUSE_CACHE_DIR"] = os.path.join(cache.name, "diskuse")
    os.environ["DISKSCOUR_CACHE_DIR"] = os.path.join(cache.name, "diskscour")
    if kind == "cold":
        prepare = "sudo purge" if MACOS else "sudo sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'"
        # ask for the password once, up front
        subprocess.run(["sudo", "-v"], check=True)
        extra = ["--prepare", prepare, "--runs", str(args.runs or 5)]
    else:
        extra = ["--warmup", "3", "--runs", str(args.runs or 10)]
    ref = reference(path, label)
    print(f"reference: {ref['bytes']} bytes", file=sys.stderr)
    rows, memory = [], {}
    for tool in tools:
        for threads in configs(tool, kind == "warm"):
            row = {"tool": tool.name, "league": tool.league, "threads": threads}
            row["command"] = recorded(tool, path, label, threads)
            if label == "R2" and not tool.one_fs:
                rows.append(row | {"valid": False, "note": "cannot stay on one filesystem"})
                break
            argv = [tool.exe, *tool.args(path, threads)]
            row |= total(tool, path, threads, ref["bytes"])
            row |= hyperfine(argv, extra)
            rows.append(row)
            valid = "valid" if row["valid"] else "INVALID"
            print(f"{row['command']}: {row['mean']:.3f} s, {valid}", file=sys.stderr)
            if kind == "warm" and threads is None:
                memory[tool.name] = peak_rss(argv)
    data |= {
        "machine": machine(args),
        "dataset": label,
        "date": TODAY,
        "cores": CORES,
        "reference": ref,
        "versions": data.get("versions", {}) | {t.name: version(t) for t in tools},
        "skipped": skipped,
        kind: rows,
    }
    if kind == "warm":
        data["memory"] = memory
    file.parent.mkdir(parents=True, exist_ok=True)
    file.write_text(json.dumps(data, indent=1) + "\n")
    print(f"wrote {file.relative_to(REPO)}", file=sys.stderr)


def machine(args) -> str:
    """A label from the hardware and OS only, never the host or user name."""
    if args.machine:
        return args.machine
    if MACOS:

        def sysctl(key: str) -> str:
            return run(["sysctl", "-n", key]).stdout.strip()

        brand = sysctl("machdep.cpu.brand_string")
        chip = brand.removeprefix("Apple ").lower().replace(" ", "") if "Apple" in brand else platform.machine()
        ram = int(sysctl("hw.memsize")) >> 30
        os_ = "macos" + run(["sw_vers", "-productVersion"]).stdout.split(".")[0]
    else:
        chip = platform.machine()
        meminfo = Path("/proc/meminfo").read_text()
        ram = round(int(re.search(r"MemTotal:\s+(\d+)", meminfo).group(1)) / (1 << 20))
        rel = dict(re.findall(r'^(\w+)="?([^"\n]*)', Path("/etc/os-release").read_text(), re.MULTILINE))
        os_ = rel.get("ID", "linux") + rel.get("VERSION_ID", "")
    return f"{chip}-{CORES}c-{ram}g-{os_}"


# --- report and gates --------------------------------------------------------


def ci(row: dict) -> tuple[float, float]:
    """The 95% interval of the mean: mean +/- 1.96 * stddev / sqrt(n)."""
    n, sd = len(row["times"]), row["stddev"] or 0.0
    half = 1.96 * sd / math.sqrt(n) if n > 1 else math.inf
    return row["mean"] - half, row["mean"] + half


def timed(row: dict) -> bool:
    return row.get("valid", False) and "mean" in row


def best(rows: list[dict], tool: str) -> dict | None:
    ok = [r for r in rows if r["tool"] == tool and timed(r)]
    return min(ok, key=lambda r: r["mean"]) if ok else None


def default(rows: list[dict], tool: str) -> dict | None:
    return next((r for r in rows if r["tool"] == tool and r["threads"] is None), None)


def mib(n: int) -> str:
    return f"{n / (1 << 20):.1f}"


def fmt(row: dict | None) -> str:
    if row is None or "mean" not in row:
        return "-"
    lo, hi = ci(row)
    return f"{row['mean']:.3f} +/- {(hi - lo) / 2:.3f}"


def results(machine_dir: Path) -> dict[str, list[tuple[Path, dict]]]:
    """Each dataset's result files, oldest first (names start with the date)."""
    by_dataset = {}
    for f in sorted(machine_dir.glob("*.json")):
        data = json.loads(f.read_text())
        by_dataset.setdefault(data["dataset"], []).append((f, data))
    return by_dataset


def latest(machine_dir: Path) -> dict[str, dict]:
    """The newest warm, cold and memory results of each dataset, merged, so a
    cold run on a later day does not hide the warm one."""
    merged = {}
    for name, files in results(machine_dir).items():
        merged[name] = {}
        for _, data in files:
            merged[name] |= data
    return merged


def is_cold(row: dict, warm: list[dict]) -> bool:
    """A cold set counts only if its median is at least 2x the warm one."""
    w = default(warm, row["tool"])
    return timed(row) and w is not None and timed(w) and row["median"] >= 2 * w["median"]


def gates(d: dict) -> list[tuple[bool | None, str]]:
    """(True pass / False fail / None skip, line) for each gate of a dataset."""
    out = []
    warm, cold = d.get("warm", []), d.get("cold", [])
    ours = default(warm, "diskuse")
    others = {r["tool"]: r["league"] for r in warm + cold if r["tool"] != "diskuse"}
    for league in ("tree", "totals"):
        rivals = [t for t, lg in others.items() if lg == league]
        theirs = [b for t in rivals if (b := best(warm, t))]
        if ours is None or not theirs:
            out.append((None, f"warm/{league}: nothing to compare"))
        else:
            rival = min(theirs, key=lambda r: r["mean"])
            ok = timed(ours) and ci(ours)[1] < ci(rival)[0]
            out.append(
                (
                    ok,
                    f"warm/{league}: diskuse {fmt(ours)} s vs {rival['tool']} {fmt(rival)} s (threads {rival['threads'] or 'default'})",
                )
            )
        our_cold = default(cold, "diskuse")
        their_cold = [r for r in cold if r["tool"] in rivals and is_cold(r, warm)]
        if our_cold is None or not their_cold:
            out.append((None, f"cold/{league}: no cold runs"))
        elif not is_cold(our_cold, warm):
            out.append(
                (
                    None,
                    f"cold/{league}: diskuse cold median {our_cold['median']:.3f} s is under 2x warm, not cold",
                )
            )
        else:
            rival = min(their_cold, key=lambda r: r["median"])
            ok = our_cold["median"] <= 1.05 * rival["median"]
            out.append(
                (
                    ok,
                    f"cold/{league}: diskuse median {our_cold['median']:.3f} s vs {rival['tool']} {rival['median']:.3f} s (limit 1.05x)",
                )
            )
    mem = d.get("memory", {})
    tree_mem = {t: b for t, b in mem.items() if others.get(t) == "tree"}
    if "diskuse" in mem and tree_mem:
        rival = min(tree_mem, key=lambda t: tree_mem[t])
        ok = mem["diskuse"] <= tree_mem[rival]
        out.append(
            (
                ok,
                f"memory: diskuse {mib(mem['diskuse'])} MiB vs {rival} {mib(tree_mem[rival])} MiB",
            )
        )
    ours_all = [r for r in warm + cold if r["tool"] == "diskuse"]
    bad = [r for r in ours_all if not r.get("valid")]
    out.append(
        (
            bool(ours_all) and not bad,
            f"totals: {len(ours_all) - len(bad)}/{len(ours_all)} diskuse runs valid",
        )
    )
    return out


def table(d: dict) -> list[str]:
    ref = d["reference"]["bytes"]
    lines = [
        f"## {d['dataset']} ({d['date']})",
        "",
        f"Reference `{d['reference']['command']}`: {ref:,} bytes. Times in seconds, mean +/- 95% interval.",
        "",
        "| tool | league | total | warm, default threads | warm, best | best threads | cold median | peak RSS MiB |",
        "| --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    warm, cold, mem = d.get("warm", []), d.get("cold", []), d.get("memory", {})
    for tool in dict.fromkeys(r["tool"] for r in warm + cold):
        row = default(warm, tool) or default(cold, tool)
        assert row, tool
        if row.get("valid"):
            status = "valid"
        elif "total" in row:
            status = f"INVALID ({(row['total'] - ref) / ref:+.2%})"
        else:
            status = f"INVALID ({row.get('note', 'error')})".replace("|", "/")
        b, c = best(warm, tool), default(cold, tool)
        cells = [
            tool,
            row["league"],
            status,
            fmt(default(warm, tool)),
            fmt(b),
            str(b["threads"] or "default") if b else "-",
            f"{c['median']:.3f}" if c and "median" in c else "-",
            mib(mem[tool]) if tool in mem else "-",
        ]
        lines.append("| " + " | ".join(cells) + " |")
    return lines


def report(args) -> int:
    machine_dir = RESULTS / machine(args)
    datasets = latest(machine_dir)
    if not datasets:
        sys.exit(f"no results in {machine_dir.relative_to(REPO)}")
    md = [
        f"# {machine(args)}",
        "",
        "Generated by `bench/run.py report` from the newest warm, cold and memory results per dataset.",
    ]
    failed = False
    for name in sorted(datasets):
        d = datasets[name]
        md += ["", *table(d), "", "Gates:", ""]
        for ok, line in gates(d):
            word = {True: "PASS", False: "FAIL", None: "SKIP"}[ok]
            failed |= ok is False
            print(f"{word} {name} {line}")
            md.append(f"- {word} {line}")
        versions = ", ".join(f"{t} {v}" for t, v in d.get("versions", {}).items())
        md += ["", f"Versions: {versions}."]
    (machine_dir / "README.md").write_text("\n".join(md) + "\n")
    return int(failed)


def git(*args: str) -> str:
    return run(["git", "-C", str(REPO), *args]).stdout


def check(args) -> int:
    """Fails if a diskuse warm median is > 10% above the last committed one."""
    machine_dir = RESULTS / machine(args)
    tracked = set(git("ls-files", str(machine_dir)).split())
    failed = False
    for name, files in sorted(results(machine_dir).items()):
        warm = [(f, d) for f, d in files if "warm" in d]
        if not warm:
            continue
        current = warm[-1][1]
        committed = [
            json.loads(git("show", f"HEAD:{f.relative_to(REPO)}"))
            for f, _ in warm
            if str(f.relative_to(REPO)) in tracked
        ]
        # an unchanged committed copy of the newest result is not "previous"
        previous = [c for c in committed if c != current and "warm" in c]
        now = default(current["warm"], "diskuse")
        then = default(previous[-1]["warm"], "diskuse") if previous else None
        if not (now and then and timed(now) and timed(then)):
            print(f"SKIP {name}: no previous committed result")
            continue
        ratio = now["median"] / then["median"]
        ok = ratio <= 1.10
        failed |= not ok
        word = "PASS" if ok else "FAIL"
        was = f"{then['median']:.3f} s ({previous[-1]['date']})"
        print(f"{word} {name}: median {now['median']:.3f} s vs {was}, {ratio:.2f}x")
    return int(failed)


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--machine", help="results label [default: from the hardware and OS]")
    sub = p.add_subparsers(dest="cmd", required=True)
    for kind in ("warm", "cold"):
        s = sub.add_parser(kind)
        s.add_argument("dataset", help="label stored in the results, e.g. S1 or R1")
        s.add_argument("path")
        s.add_argument("--runs", type=int, help="timed runs [default: warm 10, cold 5]")
    sub.add_parser("report")
    sub.add_parser("check")
    args = p.parse_args()
    if args.cmd in ("warm", "cold"):
        bench(args, args.cmd)
        return 0
    return {"report": report, "check": check}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
