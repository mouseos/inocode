#!/usr/bin/env python3
"""Smoke test: a built InoCode binary must not touch Codex state.

usage: smoke_isolation.py <path-to-inocode-binary>
"""
import hashlib
import os
import pathlib
import re
import subprocess
import sys
import tempfile


def snapshot(root: pathlib.Path):
    out = {}
    for p in sorted(root.rglob("*")):
        if p.is_file():
            out[str(p.relative_to(root))] = hashlib.sha256(p.read_bytes()).hexdigest()
    return out


def run(binary, args, env):
    return subprocess.run([binary, *args], env=env, capture_output=True, text=True, timeout=120)


def main() -> int:
    binary = sys.argv[1]
    failures = []
    with tempfile.TemporaryDirectory(prefix="inocode-smoke-") as tmp:
        tmp = pathlib.Path(tmp)
        home = tmp / "home"
        decoy_env_home = tmp / "decoy-codex-home"
        (home / ".codex").mkdir(parents=True)
        decoy_env_home.mkdir()
        # Decoy Codex state that InoCode must neither read nor modify.
        (home / ".codex" / "auth.json").write_text('{"OPENAI_API_KEY":"sk-decoy-not-for-inocode"}')
        (home / ".codex" / "config.toml").write_text('model = "decoy-codex-model"\n')
        (decoy_env_home / "auth.json").write_text('{"OPENAI_API_KEY":"sk-decoy-env"}')

        env = {k: v for k, v in os.environ.items() if not k.startswith(("CODEX_", "INOCODE_"))}
        env.update(
            HOME=str(home),
            USERPROFILE=str(home),
            APPDATA=str(home / "AppData" / "Roaming"),
            LOCALAPPDATA=str(home / "AppData" / "Local"),
            CODEX_HOME=str(decoy_env_home),
            CODEX_API_KEY="sk-decoy-env-key",
            OPENAI_API_KEY="",
        )
        before_codex = snapshot(home / ".codex")
        before_env = snapshot(decoy_env_home)

        version = run(binary, ["--version"], env)
        helptext = run(binary, ["--help"], env)
        status = run(binary, ["login", "status"], env)
        features = run(binary, ["features", "list"], env)

        if version.returncode != 0 or "inocode" not in version.stdout.lower():
            failures.append(f"--version should mention inocode: {version.stdout!r} {version.stderr!r}")
        if re.search(r"codex", version.stdout, re.I):
            failures.append(f"--version output still mentions codex: {version.stdout!r}")
        if re.search(r"\bcodex\b", helptext.stdout, re.I):
            lines = [l for l in helptext.stdout.splitlines() if re.search(r"\bcodex\b", l, re.I)]
            failures.append("--help still mentions codex: " + " | ".join(lines[:5]))
        combined = (status.stdout + status.stderr).lower()
        if "decoy" in combined or "logged in using" in combined:
            failures.append(f"login status picked up Codex credentials: {combined!r}")
        if features.returncode != 0:
            failures.append(f"features list failed: {features.stderr[-300:]!r}")
        if "decoy-codex-model" in (features.stdout + features.stderr):
            failures.append("config from ~/.codex leaked into InoCode")

        if snapshot(home / ".codex") != before_codex:
            failures.append("~/.codex was modified by inocode")
        if snapshot(decoy_env_home) != before_env:
            failures.append("CODEX_HOME directory was modified by inocode")
        if (home / ".inocode").exists():
            print("note: inocode created", (home / ".inocode"))
        else:
            print("note: inocode did not create ~/.inocode for these commands")

    for f in failures:
        print("FAIL:", f)
    print(f"smoke isolation: {len(failures)} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())

