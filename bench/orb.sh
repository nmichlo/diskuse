#!/usr/bin/env bash
# Runs the warm suite on Linux, in an OrbStack machine, from macOS:
#
#   bench/orb.sh [--runs N] [S1 S2 ...]    (default: S1 to S5)
#
# The source, the build and the datasets live on the machine's own disk,
# not the shared macOS home, which would benchmark the file sharing.
set -euo pipefail

MACHINE=diskuse-bench
SRC=/var/tmp/diskuse-src
DATA=/var/tmp/diskuse-bench
RUNS=""
if [[ "${1:-}" == --runs ]]; then
    RUNS="--runs $2"
    shift 2
fi
DATASETS="${*:-S1 S2 S3 S4 S5}"
# e.g. "orb-m4-ubuntu": the host chip, never the host name
chip="$(sysctl -n machdep.cpu.brand_string | sed 's/^Apple //' | tr -d ' ' | tr '[:upper:]' '[:lower:]')"
LABEL="orb-$chip-ubuntu"
cd "$(dirname "$0")/.."

orb list -q | grep -qx "$MACHINE" || orb create ubuntu:noble "$MACHINE"
vm() { orb -m "$MACHINE" bash -c "$1"; }

# a copy of the working tree, uncommitted changes included
git ls-files -z --cached --others --exclude-standard |
    tar -czf - --null -T - |
    vm "rm -rf $SRC && mkdir -p $SRC && tar -xzf - -C $SRC"

vm "set -e
command -v cc >/dev/null && command -v /usr/bin/time >/dev/null ||
    { sudo apt-get update -q && sudo apt-get install -yq build-essential curl git python3 time; }
command -v cargo >/dev/null || [ -x ~/.cargo/bin/cargo ] ||
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
. ~/.cargo/env
export PATH=\$HOME/.local/bin:\$PATH
command -v pdu >/dev/null || $SRC/bench/install.sh
cd $SRC
cargo build --release -p diskuse -p diskuse-core --bin diskuse --example bench-gen
for id in $DATASETS; do
    [ -d $DATA/\$id ] || target/release/examples/bench-gen \$id $DATA/\$id
    python3 bench/run.py --machine $LABEL warm \$id $DATA/\$id $RUNS
done
python3 bench/run.py --machine $LABEL report || true"

mkdir -p "bench/results/$LABEL"
vm "tar -czf - -C $SRC/bench/results/$LABEL ." | tar -xzf - -C "bench/results/$LABEL"
echo "results in bench/results/$LABEL"
