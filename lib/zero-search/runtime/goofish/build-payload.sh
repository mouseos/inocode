#!/usr/bin/env bash
# Build the Goofish signing payload (goofish-payload/) that ships
# next to the server binary in the release ZIP / .mcpb:
#
#   goofish-payload/
#     signer/
#       sgharness[.exe]       the Rust signing harness: it runs the
#                             app's own SecurityGuard native library
#                             under the Unicorn engine (no JVM) and
#                             serves signatures over stdio
#     syslibs/                the Android SDK23 system libraries the
#                             harness loads SecurityGuard against,
#                             extracted from the pinned
#                             unidbg-android jar (see below)
#     payload.json            what the payload is + where the server
#                             gets the Goofish app package from
#
# The Goofish APK itself is NOT part of the payload (Alibaba-
# proprietary, ~269 MB, not redistributable). The server downloads
# it on first Goofish use from the pinned app-store CDN URL in
# payload.json, verifies its SHA-256, and assembles the runtime in
# its own data directory. Users never run this script: this runs
# at release time (CI) only.
#
# Usage:
#   ./build-payload.sh [platform] [output-dir]
#     platform: linux-x64 | win32-x64 | macos-arm64
#               (default: detected from the host)
#     output-dir default: <repo>/runtime/goofish/payload-dist/<platform>
#               (the payload itself lands in <output-dir>/goofish-payload)
#
# Build-time requirements (release machines only):
#   - the signer binary: $GOOFISH_SIGNER_BIN, else the conventional
#     build output of crates/helpers/goofish-signer, else this
#     script builds it with cargo (its Unicorn build needs cmake
#     and libclang for bindgen)
#   - curl, python3
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

PLATFORM="${1:-}"
if [ -z "$PLATFORM" ]; then
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) PLATFORM="linux-x64" ;;
    Linux-aarch64) PLATFORM="linux-arm64" ;;
    Darwin-arm64) PLATFORM="macos-arm64" ;;
    Darwin-x86_64) PLATFORM="macos-x64" ;;
    MINGW*-x86_64|MSYS*-x86_64) PLATFORM="win32-x64" ;;
    *) echo "cannot detect platform; pass one explicitly" >&2; exit 1 ;;
  esac
fi
OUT_BASE="${2:-$HERE/payload-dist/$PLATFORM}"
OUT="$OUT_BASE/goofish-payload"

# ---------------------------------------------------------------------------
# Pinned inputs. The APK constants must match the provider's
# provisioning defaults (crates/providers/goofish/src/provision.rs).
# ---------------------------------------------------------------------------

APK_VERSION="7.28.70"
# Tencent app-store (应用宝) CDN, content-addressed by the file's
# MD5: this exact build (the `_yyb` distribution build whose channel
# the emulated identity uses) is served at this URL. Verified
# 2026-11-08: range GET returns 206 with the APK content type and
# the full download hashes to APK_SHA256 below.
APK_URL="https://imtt.dd.qq.com/16891/apk/361C15B2C9BC291581DBE4FF8194A899.apk"
APK_SHA256="905718aff71859527c53b80c88a971fc47c7aa5e8c1a6d3d661b9e073b837530"
APK_SIZE="268743306"
MAIN_SO="libsgmainso-6.7.260202.so"

# The Android system libraries ship inside the unidbg-android jar
# (android/sdk23/lib64/) — the same source the signing runtime has
# always used. Only the jar's library entries are used; nothing of
# unidbg itself ships. SHA-256 verified against Maven Central and
# against the library copies the harness was validated with
# (byte-identical) on 2026-11-08.
SYSLIB_JAR_URL="https://repo1.maven.org/maven2/com/github/zhkl0228/unidbg-android/0.9.9/unidbg-android-0.9.9.jar"
SYSLIB_JAR_SHA256="ce52ddda7772ac79adb4606dbaf0a65c53c41af96e73299a98eadec608f270fc"
# jar entry -> shipped name (the loader looks for libc++.so; the
# jar carries the same bytes as libcpp.so)
SYSLIBS="
android/sdk23/lib64/libc.so libc.so
android/sdk23/lib64/libm.so libm.so
android/sdk23/lib64/libz.so libz.so
android/sdk23/lib64/liblog.so liblog.so
android/sdk23/lib64/libdl.so libdl.so
android/sdk23/lib64/libcpp.so libc++.so
"

case "$PLATFORM" in
  linux-x64)   RUST_TARGET="x86_64-unknown-linux-gnu"; SIGNER_NAME="sgharness" ;;
  win32-x64)   RUST_TARGET="x86_64-pc-windows-msvc";   SIGNER_NAME="sgharness.exe" ;;
  macos-arm64) RUST_TARGET="aarch64-apple-darwin";     SIGNER_NAME="sgharness" ;;
  *)
    echo "no signer build for platform '$PLATFORM' (supported: linux-x64, win32-x64, macos-arm64)" >&2
    exit 1
    ;;
esac

sha256_of() {  # sha256sum is absent on stock macOS
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# ---------------------------------------------------------------------------
# Signer binary
# ---------------------------------------------------------------------------

SIGNER_BIN="${GOOFISH_SIGNER_BIN:-}"
if [ -z "$SIGNER_BIN" ]; then
  for cand in \
    "$ROOT/crates/helpers/goofish-signer/target/$RUST_TARGET/release/$SIGNER_NAME" \
    "$ROOT/crates/helpers/goofish-signer/target/release/$SIGNER_NAME"; do
    if [ -f "$cand" ]; then SIGNER_BIN="$cand"; break; fi
  done
fi
if [ -z "$SIGNER_BIN" ]; then
  echo "[build-payload] signer binary not found; building it with cargo"
  (cd "$ROOT" && cargo build --release --target "$RUST_TARGET" \
    --manifest-path crates/helpers/goofish-signer/Cargo.toml)
  SIGNER_BIN="$ROOT/crates/helpers/goofish-signer/target/$RUST_TARGET/release/$SIGNER_NAME"
fi
[ -f "$SIGNER_BIN" ] || { echo "signer binary not found: $SIGNER_BIN" >&2; exit 1; }
echo "[build-payload] signer: $SIGNER_BIN"

# ---------------------------------------------------------------------------
# System libraries (download the pinned jar once, verify, extract)
# ---------------------------------------------------------------------------

CACHE="$HERE/.jar-cache"
mkdir -p "$CACHE"
JAR="$CACHE/$(basename "$SYSLIB_JAR_URL")"
if [ ! -f "$JAR" ]; then
  echo "[build-payload] downloading syslib jar: $SYSLIB_JAR_URL"
  curl -sL --fail -o "$JAR.part" "$SYSLIB_JAR_URL"
  mv "$JAR.part" "$JAR"
fi
ACTUAL_SHA="$(sha256_of "$JAR")"
[ "$ACTUAL_SHA" = "$SYSLIB_JAR_SHA256" ] || {
  echo "syslib jar sha256 mismatch: expected $SYSLIB_JAR_SHA256, got $ACTUAL_SHA" >&2; exit 1; }
echo "[build-payload] syslib jar verified"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/syslibs"
python3 - "$JAR" "$WORK/syslibs" <<'PY'
import sys, zipfile
jar, outdir = sys.argv[1], sys.argv[2]
pairs = [
    ("android/sdk23/lib64/libc.so", "libc.so"),
    ("android/sdk23/lib64/libm.so", "libm.so"),
    ("android/sdk23/lib64/libz.so", "libz.so"),
    ("android/sdk23/lib64/liblog.so", "liblog.so"),
    ("android/sdk23/lib64/libdl.so", "libdl.so"),
    ("android/sdk23/lib64/libcpp.so", "libc++.so"),
]
z = zipfile.ZipFile(jar)
for entry, name in pairs:
    with z.open(entry) as src, open(f"{outdir}/{name}", "wb") as dst:
        dst.write(src.read())
    print(f"[build-payload] syslib {name} <- {entry}")
PY

# ---------------------------------------------------------------------------
# Assemble the payload
# ---------------------------------------------------------------------------

rm -rf "$OUT"
mkdir -p "$OUT/signer" "$OUT/syslibs"
cp "$SIGNER_BIN" "$OUT/signer/$SIGNER_NAME"
chmod +x "$OUT/signer/$SIGNER_NAME" 2>/dev/null || true
cp "$WORK"/syslibs/*.so "$OUT/syslibs/"

SYSLIB_NAMES="$(for f in "$OUT"/syslibs/*.so; do basename "$f"; done | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))')"
python3 - "$OUT/payload.json" "$PLATFORM" "$SIGNER_NAME" \
  "$APK_VERSION" "$APK_URL" "$APK_SHA256" "$APK_SIZE" "$MAIN_SO" "$SYSLIB_NAMES" <<'PY'
import json, sys
(path, platform, signer_name, apk_version, apk_url,
 apk_sha256, apk_size, main_so, syslib_names) = sys.argv[1:10]
doc = {
    "format": 2,
    "platform": platform,
    "signer": {"kind": "rust-sgharness", "binary": signer_name,
               "syslibs": json.loads(syslib_names)},
    "apk": {
        "version": apk_version,
        "url": apk_url,
        "sha256": apk_sha256,
        "size": int(apk_size),
    },
    "main_so": main_so,
}
with open(path, "w") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
PY

echo "[build-payload] payload assembled at: $OUT"
du -sh "$OUT" | sed 's/^/[build-payload] payload size: /'
find "$OUT" -type f | sort | sed 's/^/  /'
