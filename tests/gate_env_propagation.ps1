# Live gate (spec §11): does <agent> preserve PSMUX_PANE_INSTANCE + PSMUX_SESSION_UID
# into a hook subprocess? Manual/live — requires codex/gemini CLIs and a psmux pane.
#
# Strategy: inject a PROBE hook (records the two identity env vars to a file) into the
# agent's real hook file, launch the agent inside a psmux pane, fire one turn, then read
# the probe. PASS if both vars are non-empty. The probe is injected/removed by helper
# functions so the agent's real hook file is restored afterward.
#
# This gate does NOT use `psmux hooks install` (that CLI is built in Task 9); it writes the
# probe directly, because the gate's only job is to prove env propagation.
param(
  [ValidateSet('codex','gemini')][string]$Agent = 'codex',
  [string]$Psmux = "$PSScriptRoot\..\target\release\psmux.exe"
)
$env:PSMUX_ALLOW_ELEVATED = "1"
$ErrorActionPreference = 'Stop'

$probe = Join-Path $env:TEMP "psmux-envgate-$Agent-$PID.txt"
if (Test-Path $probe) { Remove-Item $probe -Force }

# A hook command that records whether the two identity vars survived into the subprocess.
# cmd /c so %VAR% expansion happens in the hook's own environment.
$probeCmd = "cmd /c `"echo INSTANCE=%PSMUX_PANE_INSTANCE% SESSION=%PSMUX_SESSION_UID% > `"$probe`"`""

function Read-Result {
  if (-not (Test-Path $probe)) { Write-Host "NO-PROBE (hook did not fire)"; return 'NO-PROBE' }
  $line = (Get-Content $probe -Raw).Trim()
  Write-Host "RESULT: $line"
  if ($line -match 'INSTANCE=\d+' -and $line -match 'SESSION=\S+') { Write-Host "PASS"; 'PASS' }
  else { Write-Host "FAIL (one or both identity vars empty)"; 'FAIL' }
}

Write-Host "Agent   : $Agent"
Write-Host "Psmux   : $Psmux"
Write-Host "Probe   : $probe"
Write-Host "ProbeCmd: $probeCmd"
Write-Host ""
Write-Host "Caller injects the probe hook, launches $Agent in a psmux pane, fires a turn,"
Write-Host "then invokes Read-Result. (Functions exported when dot-sourced.)"

# Export for dot-sourcing by the orchestrator.
Set-Variable -Name GateProbePath -Value $probe -Scope Global
Set-Variable -Name GateProbeCmd  -Value $probeCmd -Scope Global
