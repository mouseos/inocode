#!/usr/bin/env bash
# Pack MCPB bundles for each platform (ported from Baidu_MCP's
# scripts/pack-mcpb.sh so all search MCPs share the same packing shape;
# bundle file naming follows this repo's server.json / release.yml:
# zero-search-mcp-v<version>-<win32|macos|linux>-<arch>.mcpb).
# Usage: ./scripts/pack-mcpb.sh [version]
# Uses cross-compiled binaries in target/<triple>/release/ when present,
# falling back to the local target/release build for the host platform.
set -euo pipefail

VERSION="${1:-0.1.0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist"
rm -rf "$DIST"
mkdir -p "$DIST"

# triple bundle_platform arch exe manifest_platform helper_exe
TARGETS=(
  "x86_64-pc-windows-msvc win32 x64 zero-search-mcp.exe win32 zero-search-yahoo-us-image-helper.exe"
  "aarch64-apple-darwin macos arm64 zero-search-mcp darwin zero-search-yahoo-us-image-helper"
  "x86_64-unknown-linux-gnu linux x64 zero-search-mcp linux zero-search-yahoo-us-image-helper"
)

RUSTC="$(command -v rustc || echo "$HOME/.cargo/bin/rustc")"
HOST_TRIPLE="$("$RUSTC" -vV 2>/dev/null | sed -n 's/^host: //p' || true)"
if [ -z "$HOST_TRIPLE" ]; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) HOST_TRIPLE="x86_64-unknown-linux-gnu" ;;
    Linux-aarch64) HOST_TRIPLE="aarch64-unknown-linux-gnu" ;;
    Darwin-arm64) HOST_TRIPLE="aarch64-apple-darwin" ;;
    Darwin-x86_64) HOST_TRIPLE="x86_64-apple-darwin" ;;
    MINGW*-x86_64|MSYS*-x86_64) HOST_TRIPLE="x86_64-pc-windows-msvc" ;;
    *) HOST_TRIPLE="unknown" ;;
  esac
fi

# Refresh manifest.json's tool list and version from a locally built
# server before packing, so a bundle can never ship a stale list
# (the manifest is generated from the server's tools/list; see
# scripts/generate-manifest.py). A cross-only pack with no local
# server binary falls back to the committed manifest with a warning;
# CI additionally guards freshness via generate-manifest.py --check.
GEN_SERVER=""
for cand in \
  "$ROOT/target/release/zero-search-mcp" \
  "$ROOT/target/release/zero-search-mcp.exe" \
  "$ROOT/target/$HOST_TRIPLE/release/zero-search-mcp" \
  "$ROOT/target/$HOST_TRIPLE/release/zero-search-mcp.exe" \
  "$ROOT/target/debug/zero-search-mcp"; do
  if [ -f "$cand" ]; then
    GEN_SERVER="$cand"
    break
  fi
done
if [ -n "$GEN_SERVER" ] && command -v python3 >/dev/null 2>&1; then
  python3 "$ROOT/scripts/generate-manifest.py" --server "$GEN_SERVER" --manifest "$ROOT/manifest.json"
else
  echo "[WARN] No local server binary found; packing with the committed manifest.json as-is."
fi

# Packed-platform hashes are collected here and written into
# server.json at the end (fileSha256 per platform).
HASHES_FILE="$(mktemp)"

for row in "${TARGETS[@]}"; do
  read -r triple platform arch exe manifest_platform helper_exe <<<"$row"
  bin="$ROOT/target/$triple/release/$exe"
  helper_bin="$ROOT/target/$triple/release/$helper_exe"
  if [ ! -f "$bin" ]; then
    # Fall back to the plain target/release build ONLY when it is a build
    # for this very target (i.e. the host itself). Never relabel a binary
    # built for another platform.
    local_bin="$ROOT/target/release/$exe"
    if [ "$triple" = "$HOST_TRIPLE" ] && [ -f "$local_bin" ]; then
      bin="$local_bin"
      helper_bin="$ROOT/target/release/$helper_exe"
      echo "[INFO] Using local release build for $platform-$arch"
    else
      echo "[SKIP] Binary not found for $triple: $bin"
      continue
    fi
  fi
  if [ ! -f "$helper_bin" ]; then
    # The helper is a standalone crate with its own target directory.
    for alt in \
      "$ROOT/crates/helpers/yahoo-us-image/target/$triple/release/$helper_exe" \
      "$ROOT/crates/helpers/yahoo-us-image/target/release/$helper_exe"; do
      if [ -f "$alt" ]; then
        helper_bin="$alt"
        break
      fi
    done
  fi
  if [ ! -f "$helper_bin" ]; then
    echo "[SKIP] Helper binary not found for $triple: $helper_bin"
    continue
  fi

  name="zero-search-mcp-v$VERSION-$platform-$arch"
  stage="$DIST/$name-stage"
  mkdir -p "$stage/server"
  cp "$bin" "$stage/server/$exe"
  cp "$helper_bin" "$stage/server/$helper_exe"

  # Goofish signing payload (JRE + compiled harness), built by
  # runtime/goofish/build-payload.sh. It ships next to the server
  # binary; the server assembles the actual runtime from it on
  # first Goofish use.
  payload_src="$ROOT/runtime/goofish/payload-dist/$platform-$arch/goofish-payload"
  if [ -d "$payload_src" ]; then
    cp -r "$payload_src" "$stage/server/goofish-payload"
  else
    echo "[WARN] Goofish payload not found for $platform-$arch: $payload_src (bundle will lack the Goofish signing runtime)"
  fi

  # manifest.json with platform-specific entry_point / command / platforms
  python3 - "$ROOT/manifest.json" "$stage/manifest.json" "$VERSION" "$exe" "$manifest_platform" <<'PY'
import json, sys
src, dst, version, exe, platform = sys.argv[1:6]
m = json.load(open(src))
m["version"] = version
m["server"]["entry_point"] = f"server/{exe}"
m["server"]["mcp_config"]["command"] = f"server/{exe}"
m["compatibility"]["platforms"] = [platform]
json.dump(m, open(dst, "w"), ensure_ascii=False, indent=2)
PY

  (cd "$stage" && zip -qr "$DIST/$name.mcpb" .)
  rm -rf "$stage"
  hash="$(sha256sum "$DIST/$name.mcpb" | cut -d' ' -f1)"
  size="$(stat -c%s "$DIST/$name.mcpb")"
  echo "[OK] $name.mcpb ($size bytes, sha256: $hash)"
  echo "$manifest_platform $hash" >> "$HASHES_FILE"
done

# Write the packed platforms' hashes (and the version) into
# server.json so the registry metadata always matches the bundles
# this run produced. Platforms not packed in this run keep their
# previous fileSha256.
if [ -s "$HASHES_FILE" ] && command -v python3 >/dev/null 2>&1; then
  python3 - "$ROOT/server.json" "$VERSION" "$HASHES_FILE" <<'PY'
import json, re, sys
path, version, hashes_file = sys.argv[1:4]
hashes = {}
for line in open(hashes_file):
    parts = line.split()
    if len(parts) == 2:
        hashes[parts[0]] = parts[1]
doc = json.load(open(path))
doc["version"] = version
for pkg in doc.get("packages", []):
    plats = pkg.get("supported_platforms") or []
    if plats and plats[0] in hashes:
        pkg["fileSha256"] = hashes[plats[0]]
    ident = pkg.get("identifier", "")
    ident = re.sub(r"/download/v[^/]+/", f"/download/v{version}/", ident)
    ident = re.sub(r"-v\d+\.\d+\.\d+-", f"-v{version}-", ident)
    pkg["identifier"] = ident
json.dump(doc, open(path, "w"), ensure_ascii=False, separators=(",", ":"))
print(f"server.json updated: version {version}, fileSha256 for {sorted(hashes)}")
PY
fi
rm -f "$HASHES_FILE"

echo
echo "Done. Bundles are in: $DIST"
echo "server.json has been updated with the hashes above for the packed platforms."
