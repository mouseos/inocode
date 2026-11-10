#!/usr/bin/env bash
# Verify an assembled Goofish signing-runtime bundle (static checks).
#
# bootstrap.sh assembles the bundle out of band (user-supplied APK,
# Maven downloads, native library extraction), so a broken or
# partial assembly otherwise surfaces only minutes into the first
# Goofish call, as a harness boot failure. This script checks the
# bundle's contents against the layout bootstrap.sh produces and
# serve.sh expects — without starting the harness:
#
#   - serve.sh exists and is executable
#   - goofish.apk is a valid zip carrying the arm64 SecurityGuard
#     libraries the harness loads
#   - the compiled SgHarness class and the patched classes exist
#   - every jar on serve.sh's classpath exists under m2repo/
#   - libs/ holds the extracted SecurityGuard .so set
#   - rootfs/ exists
#   - a JDK 8 is resolvable the same way serve.sh resolves it
#     (environment, the path baked in at assembly time, then the
#     usual install locations)
#
# Usage: ./verify-bundle.sh [bundle-dir]
# Default bundle-dir: $GOOFISH_RUNTIME_DIR, else ./goofish-runtime.
# Exit status: 0 when every check passes, 1 otherwise.
set -u

BUNDLE="${1:-${GOOFISH_RUNTIME_DIR:-./goofish-runtime}}"
FAIL=0
ok()  { echo "[OK]   $1"; }
bad() { echo "[FAIL] $1"; FAIL=1; }

echo "Verifying Goofish runtime bundle: $BUNDLE"

if [ -d "$BUNDLE" ]; then
  ok "bundle directory exists"
else
  bad "bundle directory not found: $BUNDLE"
  echo
  echo "Verification FAILED (bundle directory missing)."
  exit 1
fi
BUNDLE="$(cd "$BUNDLE" && pwd)"

# ── serve.sh ──
if [ -f "$BUNDLE/serve.sh" ]; then
  ok "serve.sh exists"
  if [ -x "$BUNDLE/serve.sh" ]; then
    ok "serve.sh is executable"
  else
    bad "serve.sh is not executable (chmod +x it)"
  fi
else
  bad "serve.sh missing"
fi

# ── goofish.apk ──
if [ -f "$BUNDLE/goofish.apk" ]; then
  ok "goofish.apk exists ($(du -h "$BUNDLE/goofish.apk" | cut -f1))"
  if python3 - "$BUNDLE/goofish.apk" <<'PY'
import sys, zipfile
try:
    z = zipfile.ZipFile(sys.argv[1])
except Exception:
    sys.exit(1)
names = z.namelist()
sys.exit(0 if any("/arm64-v8a/" in n and "libsgmain" in n for n in names) else 1)
PY
  then
    ok "goofish.apk is a zip carrying arm64 SecurityGuard libraries"
  else
    bad "goofish.apk is not a readable zip with arm64 libsgmain (wrong or truncated APK?)"
  fi
else
  bad "goofish.apk missing (bootstrap.sh copies the supplied APK into the bundle)"
fi

# ── Harness classes ──
if [ -f "$BUNDLE/harness/target/classes/local/goofish/SgHarness.class" ]; then
  ok "compiled SgHarness class exists"
else
  bad "harness/target/classes/local/goofish/SgHarness.class missing (assembly step 2 did not complete)"
fi
if [ -d "$BUNDLE/harness/patched/classes" ]; then
  ok "harness/patched/classes exists"
else
  bad "harness/patched/classes missing (the vendored patched classes were not copied)"
fi

# ── Classpath jars ──
if [ -f "$BUNDLE/serve.sh" ]; then
  missing=0
  for jar in $(grep -o '\$R/[A-Za-z0-9._-]*\.jar' "$BUNDLE/serve.sh" | sed 's|^\$R/||' | sort -u); do
    if [ ! -f "$BUNDLE/m2repo/$jar" ]; then
      bad "classpath jar missing: m2repo/$jar"
      missing=1
    fi
  done
  [ "$missing" -eq 0 ] && ok "every jar on serve.sh's classpath exists under m2repo/"
fi

# ── SecurityGuard libraries ──
for pat in 'libsgmain*.so' 'libsgcore*.so' 'libsgmisc*.so' 'libsgnocaptcha*.so'; do
  # shellcheck disable=SC2086
  if ls "$BUNDLE/libs/"$pat >/dev/null 2>&1; then
    ok "libs/ contains $pat"
  else
    bad "libs/ has no $pat (APK extraction step did not produce it)"
  fi
done

# ── rootfs ──
if [ -d "$BUNDLE/rootfs" ]; then
  ok "rootfs/ exists"
else
  bad "rootfs/ missing (the harness manages it, but bootstrap.sh creates it)"
fi

# ── JDK 8 resolution (same rules as serve.sh) ──
is_jdk8() {
  [ -x "$1/bin/java" ] || return 1
  "$1/bin/java" -version 2>&1 | head -n 1 | grep -q 'version "1\.8'
}
jdk_found=""
baked=""
if [ -f "$BUNDLE/serve.sh" ]; then
  baked="$(sed -n 's/^export JAVA_HOME="${JAVA_HOME:-\(.*\)}"$/\1/p' "$BUNDLE/serve.sh" | head -n 1)"
fi
for cand in "${JAVA_HOME:-}" "$baked" /usr/lib/jvm/*8* /usr/lib/jvm/* \
            "$HOME/.jdks/"*8* "$HOME/tools/jdk8"; do
  [ -n "$cand" ] && [ -d "$cand" ] || continue
  if is_jdk8 "$cand"; then
    jdk_found="$cand"
    break
  fi
done
if [ -n "$jdk_found" ]; then
  ok "JDK 8 resolvable for serve.sh: $jdk_found"
else
  bad "no JDK 8 found (checked JAVA_HOME, the path baked into serve.sh, /usr/lib/jvm, ~/.jdks, ~/tools/jdk8)"
fi

echo
if [ "$FAIL" -eq 0 ]; then
  echo "Verification PASSED. Place the bundle next to the zero-search-mcp"
  echo "binary (or in the server's data directory); the server starts it"
  echo "on the first Goofish call, which takes a few minutes."
else
  echo "Verification FAILED. Re-run bootstrap.sh to reassemble the bundle,"
  echo "or fix the items marked [FAIL] above."
fi
exit "$FAIL"
