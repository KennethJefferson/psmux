# E2E: capture-pane -t %N must return THAT pane's screen even when other
# commands land mid-settle.
#
# Regression for the settle wrong-pane leak found in the 2026-07-12 live demo:
# -t %N targeting rode on FocusPaneTemp, whose one-shot restore is consumed by
# the next non-probe request from ANY client. A pane-targeted command (here:
# send-keys to a third pane; in the wild: an agent's hook-notify) arriving
# during the settle window snapped focus away, and the final capture returned
# the WRONG pane's content. The fix pins the capture to the resolved pane id.

$ErrorActionPreference = "Stop"
$env:PSMUX_ALLOW_ELEVATED = "1"
$P = Join-Path $PSScriptRoot "..\target\release\psmux.exe"
$REG = Join-Path $env:TEMP "psmux-settlepin-e2e-$PID"
$env:PSMUX_REGISTRY_DIR = $REG
if (Test-Path $REG) { Remove-Item -Recurse -Force $REG }

$pass = 0; $fail = 0
function P($m){ Write-Host "[PASS] $m" -ForegroundColor Green; $script:pass++ }
function F($m){ Write-Host "[FAIL] $m" -ForegroundColor Red; $script:fail++ }

$S = "settlepin"
& $P new-session -d -s $S -x 160 -y 40
Start-Sleep -m 1500
& $P -t $S split-window -h -d
& $P -t $S split-window -v -d
Start-Sleep -m 1200
$panes = @(& $P -t $S list-panes | ForEach-Object { if ($_ -match '(%\d+)') { $Matches[1] } })
$active = @(& $P -t $S list-panes | Where-Object { $_ -match '\(active\)' } | ForEach-Object { if ($_ -match '(%\d+)') { $Matches[1] } })[0]
$others = @($panes | Where-Object { $_ -ne $active })
$target = $others[0]; $poke = $others[1]

# distinct content: marker in the TARGET pane, different marker in the ACTIVE pane
& $P -t "${S}:$target" send-keys "echo TARGET-MARKER-XYZ" Enter
& $P -t "${S}:$active" send-keys "echo ACTIVE-MARKER-QQQ" Enter
Start-Sleep -m 1500

# long settle capture of the (quiet) target pane, poke a THIRD pane mid-settle
$job = Start-Job -ScriptBlock {
  param($exe, $sess, $tgt, $reg)
  $env:PSMUX_ALLOW_ELEVATED = "1"; $env:PSMUX_REGISTRY_DIR = $reg
  @(& $exe -t "${sess}:$tgt" capture-pane -p --settle 2500 --settle-timeout 8000)
} -ArgumentList $P, $S, $target, $REG
Start-Sleep -m 1000
& $P -t "${S}:$poke" send-keys "" 2>$null       # pane-targeted request mid-settle
Start-Sleep -m 300
& $P -t "${S}:$poke" send-keys "" 2>$null
$cap = @(Receive-Job -Job $job -Wait); Remove-Job $job
$joined = ($cap | Where-Object { $_ -match '\S' }) -join "`n"

if ($joined -match "TARGET-MARKER-XYZ") { P "settle capture contains the target pane's marker" }
else { F "settle capture MISSING target marker; got: $(($cap | Select-Object -Last 3) -join ' | ')" }
if ($joined -notmatch "ACTIVE-MARKER-QQQ") { P "settle capture did not leak the active pane's content" }
else { F "WRONG-PANE LEAK: capture of $target contains the active pane's marker" }

# plain (non-settle) -t capture with a concurrent poke stays correct too
$job2 = Start-Job -ScriptBlock {
  param($exe, $sess, $tgt, $reg)
  $env:PSMUX_ALLOW_ELEVATED = "1"; $env:PSMUX_REGISTRY_DIR = $reg
  @(& $exe -t "${sess}:$tgt" capture-pane -p)
} -ArgumentList $P, $S, $target, $REG
& $P -t "${S}:$poke" send-keys "" 2>$null
$cap2 = @(Receive-Job -Job $job2 -Wait); Remove-Job $job2
if ((($cap2 | Where-Object { $_ -match '\S' }) -join "`n") -match "TARGET-MARKER-XYZ") { P "plain -t capture correct under concurrency" }
else { F "plain -t capture wrong under concurrency" }

& $P kill-session -t $S
Remove-Item -Recurse -Force $REG -EA SilentlyContinue
Remove-Item Env:\PSMUX_REGISTRY_DIR -EA SilentlyContinue

Write-Host "`nPassed=$pass Failed=$fail"
if ($fail -eq 0) { Write-Host "PASS test_settle_pinned_capture_e2e" } else { exit 1 }
