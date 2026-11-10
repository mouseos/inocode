#!/usr/bin/env python3
"""Regenerate manifest.json's tool list and version from the server.

The MCPB manifest used to be maintained by hand and silently went
stale (it shipped a v0.1.0 tool list in the v0.3.x bundles). The
server's own `tools/list` is the single source of truth, so this
script asks a built server binary over stdio and rewrites:

  - "tools":            [{name, description}, ...] in tools/list order
  - "tools_generated":  true
  - "version":          the server's reported version (initialize)

Everything else in manifest.json (descriptions, keywords, server
block, ...) is editorial and left untouched.

Usage:
  scripts/generate-manifest.py [--server PATH] [--manifest PATH]
                               [--check]

  --server PATH    server binary to query
                   (default: target/release/zero-search-mcp, falling
                   back to target/debug/zero-search-mcp)
  --manifest PATH  manifest to update (default: manifest.json next
                   to this script's repository root)
  --check          write nothing; exit 1 if the manifest is stale
                   (for CI)

Requires Python 3.8+; no third-party packages.
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def default_server() -> Path:
    for cand in (
        ROOT / "target" / "release" / "zero-search-mcp",
        ROOT / "target" / "debug" / "zero-search-mcp",
        ROOT / "target" / "release" / "zero-search-mcp.exe",
        ROOT / "target" / "debug" / "zero-search-mcp.exe",
    ):
        if cand.is_file():
            return cand
    return ROOT / "target" / "release" / "zero-search-mcp"


def query_server(server: Path) -> tuple[str, list[dict]]:
    """Run the server, send initialize + tools/list, parse replies."""
    requests = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "generate-manifest", "version": "1"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
    ]
    payload = "".join(json.dumps(r) + "\n" for r in requests)
    proc = subprocess.run(
        [str(server)],
        input=payload,
        capture_output=True,
        text=True,
        timeout=120,
    )
    version = None
    tools = None
    for line in proc.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        result = msg.get("result") or {}
        if msg.get("id") == 1:
            version = (result.get("serverInfo") or {}).get("version")
        elif msg.get("id") == 2:
            tools = result.get("tools")
    if not version:
        raise SystemExit(f"server at {server} did not report a version")
    if tools is None:
        raise SystemExit(f"server at {server} did not answer tools/list")
    out = []
    for tool in tools:
        out.append(
            {
                "name": tool["name"],
                "description": tool.get("description", ""),
            }
        )
    return version, out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--server", type=Path, default=None)
    ap.add_argument("--manifest", type=Path, default=ROOT / "manifest.json")
    ap.add_argument("--check", action="store_true")
    args = ap.parse_args()

    server = args.server or default_server()
    if not server.is_file():
        raise SystemExit(
            f"server binary not found: {server}\n"
            "Build it first: cargo build --release -p zero-search-mcp-server"
        )

    version, tools = query_server(server)

    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    old_tools = manifest.get("tools") or []
    old_version = manifest.get("version")
    stale = (
        old_tools != tools
        or old_version != version
        or manifest.get("tools_generated") is not True
    )

    if args.check:
        if stale:
            print(
                f"manifest.json is stale: {len(old_tools)} tools / version "
                f"{old_version} in file, {len(tools)} tools / version "
                f"{version} from the server. Run scripts/generate-manifest.py.",
                file=sys.stderr,
            )
            return 1
        print(f"manifest.json is fresh ({len(tools)} tools, version {version})")
        return 0

    manifest["tools"] = tools
    manifest["tools_generated"] = True
    manifest["version"] = version
    text = json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True)
    args.manifest.write_text(text + "\n", encoding="utf-8")
    print(
        f"manifest.json regenerated: {len(old_tools)} -> {len(tools)} tools, "
        f"version {old_version} -> {version} (server: {server})"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
