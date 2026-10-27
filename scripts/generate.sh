#!/usr/bin/env bash
# Regenerates src/generated.rs from the OpenAPI document of the ControlResell public API.
#
#   scripts/generate.sh [spec]   # a file or a URL; default: https://api.controlresell.com/api/public/openapi.json
#
# The document is OpenAPI 3.1.1 and Progenitor reads 3.0.x only: scripts/prepare_spec.py rewrites it to 3.0 first
# (and drops the Idempotency-Key parameter, which the client sets itself). Writes src/generated.rs and .spec-sha256
# (of the document as served). The document itself is not committed. Needs Python 3 and a Rust toolchain.
set -euo pipefail

SPEC="${1:-https://api.controlresell.com/api/public/openapi.json}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

case "$SPEC" in
  http://*|https://*) curl -fsSL -o "$WORK/openapi.json" "$SPEC" ;;
  *) cp "$SPEC" "$WORK/openapi.json" ;;
esac

python3 "$ROOT/scripts/prepare_spec.py" "$WORK/openapi.json" "$WORK/openapi-3.0.json"
cargo run --quiet --manifest-path "$ROOT/tools/generate/Cargo.toml" --release -- "$WORK/openapi-3.0.json" "$ROOT/src/generated.rs"

shasum -a 256 "$WORK/openapi.json" | cut -d' ' -f1 > "$ROOT/.spec-sha256"
echo "Generated from $SPEC (sha256 $(cat "$ROOT/.spec-sha256"))"
