#!/usr/bin/env bash
# Renders the app's views in their preview states to PNGs (default: build/previews).
# Compiles MacRDPKit + the app's views + Tools/PreviewRenderer into a throwaway tool and runs
# it. It never starts the app or the helper.
set -Eeuo pipefail

readonly APP_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
readonly OUT="${1:-$APP_DIR/../build/previews}"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT
mkdir -p -- "$OUT"

readonly TARGET="arm64-apple-macos26.0"
xcrun swiftc -target "$TARGET" -swift-version 6 -parse-as-library -O \
  -module-name MacRDPKit -emit-module -emit-module-path "$work/MacRDPKit.swiftmodule" \
  -emit-library -static -o "$work/libMacRDPKit.a" \
  "$APP_DIR"/MacRDPKit/Sources/MacRDPKit/*.swift

app_sources=()
while IFS= read -r -d '' f; do app_sources+=("$f"); done \
  < <(find "$APP_DIR/App" -name '*.swift' ! -name 'MacRDPApp.swift' -print0)

xcrun swiftc -target "$TARGET" -swift-version 6 -module-name PreviewRenderer \
  -I "$work" -L "$work" -lMacRDPKit -o "$work/render" \
  "${app_sources[@]}" "$APP_DIR/Tools/PreviewRenderer/main.swift"

"$work/render" "$OUT"
