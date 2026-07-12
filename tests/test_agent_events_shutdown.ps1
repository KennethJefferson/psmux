$ErrorActionPreference = "Stop"
$env:PSMUX_ALLOW_ELEVATED = "1"
$P = Join-Path $PSScriptRoot "..\target\release\psmux.exe"
$S = "evshut"
& $P kill-server 2>$null; Start-Sleep -m 500
& $P new-session -d -s $S
Start-Sleep -m 800
$out = "$env:TEMP\psmux-evshut.jsonl"
Remove-Item $out -ErrorAction SilentlyContinue
$job = Start-Job -ScriptBlock {
    param($exe, $sess, $file)
    & $exe -t $sess events *> $file
} -ArgumentList $P, $S, $out
Start-Sleep -Seconds 2
& $P -t $S kill-server
$null = Wait-Job $job -Timeout 15
if ((Get-Content $out -Raw) -notmatch '"bus-closed"') { throw "no bus-closed terminal frame" }
Remove-Job $job -Force
Write-Host "PASS test_agent_events_shutdown"
