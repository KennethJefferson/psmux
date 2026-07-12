# Controlled reproduction: does psmux leak `server -s __warm__` processes?
#
# Warm servers are a perf feature (a pre-spawned shell so the NEXT new-session is
# fast). At most ONE warm server per namespace should be alive at a time. This
# test proves whether rapid session creation accumulates warm servers, and
# whether kill-server reaps them.
#
# Uses a dedicated namespace -L rbLeak. Does a GLOBAL baseline kill ONCE at the
# very start (allowed for setup) so the count is unambiguous, then only uses
# namespaced operations.

$ErrorActionPreference = "Continue"
$NS = "rbLeak"
$pass = 0; $fail = 0
function P($m){ Write-Host "[PASS] $m" -ForegroundColor Green; $script:pass++ }
function F($m){ Write-Host "[FAIL] $m" -ForegroundColor Red; $script:fail++ }
function I($m){ Write-Host "[INFO] $m" -ForegroundColor Cyan }

function Count-Psmux { @(Get-Process psmux -EA SilentlyContinue).Count }
function Count-Warm {
  $p = Get-CimInstance Win32_Process -Filter "Name='psmux.exe'" -EA SilentlyContinue
  @($p | Where-Object { $_.CommandLine -match '-s __warm__' }).Count
}

Write-Host "=== WARM-SERVER LEAK CONTROLLED REPRO (-L $NS) ===" -ForegroundColor Yellow

# Hard clean baseline (setup only).
Get-Process psmux -EA SilentlyContinue | Stop-Process -Force -EA SilentlyContinue
Start-Sleep -Seconds 2
$base = Count-Psmux
$baseWarm = Count-Warm
I "baseline psmux=$base warm=$baseWarm (expect 0/0)"

# Create and immediately kill N sessions in the namespace.
$N = 12
for ($i=1; $i -le $N; $i++) {
  & psmux -L $NS new-session -d -s "rbLeak_$i" 2>&1 | Out-Null
  if ($i -eq 1) { Start-Sleep -Seconds 3 } else { Start-Sleep -Milliseconds 400 }
  & psmux -L $NS kill-session -t "rbLeak_$i" 2>&1 | Out-Null
  Start-Sleep -Milliseconds 200
  if ($i % 4 -eq 0) {
    I ("after $i create/kill cycles: psmux={0} warm={1}" -f (Count-Psmux), (Count-Warm))
  }
}

Start-Sleep -Seconds 1
$afterChurn = Count-Psmux
$afterChurnWarm = Count-Warm
I "after $N create/kill cycles: psmux=$afterChurn warm=$afterChurnWarm"

# D2 (retire-warm): each cycle's kill-session killed the namespace's LAST real
# session, which must also retire the namespace warm standby. After the final
# kill-session, zero warm servers may remain in this namespace.
$nsWarm = @(Get-CimInstance Win32_Process -Filter "Name='psmux.exe'" -EA SilentlyContinue |
  Where-Object { $_.CommandLine -match '-s __warm__' -and $_.CommandLine -match "-L $NS" }).Count
if ($nsWarm -eq 0) {
  P "kill-session of last real session retired the namespace warm server (0 remain)"
} else {
  F "WARM NOT RETIRED: $nsWarm namespace warm server(s) alive after last kill-session"
}

# Tear down the namespace server.
& psmux -L $NS kill-server 2>&1 | Out-Null
Start-Sleep -Seconds 2
$afterKill = Count-Psmux
$afterKillWarm = Count-Warm
I "after kill-server: psmux=$afterKill warm=$afterKillWarm"

# VERDICT
# At most 1 warm server should ever survive (and kill-server should reap it).
if ($afterChurnWarm -le 1) {
  P "warm servers bounded during churn (<=1, got $afterChurnWarm)"
} else {
  F "WARM-SERVER LEAK: $afterChurnWarm warm servers alive after $N create/kill cycles (expected <=1)"
}

if ($afterKillWarm -eq 0) {
  P "kill-server reaped warm server(s) (0 remain)"
} else {
  F "kill-server did NOT reap warm servers ($afterKillWarm remain)"
}

if ($afterKill -le $base) {
  P "total psmux returned to baseline after kill-server ($afterKill <= $base)"
} else {
  F "psmux process leak: $afterKill alive after kill-server (baseline $base)"
}

# Final safety cleanup (namespaced).
& psmux -L $NS kill-server 2>&1 | Out-Null

Write-Host "`nPassed=$pass Failed=$fail"
exit $fail
