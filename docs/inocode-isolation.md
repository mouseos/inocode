# InoCode isolation from OpenAI Codex

InoCode can be installed and run next to OpenAI Codex. Nothing below is shared between the two.

| Area | Codex | InoCode |
|---|---|---|
| Executable | codex | inocode (helpers: inocode-*) |
| Config / state directory | ~/.codex, $CODEX_HOME | ~/.inocode, $INOCODE_HOME |
| Project directory | .codex/ | .inocode/ |
| Environment variables | CODEX_* | INOCODE_* |
| Credential store | "Codex Auth", "Codex MCP Credentials", "codex" | "InoCode Auth", "InoCode MCP Credentials", "inocode" |
| OAuth callback port | prefers 1455, falls back to 1457 | prefers 1457, falls back to 1455; never cancels a login server it did not start |
| Login /cancel | bare GET /cancel | only /cancel?inocode_cancel=1 |
| Windows sandbox | CodexSandbox* users/group/service/pipe/mutex/desktop, WFP GUIDs, HKLM key, LocalAppData dir | InoCodeSandbox*, new WFP GUIDs, HKLM\SOFTWARE\mouseos\InoCode, LocalAppData\mouseos\InoCode |
| Updater / installer | openai/codex releases, @openai/codex, chatgpt.com/codex/install.* | mouseos/inocode releases, @mouseos/inocode |
| Desktop app id / URL scheme | com.openai.codex, codex:// | com.mouseos.inocode, inocode:// |

Values checked by OpenAI services are intentionally unchanged: OAuth client_id, the codex_cli_simplified_flow
parameter, the originator header, and /backend-api/codex and /wham API paths.

## Checks

    python scripts/check_isolation.py                              # static guard (source, tests, build files)
    python scripts/smoke_isolation.py codex-rs/target/debug/inocode.exe   # runs the binary next to decoy Codex state

Unit tests: isolation_tests modules in utils/home-dir, login (storage, server), rmcp-client (oauth), secrets,
and login/tests/suite/login_server_e2e.rs (plain cancel is ignored, occupied port is not stolen).

