# Task 10 live E2E v2 — same as v1 but robust codex prompt submission (Esc-clear, type, pause, Enter x2).
$ErrorActionPreference = 'Continue'
$env:PSMUX_ALLOW_ELEVATED = '1'
$env:PSMUX_NO_WARM        = '1'
$Psmux   = 'K:\Downloads\__Projects.Mine\psmuxPlus\psmux\target\release\psmux.exe'
$hooks   = "$env:USERPROFILE\.codex\hooks.json"
$backup  = "$hooks.e2e-bak-$PID"
$session = "codexe2e$PID"
$waitOut = Join-Path $env:TEMP "psmux-e2e-waitevent-$PID.txt"

Write-Host "=== TASK 10 LIVE E2E v2 ===  session: $session"
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

  & $Psmux send-keys -t "$session.0" -l 'codex --dangerously-bypass-approvals-and-sandbox' | Out-Null
  & $Psmux send-keys -t "$session.0" Enter | Out-Null
  Write-Host ">>> 30s codex boot... <<<"; Start-Sleep -Seconds 30
  # accept trust
  & $Psmux send-keys -t "$session.0" -l '2' | Out-Null; Start-Sleep -Milliseconds 400
  & $Psmux send-keys -t "$session.0" Enter | Out-Null; Start-Sleep -Seconds 5
  & $Psmux capture-pane -t "$session.0" -p 2>&1 | Select-Object -Last 6 | Write-Host

  # start wait-event background
  Write-Host "`nStarting wait-event --name agent-done --pane $pane --timeout 120 (bg)..."
  $wjob = Start-Job -ScriptBlock {
    param($exe,$pane,$out); $env:PSMUX_ALLOW_ELEVATED='1'
    & $exe wait-event --name agent-done --pane $pane --timeout 120 *>&1 | Set-Content $out
    "EXIT=$LASTEXITCODE" | Add-Content $out
  } -ArgumentList $Psmux,$paneNum,$waitOut
  Start-Sleep -Seconds 2

  # ROBUST submit: Esc to clear any stray buffer, type prompt, pause, Enter, pause, Enter again
  Write-Host "Submitting prompt (robust)..."
  & $Psmux send-keys -t "$session.0" Escape | Out-Null; Start-Sleep -Milliseconds 500
  & $Psmux send-keys -t "$session.0" -l 'say pong' | Out-Null; Start-Sleep -Seconds 2
  & $Psmux send-keys -t "$session.0" Enter | Out-Null; Start-Sleep -Seconds 2
  # verify it left the input line; if still showing our text unsubmitted, hit Enter once more
  $c = & $Psmux capture-pane -t "$session.0" -p 2>&1 | Out-String
  if ($c -match 'say pong') { Write-Host "prompt still in box; Enter again"; & $Psmux send-keys -t "$session.0" Enter | Out-Null }
  Write-Host "capture after submit:"; & $Psmux capture-pane -t "$session.0" -p 2>&1 | Select-Object -Last 8 | Write-Host

  Write-Host ">>> waiting up to 125s for agent-done... <<<"
  $wjob | Wait-Job -Timeout 128 | Out-Null; Receive-Job $wjob 2>&1 | Out-Null; Remove-Job $wjob -Force 2>$null

  Write-Host "`n=== WAIT-EVENT RESULT ==="
  if (Test-Path $waitOut) {
    $res = Get-Content $waitOut -Raw; Write-Host $res
    if ($res -match 'agent-done' -and $res -match 'EXIT=0') { Write-Host "VERDICT: PASS" }
    elseif ($res -match 'EXIT=2') { Write-Host "VERDICT: FAIL (timeout)" }
    else { Write-Host "VERDICT: INCONCLUSIVE" }
  } else { Write-Host "no output. INCONCLUSIVE" }
  Write-Host "final capture:"; & $Psmux capture-pane -t "$session.0" -p 2>&1 | Select-Object -Last 12 | Write-Host
}
finally {
  Write-Host "`n=== CLEANUP ==="
  & $Psmux hooks uninstall codex 2>&1 | Write-Host
  Copy-Item $backup $hooks -Force; Remove-Item $backup -Force
  & $Psmux kill-session -t $session 2>&1 | Write-Host
  if (Test-Path $waitOut) { Remove-Item $waitOut -Force }
  Write-Host "e2e done."
}
