#!/usr/bin/env bash
# Build the rustgd-viewer binary and place it in inst/bin/.
set -e

cd "$(dirname "$0")/.."
mkdir -p inst/bin
cd src/rust
cargo build --release --bin rustgd-viewer
cp target/release/rustgd-viewer ../../inst/bin/rustgd-viewer
echo "rustgd-viewer built and copied to inst/bin/"
