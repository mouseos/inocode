# Start the mock Responses server and an app-server pointed at it (for GUI streaming checks).
# mock: 127.0.0.1:18800, app-server: ws://127.0.0.1:18802. Logs: .build\mock.log, .build\app.log
$root = Split-Path (Split-Path $MyInvocation.MyCommand.Path -Parent) -Parent
Set-Location $root
New-Item -ItemType Directory -Force .build\gui-home | Out-Null
Start-Process node -WindowStyle Hidden -ArgumentList 'scripts/mock-responses-server.mjs','18800' -RedirectStandardOutput .build\mock.log -RedirectStandardError .build\mock.err
$env:INOCODE_HOME = "$root\.build\gui-home"
Start-Process "$root\codex-rs\target-win\dev-small\inocode.exe" -WindowStyle Hidden -ArgumentList 'app-server','--listen','ws://127.0.0.1:18802','-c','model_provider=mock','-c','model_providers.mock.name=mock','-c','model_providers.mock.base_url=http://127.0.0.1:18800/v1','-c','model_providers.mock.wire_api=responses' -RedirectStandardOutput .build\app.log -RedirectStandardError .build\app.err
