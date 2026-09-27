#!/usr/bin/env bash
# Build portable Linux x86-64 helpers for Windows/Linux/macOS SSH clients.
set -euo pipefail
repo="$(cd -- "$(dirname -- "$0")/.." && pwd)"
destination="${1:-$repo/target/remote-tools}"
command -v musl-gcc >/dev/null || { echo 'Install musl-tools first (Ubuntu/Debian: sudo apt-get install musl-tools).' >&2; exit 1; }
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
rustup target add --toolchain stable x86_64-unknown-linux-musl
RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-D warnings" cargo +stable build --locked --release \
    --manifest-path "$repo/Cargo.toml" --target-dir "$repo/target/remote-build" \
    --target x86_64-unknown-linux-musl -p forge-agent -p forge-server
mkdir -p "$destination"
for binary in forge-agent forge-server; do
    source="$repo/target/remote-build/x86_64-unknown-linux-musl/release/$binary"
    "$source" --version
    install -m 755 "$source" "$destination/$binary-x86_64"
done
echo "SSH helpers: $destination"
