# Proves acv works, end to end, with no setup and no agent.
#
# Builds a throwaway git repo, plants an agent session that LIES (claims tests
# pass when the suite failed, and quietly deletes a file it never mentioned),
# then runs the real binary against it.
#
#   .\scripts\demo.ps1              # debug binary
#   .\scripts\demo.ps1 -Release     # release binary

param([switch]$Release)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

if ($Release) {
    Push-Location $root
    & cargo build --release --quiet
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw "release build failed" }
    Pop-Location
    $acv = Join-Path $root "target\release\acv.exe"
} else {
    Push-Location $root
    & cargo build --quiet
    if ($LASTEXITCODE -ne 0) { Pop-Location; throw "build failed" }
    Pop-Location
    $acv = Join-Path $root "target\debug\acv.exe"
}

$demo = Join-Path $env:TEMP "acv-demo"
if (Test-Path $demo) { Remove-Item -Recurse -Force $demo }
New-Item -ItemType Directory -Path $demo | Out-Null
Push-Location $demo

& git init -q -b main
& git config user.email "demo@example.com"
& git config user.name "demo"

"def send():" | Set-Content client.py
"# proj"      | Set-Content README.md
& git add -A | Out-Null
& git commit -q -m "base"
$baseline = (& git rev-parse HEAD).Trim()

# --- the agent's undisclosed work ---
"def send():"                       | Set-Content client.py   # silently rewritten
"def health(): return {'ok':True}"  | Set-Content server.py   # never mentioned
Remove-Item README.md                                       # a silent DELETION
& git add -A | Out-Null
& git commit -q -m "agent work"

# --- the agent's transcript, claiming the opposite ---
$cwdFwd = $demo.Replace('\', '/')
$tpl = Get-Content (Join-Path $root "tests\fixtures\lying_session.jsonl.tmpl") -Raw
$rollout = "rollout-2026-09-27T11-00-00-22222222-2222-2222-2222-222222222222.jsonl"
[System.IO.File]::WriteAllText((Join-Path $demo $rollout), $tpl.Replace('__CWD__', $cwdFwd).Replace('__BASELINE__', $baseline))

Pop-Location

Write-Host ""
Write-Host "  git ground truth since baseline:" -ForegroundColor DarkGray
& git -C $demo diff --name-status "$baseline..HEAD" | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
Write-Host ""
Write-Host "  agent claimed: Added a health check endpoint in server.py. All tests pass and the work is complete." -ForegroundColor DarkGray
Write-Host ""
Write-Host "  acv says:" -ForegroundColor DarkGray
Write-Host "  ----------------------------------------------------------------"

& $acv session (Join-Path $demo $rollout) --no-color
$code = $LASTEXITCODE

Write-Host "  ----------------------------------------------------------------"
Write-Host ""
if ($code -eq 1) {
    Write-Host "  acv exited 1: it found discrepancies. That is the tool working." -ForegroundColor Green
} else {
    Write-Host "  acv exited $code : expected 1." -ForegroundColor Red
}
Write-Host ""
Write-Host "  demo repo left at $demo" -ForegroundColor DarkGray
exit $code
