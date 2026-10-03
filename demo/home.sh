#!/usr/bin/env bash
# Builds the made-up home folder demo.tape browses, so the demo shows no
# real paths. Files hold real bytes, as diskuse counts allocated sizes.
set -euo pipefail
root="${1:-/tmp/diskuse-demo/me}"
rm -rf "$root" && mkdir -p "$root"
cd "$root"

# file PATH MiB
file() { mkdir -p "$(dirname "$1")"; dd if=/dev/zero of="$1" bs=1048576 count="$2" status=none; }

file "Movies/holiday.mov" 120
file "Downloads/installer.dmg" 95
file "Downloads/photos.zip" 40
file "Documents/report.pdf" 4
file "Documents/notes.txt" 1
file ".Trash/old.iso" 60
file "Library/Caches/Homebrew/downloads.tar" 55
file "Library/Caches/com.apple.Safari/cache.db" 30
file "Library/Developer/Xcode/DerivedData/App-abc/Build.db" 110
file "Library/Developer/CoreSimulator/Devices/iPhone/data.img" 85
file "Library/Application Support/MobileSync/Backup/0001/backup.db" 70
file "projects/webapp/node_modules/react/index.js" 18
file "projects/webapp/node_modules/typescript/lib.js" 26
file "projects/webapp/node_modules/esbuild/bin" 12
file "projects/webapp/.next/cache/webpack.pack" 22
file "projects/webapp/src/app.ts" 1
file "projects/webapp/package.json" 1
file "projects/engine/target/debug/engine" 64
file "projects/engine/src/main.rs" 1
file "projects/engine/Cargo.toml" 1
file "projects/ml/.venv/lib/torch.so" 48
file "projects/ml/.venv/pyvenv.cfg" 1
file "projects/ml/data/weights.bin" 90
echo "$root"
