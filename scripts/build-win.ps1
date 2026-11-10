# Local Windows build. All output stays inside the project (.build, codex-rs\target-win).
$root = Split-Path (Split-Path $MyInvocation.MyCommand.Path -Parent) -Parent
Set-Location $root
New-Item -ItemType Directory -Force .build\cargo-home, .build\tmp | Out-Null
$env:CARGO_HOME = "$root\.build\cargo-home"
$env:TMP = "$root\.build\tmp"; $env:TEMP = $env:TMP
$env:CARGO_TARGET_DIR = "$root\codex-rs\target-win"
$env:CARGO_NET_GIT_FETCH_WITH_CLI = "true"
Set-Location codex-rs
cargo build --profile dev-small -p codex-cli --bin inocode *> "$root\.build\build.log"
"EXIT $LASTEXITCODE" >> "$root\.build\build.log"
