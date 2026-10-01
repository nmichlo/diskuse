#!/usr/bin/env bash
# Installs the tools bench/run.py compares against, then prints their versions.
#
# macOS: Homebrew has no version pins, so it installs the current formulae;
# run.py records each tool's version in the results. Linux: pinned to the
# versions the macOS results were taken with.
set -euo pipefail

DUMAC_REV=1ffbe3c38d1066c45cac9b4ec1e31eb74edc1076
DISKSCOUR_REV=20f39db3264a1e12ca55af2d966a090548d225fe
GDU=5.37.0
NCDU=2.9.1 # the newest static Linux build; Homebrew has 2.9.2

if [[ "$(uname)" == Darwin ]]; then
    brew install hyperfine dua-cli gdu dust parallel-disk-usage diskus ncdu
    cargo install --locked --git https://github.com/healeycodes/dumac --rev "$DUMAC_REV"
    cargo install --locked --git https://github.com/pathorsAI/diskscour --rev "$DISKSCOUR_REV"
else
    cargo install --locked hyperfine@1.20.0 dua-cli@2.45.1 du-dust@1.2.6 \
        parallel-disk-usage@0.24.0 diskus@0.9.0
    bin="$HOME/.local/bin"
    mkdir -p "$bin"
    arch="$(uname -m)"
    gdu_arch="$([[ "$arch" == aarch64 ]] && echo arm64 || echo amd64)"
    # named gdu-go, as on macOS, where plain gdu is GNU du
    curl -fsSL "https://github.com/dundee/gdu/releases/download/v$GDU/gdu_linux_$gdu_arch.tgz" |
        tar -xzO >"$bin/gdu-go"
    chmod +x "$bin/gdu-go"
    curl -fsSL "https://dev.yorhel.nl/download/ncdu-$NCDU-linux-$arch.tar.gz" | tar -xz -C "$bin"
fi

for cmd in "hyperfine --version" "dua --version" "gdu-go --version" "dust --version" \
    "pdu --version" "diskus --version" "ncdu -v"; do
    $cmd | head -1
done
cargo install --list | grep -E '^(dumac|diskscour) ' || true
