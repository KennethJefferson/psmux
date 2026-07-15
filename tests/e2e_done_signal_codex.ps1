# Live E2E: codex done-signal via the REAL installed hook + REAL wait-event.
# PASSED 2026-07-15 (agent-done {pane:1, pane_instance:1} woke wait-event, exit 0).
#
# Chain proven: `psmux hooks install codex` -> codex turn -> Stop hook (PowerShell
# call-operator command) -> hook-notify -> agent-done on the bus -> wait-event exit 0.
#
# Requirements / gotchas baked in (each cost a failed run to learn):
#  - PSMUX_NO_WARM=1: a session's first pane needs minted identity (warm-claim gap).
#  - Launch codex INTO a surviving shell pane via send-keys (codex as the new-session
#    initial command kills the pane); wait for idle before submitting a prompt.
#  - codex re-trust modal auto-accepted with '2' + Enter (hooks.json changed => re-trust).
#  - `wait-event --timeout` is MILLISECONDS (420000 = 7 min). 60/150/420 caused instant
#    timeouts that masked a working hook.
#  - Cleanup is a finally block: uninstall hook, restore hooks.json, kill-session, and
#    close the spawned attach window (no leftover psmux instances).
param([switch]$Visible)
$ErrorActionPreference = 'Continue'
$env:PSMUX_ALLOW_ELEVATED = '1'
$env:PSMUX_NO_WARM        = '1'
$Psmux   = "$PSScriptRoot\..\target\release\psmux.exe"
$hooks   = "$env:USERPROFILE\.codex\hooks.json"
$backup  = "$hooks.e2e-bak-$PID"
$session = "codexe2e$PID"
$waitOut = Join-Path $env:TEMP "psmux-e2e-waitevent-$PID.txt"
$attachProc = $null

Write-Host "=== CODEX DONE-SIGNAL E2E ===  session: $session  visible: $Visible"
Copy-Item $hooks $backup -Force

try {
  & $Psmux hooks install codex 2>&1 | Write-Host
  & $Psmux hooks status codex 2>&1 | Write-Host

  & $Psmux kill-session -t $session 2>$null | Out-Null
  & $Psmux new-session -d -s $session -x 200 -y 50 2>&1 | Write-Host
  Start-Sleep -Seconds 2
  $pane = (& $Psmux list-panes -t $session -F "#{pane_id}" | Select-Object -First 1)
  $paneNum = $pane.TrimStart('%')
  Write-Host "pane: $pane"

  if ($Visible) {
    $attachProc = Start-Process -PassThru -FilePath "powershell.exe" -ArgumentList @(
      '-NoExit','-NoProfile','-Command',
      "`$env:PSMUX_ALLOW_ELEVATED='1'; & '$Psmux' attach -t $session")
    Start-Sleep -Seconds 3
  }

  Write-Host "Launching codex into the pane..."
  & $Psmux send-keys -t "$session.0" -l 'codex --dangerously-bypass-approvals-and-sandbox -c model_reasoning_effort=low' | Out-Null
  & $Psmux send-keys -t "$session.0" Enter | Out-Null
  Write-Host ">>> 30s codex boot... <<<"; Start-Sleep -Seconds 30
  Write-Host "Auto-accepting re-trust (2 + Enter)..."
  & $Psmux send-keys -t "$session.0" -l '2' | Out-Null; Start-Sleep -Milliseconds 400
  & $Psmux send-keys -t "$session.0" Enter | Out-Null; Start-Sleep -Seconds 6

  Write-Host "Starting wait-event --name agent-done --pane $pane --timeout 420000 (bg)..."
  $wjob = Start-Job -ScriptBlock {
    param($exe,$pane,$out); $env:PSMUX_ALLOW_ELEVATED='1'
    & $exe wait-event --name agent-done --pane $pane --timeout 420000 *>&1 | Set-Content $out
    "EXIT=$LASTEXITCODE" | Add-Content $out
  } -ArgumentList $Psmux,$paneNum,$waitOut
  Start-Sleep -Seconds 2

  Write-Host "Waiting for codex idle, then submitting prompt..."
  for ($k=0; $k -lt 12; $k++) {
    $c = & $Psmux capture-pane -t "$session.0" -p 2>&1 | Out-String
    if ($c -notmatch 'Working \(' ) { break }; Start-Sleep -Seconds 3
  }
  & $Psmux send-keys -t "$session.0" -l 'say pong' | Out-Null; Start-Sleep -Seconds 2
  & $Psmux send-keys -t "$session.0" Enter | Out-Null; Start-Sleep -Seconds 3
  $c = & $Psmux capture-pane -t "$session.0" -p 2>&1 | Out-String
  if ($c -match 'say pong' -and $c -notmatch 'Working \(') {
    & $Psmux send-keys -t "$session.0" Enter | Out-Null   # TUI sometimes needs a second Enter
  }
  Write-Host ">>> Waiting up to 7 min (wait-event returns the instant the Stop hook fires)... <<<"
  for ($m=0; $m -lt 84; $m++) {
    if ($wjob.State -ne 'Running') { break }; Start-Sleep -Seconds 5
  }
  $wjob | Wait-Job -Timeout 5 | Out-Null; Receive-Job $wjob 2>&1 | Out-Null; Remove-Job $wjob -Force 2>$null

  Write-Host "`n=== WAIT-EVENT RESULT ==="
  if (Test-Path $waitOut) {
    $res = Get-Content $waitOut -Raw; Write-Host $res
    if ($res -match 'agent-done' -and $res -match 'EXIT=0') { Write-Host "VERDICT: PASS" }
    elseif ($res -match 'EXIT=2') { Write-Host "VERDICT: FAIL (timeout)" }
    else { Write-Host "VERDICT: INCONCLUSIVE" }
  } else { Write-Host "no output. INCONCLUSIVE" }
}
finally {
  Write-Host "`n=== CLEANUP ==="
  & $Psmux hooks uninstall codex 2>&1 | Write-Host
  Copy-Item $backup $hooks -Force; Remove-Item $backup -Force
  & $Psmux kill-session -t $session 2>&1 | Out-Null
  if ($attachProc -and -not $attachProc.HasExited) { Stop-Process -Id $attachProc.Id -Force -Confirm:$false }
  if (Test-Path $waitOut) { Remove-Item $waitOut -Force }
  Write-Host "hooks.json restored; session + attach window cleaned. done."
}
