#!/usr/bin/env bash
# NOTE: legacy/manual route. Release installs no longer assemble
# the runtime this way — they ship a goofish-payload/ (see
# build-payload.sh) and the server assembles the runtime itself on
# first use (see README.md in this directory). This script remains
# for development and payload-less deployments.
#
# Assemble a Goofish signing-runtime bundle (goofish-runtime/) from
# the vendored harness sources in ./harness.
#
# Why this exists: Goofish MTOP requests must be signed by Alibaba's
# SecurityGuard native library. The server runs that library under
# unidbg (a JVM Android emulator) — the "SG serve" harness — and
# auto-discovers the assembled bundle next to the server executable
# (or in the server's data directory), starting it on first use.
# The device identity needs no assembly at all: the server
# generates and persists it itself.
#
# What cannot be shipped in this repository (and therefore must be
# supplied once, at assembly time):
#   - the Goofish Android APK (idlefish 7.28.70): ~269 MB and
#     Alibaba-proprietary, so it is not redistributable here. The
#     harness validates it and extracts its SecurityGuard .so
#     libraries from it.
#   - a JDK 8 and Maven, used only during assembly.
#
# Usage:
#   ./bootstrap.sh /path/to/idlefish_7.28.70.apk [output-dir]
# Default output-dir: ../../target/release/goofish-runtime when run
# from runtime/goofish (next to the built server), else
# ./goofish-runtime.
set -euo pipefail

APK="${1:?usage: bootstrap.sh /path/to/idlefish.apk [output-dir]}"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${2:-}"
if [ -z "$OUT" ]; then
  if [ -d "$HERE/../../target/release" ]; then
    OUT="$HERE/../../target/release/goofish-runtime"
  else
    OUT="$HERE/goofish-runtime"
  fi
fi
[ -f "$APK" ] || { echo "APK not found: $APK" >&2; exit 1; }

# Locate a JDK 8 (the harness's unidbg build needs a real JDK 8).
# An explicit JAVA_HOME wins as-is; otherwise PATH and the usual
# install locations are searched for a java whose version reports
# 1.8. $HOME/tools/jdk8 is still in the list — as one fallback among
# several, not as the assumed default.
is_jdk8() {
  [ -x "$1/bin/java" ] || return 1
  "$1/bin/java" -version 2>&1 | head -n 1 | grep -q 'version "1\.8'
}
find_jdk8() {
  if [ -n "${JAVA_HOME:-}" ]; then
    if [ -x "$JAVA_HOME/bin/java" ]; then
      is_jdk8 "$JAVA_HOME" || echo "warning: JAVA_HOME ($JAVA_HOME) is not a JDK 8; the harness is built for JDK 8 and may fail." >&2
      echo "$JAVA_HOME"
      return 0
    fi
    return 1
  fi
  local cand=""
  if command -v java >/dev/null 2>&1; then
    cand="$(dirname "$(dirname "$(readlink -f "$(command -v java)" 2>/dev/null)" 2>/dev/null)" 2>/dev/null)"
    if [ -n "$cand" ] && is_jdk8 "$cand"; then
      echo "$cand"
      return 0
    fi
  fi
  for cand in /usr/lib/jvm/*8* /usr/lib/jvm/* /opt/*jdk* /opt/* \
              "$HOME/.jdks/"*8* "$HOME/tools/jdk8"; do
    [ -d "$cand" ] || continue
    if is_jdk8 "$cand"; then
      echo "$cand"
      return 0
    fi
  done
  return 1
}
JAVA_HOME="$(find_jdk8)" || {
  echo "JDK 8 not found (searched: JAVA_HOME, PATH, /usr/lib/jvm, /opt, ~/.jdks, ~/tools/jdk8)." >&2
  echo "Install a JDK 8 or set JAVA_HOME to one." >&2
  exit 1
}
export JAVA_HOME
echo "Using JAVA_HOME=$JAVA_HOME"
command -v mvn >/dev/null || { echo "mvn not found on PATH" >&2; exit 1; }

mkdir -p "$OUT/harness" "$OUT/libs" "$OUT/rootfs"

echo "[1/4] Resolving harness dependencies (unidbg 0.9.9, Maven Central)"
cp "$HERE/harness/pom.xml" "$OUT/harness/pom.xml"
cp -r "$HERE/harness/src" "$OUT/harness/src"
cp -r "$HERE/harness/patched" "$OUT/harness/patched"
(cd "$OUT/harness" && JAVA_HOME="$JAVA_HOME" mvn -q -o dependency:copy-dependencies \
  -DoutputDirectory="$OUT/m2repo" 2>/dev/null || \
  JAVA_HOME="$JAVA_HOME" mvn -q dependency:copy-dependencies -DoutputDirectory="$OUT/m2repo")

echo "[2/4] Compiling SgHarness"
(cd "$OUT/harness" && JAVA_HOME="$JAVA_HOME" mvn -q compile)

echo "[3/4] Extracting SecurityGuard libraries from the APK"
python3 - "$APK" "$OUT/libs" <<'PY'
import sys, zipfile
apk, out = sys.argv[1], sys.argv[2]
wanted = ("libsgmainso", "libsgmain.so", "libsgcore", "libsgmisc", "libsgnocaptcha")
with zipfile.ZipFile(apk) as z:
    for name in z.namelist():
        if "/arm64-v8a/" in name and any(w in name for w in wanted):
            data = z.read(name)
            dest = f"{out}/{name.rsplit('/', 1)[-1]}"
            open(dest, "wb").write(data)
            print("  extracted", dest)
PY
cp "$APK" "$OUT/goofish.apk"

echo "[4/4] Writing serve.sh"
cat > "$OUT/serve.sh" <<'SH'
#!/bin/bash
# ZeroSearch Goofish signing runtime (assembled by bootstrap.sh).
# Starts SgHarness in JSON-lines serve mode on stdio.
set -u
cd "$(dirname "$0")"
export JAVA_HOME="${JAVA_HOME:-__BAKED_JAVA_HOME__}"
# If the JDK 8 resolved at assembly time is no longer there, fall
# back to the usual install locations (same search as bootstrap.sh).
if [ ! -x "$JAVA_HOME/bin/java" ]; then
  for cand in /usr/lib/jvm/*8* /usr/lib/jvm/* "$HOME/.jdks/"*8* "$HOME/tools/jdk8"; do
    if [ -x "$cand/bin/java" ] && "$cand/bin/java" -version 2>&1 | head -n 1 | grep -q 'version "1\.8'; then
      JAVA_HOME="$cand"
      break
    fi
  done
  export JAVA_HOME
fi
R="$(pwd)/m2repo"
CP="harness/patched/classes:harness/target/classes"
for j in \
  $R/unidbg-android-0.9.9.jar $R/unidbg-api-0.9.9.jar \
  $R/unidbg-unicorn2-0.9.9.jar $R/unidbg-dynarmic-0.9.9.jar \
  $R/unidbg-hypervisor-0.9.9.jar $R/unidbg-kvm-0.9.9.jar \
  $R/unicorn-1.0.15.jar $R/capstone-3.1.8.jar $R/keystone-0.9.7.jar \
  $R/demumble-1.0.4.jar $R/apk-parser-2.6.10.jar $R/fastjson-1.2.83.jar \
  $R/commons-codec-1.21.0.jar $R/commons-collections4-4.5.0.jar \
  $R/commons-io-2.21.0.jar $R/slf4j-api-1.7.36.jar $R/jna-5.10.0.jar \
  $R/native-lib-loader-2.3.5.jar $R/slf4j-simple-1.7.36.jar \
  ; do CP="$CP:$j"; done
exec "$JAVA_HOME/bin/java" -Dapk="$(pwd)/goofish.apk" -Xmx512m -cp "$CP" \
  local.goofish.SgHarness serve 2>>"$(pwd)/serve_stderr.log"
SH
# Bake the JDK 8 resolved at assembly time into serve.sh as its
# default JAVA_HOME (still overridable via the environment).
sed "s|__BAKED_JAVA_HOME__|$JAVA_HOME|" "$OUT/serve.sh" > "$OUT/serve.sh.tmp"
mv "$OUT/serve.sh.tmp" "$OUT/serve.sh"
chmod +x "$OUT/serve.sh"

echo
echo "Bundle assembled at: $OUT"
echo "Place it next to the zero-search-mcp binary (or in the server's"
echo "data directory) — the server discovers and starts it on its own."
echo "The first Goofish call takes a few minutes while the harness boots."
echo "Verify the bundle's contents with: $HERE/verify-bundle.sh \"$OUT\""
