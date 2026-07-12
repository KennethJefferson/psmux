# Issue #450: prove the opt-in `@heal-crashed-panes` self-heal.
# A shell can FailFast on its first ConPTY read right after a warm-pane transplant
# (reporters' pwsh/PSReadLine env). From psmux's view that is just an exited child,
# so we simulate it deterministically by killing a freshly created pane's shell
# within the grace window. With the flag ON psmux must respawn a working shell in
# place (window survives); with it OFF the window is pruned (pre-fix behavior).

$ErrorActionPreference = "Continue"
$PSMUX = (Get-Command psmux -EA Stop).Source
$psmuxDir = "$env:USERPROFILE\.psmux"
$script:Pass = 0; $script:Fail = 0
function P($m){ Write-Host "  [PASS] $m" -ForegroundColor Green; $script:Pass++ }
function F($m){ Write-Host "  [FAIL] $m" -ForegroundColor Red; $script:Fail++ }
function I($m){ Write-Host "[*] $m" -ForegroundColor Cyan }

function New-Sess($name) {
  & $PSMUX kill-session -t $name 2>&1 | Out-Null
  Start-Sleep -Milliseconds 400
  Remove-Item "$psmuxDir\$name.*" -Force -EA SilentlyContinue
  Start-Process -FilePath $PSMUX -ArgumentList "new-session","-s",$name,"-d" -WindowStyle Hidden | Out-Null
  for ($i=0;$i -lt 40;$i++){ Start-Sleep -Milliseconds 250; & $PSMUX has-session -t $name 2>$null; if ($LASTEXITCODE -eq 0){ return $true } }
  return $false
}
function ActivePanePid($name){ (& $PSMUX display-message -t $name -p '#{pane_pid}' 2>&1).Trim() }
function ActiveWinIdx($name){ (& $PSMUX display-message -t $name -p '#{window_index}' 2>&1).Trim() }
function WinCount($name){ [int]((& $PSMUX display-message -t $name -p '#{session_windows}' 2>&1).Trim()) }

# ---------------------------------------------------------------------------
Write-Host "`n=== Issue #450 @heal-crashed-panes proof ===" -ForegroundColor Cyan

# PHASE 1: flag ON -> crashed new-window pane is respawned, window survives
I "PHASE 1: @heal-crashed-panes ON"
$S = "heal_on"
if (-not (New-Sess $S)) { F "session failed"; }
else {
  & $PSMUX set-option -t $S -g '@heal-crashed-panes' on 2>&1 | Out-Null
  $opt = (& $PSMUX show-options -t $S -g -v '@heal-crashed-panes' 2>&1).Trim()
  if ($opt -eq 'on') { P "option set (@heal-crashed-panes=on)" } else { F "option not set, got '$opt'" }

  $before = WinCount $S
  & $PSMUX new-window -t $S 2>&1 | Out-Null
  Start-Sleep -Milliseconds 1200
  $w = ActiveWinIdx $S
  $p1 = ActivePanePid $S
  I "new window index=$w pane_pid=$p1 (windows: $before -> $(WinCount $S))"

  # Simulate crash-on-startup: kill the shell within the grace window
  try { Stop-Process -Id ([int]$p1) -Force -EA Stop; I "killed pane shell $p1 (simulated FailFast)" } catch { F "could not kill ${p1}: $_" }

  # Wait for reap+heal tick
  Start-Sleep -Milliseconds 2500

  # Window must still exist
  $after = WinCount $S
  & $PSMUX has-session -t $S 2>$null
  $sessOk = ($LASTEXITCODE -eq 0)
  if ($sessOk -and $after -eq $before + 1) { P "window survived the crash (count still $after)" }
  else { F "window not preserved (before=$before after=$after sessOk=$sessOk)" }

  # A NEW shell must be alive in that window (pid changed + echo works).
  # Retry the pid read: right after respawn the pane briefly reports no pid.
  $p2 = ""
  for ($k=0; $k -lt 20; $k++) {
    Start-Sleep -Milliseconds 300
    $p2 = ActivePanePid $S
    if ($p2 -match '^\d+$' -and $p2 -ne $p1) { break }
  }
  if ($p2 -and $p2 -ne $p1 -and $p2 -match '^\d+$') { P "pane respawned with a NEW shell pid ($p1 -> $p2)" }
  else { F "pane not respawned (p1=$p1 p2=$p2)" }

  & $PSMUX send-keys -t $S "echo HEAL_MARKER_1" Enter 2>&1 | Out-Null
  Start-Sleep -Milliseconds 1200
  $cap = & $PSMUX capture-pane -t $S -p 2>&1 | Out-String
  if ($cap -match "HEAL_MARKER_1") { P "respawned shell is responsive (echo works)" } else { F "respawned shell not responsive" }

  & $PSMUX kill-session -t $S 2>&1 | Out-Null
}

# PHASE 2: flag OFF -> crashed new-window pane is NOT respawned (window pruned)
I "PHASE 2: @heal-crashed-panes OFF (default) - control"
$S2 = "heal_off"
if (-not (New-Sess $S2)) { F "session2 failed" }
else {
  $optOff = (& $PSMUX show-options -t $S2 -g -v '@heal-crashed-panes' 2>&1).Trim()
  I "option value (unset/default): '$optOff'"
  $before2 = WinCount $S2
  & $PSMUX new-window -t $S2 2>&1 | Out-Null
  Start-Sleep -Milliseconds 1200
  $wp = ActivePanePid $S2
  $cnt_mid = WinCount $S2
  try { Stop-Process -Id ([int]$wp) -Force -EA Stop; I "killed pane shell $wp" } catch { F "could not kill $wp" }
  Start-Sleep -Milliseconds 2500
  $after2 = WinCount $S2
  if ($after2 -eq $before2) { P "control: window was pruned (no heal): $cnt_mid -> $after2" }
  else { F "control: expected prune to $before2, got $after2 (heal may be firing when OFF!)" }
  & $PSMUX kill-session -t $S2 2>&1 | Out-Null
}

Write-Host "`n=== Results ===" -ForegroundColor Cyan
Write-Host "  Passed: $script:Pass" -ForegroundColor Green
Write-Host "  Failed: $script:Fail" -ForegroundColor $(if($script:Fail){'Red'}else{'Green'})
& $PSMUX kill-server 2>&1 | Out-Null
exit $script:Fail
