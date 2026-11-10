#!/usr/bin/env python3
"""Static guard: InoCode must not collide with a side-by-side OpenAI Codex install.

Scans the repository (production code, tests, build files) for identifiers that
would make InoCode share a process, file, port, credential or OS resource with
Codex. Wire values that the remote services check (OAuth client_id, originator,
API paths) are intentionally NOT covered here.
"""
import os
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SKIP_DIRS = {".git", "target", "node_modules", "third_party", ".venv"}
SKIP_FILES = {"LICENSE", "NOTICE", "CHANGELOG.md", "UPSTREAM.md", "check_isolation.py"}
EXTS = {".rs", ".toml", ".py", ".js", ".ts", ".json", ".yml", ".yaml", ".sh", ".ps1", ".bzl", ".md", ".snap"}

# Upstream Windows Filtering Platform keys (kernel-global objects).
UPSTREAM_WFP_GUIDS = """
2e31d31c_3948_4753_9117_e5d1a6496f41 e65054fd_4d32_4c7c_95ef_621f0cf6431a
9f5f3812_79f0_4fe9_9615_4c2c92d2f0ff 87498484_45ab_4510_845e_ece8b791b3bc
af4751de_f874_4a7b_a34d_f0d0f22d1d9b ea10db66_a928_4b2e_a82e_a376a54f93ba
83172805_f6be_4ae1_9dc6_6847aef04e7f d23b2efb_1efb_46b2_96f3_b0ccda5690c8
420b026f_9dc9_4aea_88f4_0f2b9feab39a 8d917c81_99cc_45e7_84d6_824df860cfb8
e1d6e0af_ce5f_471b_b2d3_15ca00e966f3 c2bceca4_66ef_4a0f_ba80_f4f761b8c6f0
ba10c618_84e7_4b83_8f74_36e22b2fa1ff fe7f22b8_5cf5_4adb_b2aa_71fc0a8f5d44
""".split()

RULES = [
    ("Codex env var (use INOCODE_*)", re.compile(r"\bCODEX_(?=[A-Z0-9])"), None),
    ("Codex home directory (use .inocode)", re.compile(r"(?<![\w])\.codex(?![-\w.])"), None),
    ("Codex keyring service", re.compile(r'"Codex Auth"|"Codex MCP Credentials"|KEYRING_SERVICE: &str = "codex"'), None),
    ("Codex Windows sandbox OS name", re.compile(r"(?<!InoCode)CodexSandbox|CodexLastStop|codex_wfp_|OpenAI\.CodexSandbox"), None),
    ("Codex URL scheme / bundle id", re.compile(r"\bcodex://|com\.openai\.codex"), None),
    ("Codex npm package", re.compile(r"@openai/codex"), None),
    ("Codex binary name in [[bin]]", re.compile(r'^name = "codex"$', re.M), "Cargo.toml"),
]
GUID_RE = re.compile("|".join(re.escape(g) for g in UPSTREAM_WFP_GUIDS), re.I)
# Fixed loopback ports shared with Codex (OAuth callback) may only appear in the login server.
PORT_RE = re.compile(r"\b(1455|1457)\b")
PORT_ALLOWED_PREFIX = "codex-rs/login/"


def main() -> int:
    problems = []
    for dp, dns, fns in os.walk(ROOT):
        dns[:] = [d for d in dns if d not in SKIP_DIRS]
        for fn in fns:
            p = pathlib.Path(dp) / fn
            if fn in SKIP_FILES or p.suffix not in EXTS:
                continue
            try:
                text = p.read_text(encoding="utf-8")
            except Exception:
                continue
            rel = pathlib.PurePosixPath(p.relative_to(ROOT).as_posix())
            for label, rx, only in RULES:
                if only and p.name != only:
                    continue
                for m in rx.finditer(text):
                    line = text.count("\n", 0, m.start()) + 1
                    problems.append(f"{rel}:{line}: {label}: {m.group(0)!r}")
            for m in GUID_RE.finditer(text):
                line = text.count("\n", 0, m.start()) + 1
                problems.append(f"{rel}:{line}: upstream WFP GUID reused: {m.group(0)}")
            if p.suffix == ".rs" and not str(rel).startswith(PORT_ALLOWED_PREFIX):
                for m in PORT_RE.finditer(text):
                    line = text.count("\n", 0, m.start()) + 1
                    ctx = text[max(0, m.start() - 20): m.end() + 20].replace("\n", " ")
                    if "port" in ctx.lower() or "127.0.0.1" in ctx or "localhost" in ctx:
                        problems.append(f"{rel}:{line}: fixed port shared with Codex: {ctx.strip()!r}")
    for item in problems:
        print(item)
    print(f"isolation check: {len(problems)} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())

