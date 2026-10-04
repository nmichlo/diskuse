# everything CI checks, in one command
check:
    uvx --from pre-commit pre-commit run --all-files
    cargo test
    uv run --quiet --extra test pytest crates/diskuse-python/tests -q
    cargo deny check
    cargo about generate --locked --workspace about.hbs | diff -u THIRD_PARTY_LICENSES.txt -

# rewrite THIRD_PARTY_LICENSES.txt, after a dependency change. needs
# cargo-about, see about.toml
licenses:
    cargo about generate --locked --workspace about.hbs > THIRD_PARTY_LICENSES.txt

# benchmarks, see bench/README.md

# make dataset ID (S1 to S6) in DIR
bench-gen ID DIR:
    cargo run --release -p diskuse-core --example bench-gen -- {{ID}} {{DIR}}

# warm runs, e.g. `just bench S1 /tmp/diskuse-bench/S1 --runs 2`
bench DATASET PATH *ARGS:
    cargo build --release
    python3 bench/run.py warm {{DATASET}} {{PATH}} {{ARGS}}

# needs sudo, to drop the file cache before each run
bench-cold DATASET PATH *ARGS:
    cargo build --release
    python3 bench/run.py cold {{DATASET}} {{PATH}} {{ARGS}}

# README table and gate results from bench/results/<machine>
bench-report:
    python3 bench/run.py report

# fails if diskuse got > 10% slower than the last committed result
bench-check:
    python3 bench/run.py check

# warm runs on Linux in an OrbStack machine
bench-orb *ARGS:
    bench/orb.sh {{ARGS}}

# store the screens the browser draws now as the expected ones, for this
# platform (crates/diskuse/tests/snapshots); review the diff before committing
snapshots:
    INSTA_UPDATE=always cargo test --test browse

# re-record the README demo GIF (needs `brew install vhs`)
demo:
    cargo build --release
    demo/home.sh
    PATH="$PWD/target/release:$PATH" vhs demo/demo.tape
