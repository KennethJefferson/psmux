# E2E: PSMUX_REGISTRY_DIR sandboxing + warm retirement (retire-warm).
#
# Proves, against the real release binary:
#   1. With PSMUX_REGISTRY_DIR set, ALL session-registry state (.port/.key/
#      .sid/.pid, next_session_id) lands in the sandbox and the user's real
#      ~/.psmux is untouched.
#   2. kill-session of a NON-last session leaves the namespace warm standby up.
#   3. kill-session of the LAST real session retires the warm standby: warm
#      registry entry gone AND warm process gone.
#   4. Safety: `retire-warm` sent to a CLAIMED (real) session server is
#      refused with "ERR: not warm" and the session stays alive — retirement
#      can never take down a real session.
#
# Teardown uses kill-session only (never kill-server: it kills every
# psmux.exe by image name, including real user sessions).

$ErrorActionPreference = "Stop"
$env:PSMUX_ALLOW_ELEVATED = "1"
$P = Join-Path $PSScriptRoot "..\target\release\psmux.exe"
$REG = Join-Path $env:TEMP "psmux-regsandbox-e2e-$PID"
$env:PSMUX_REGISTRY_DIR = $REG
if (Test-Path $REG) { Remove-Item -Recurse -Force $REG }

$pass = 0; $fail = 0
function P($m){ Write-Host "[PASS] $m" -ForegroundColor Green; $script:pass++ }
function F($m){ Write-Host "[FAIL] $m" -ForegroundColor Red; $script:fail++ }

# Count warm-launched processes born AFTER $since. Strays from earlier runs
# both inflate a plain baseline and can be reaped mid-test by other CLIs'
# orphan reapers, so creation-time filtering is the only stable measure.
function Warm-Born-Since([datetime]$since) {
  @(Get-CimInstance Win32_Process -Filter "Name='psmux.exe'" -EA SilentlyContinue |
    Where-Object { $_.CommandLine -match '-s __warm__' -and $_.CreationDate -gt $since }).Count
}

# Send one authenticated command over raw TCP and return everything the
# server writes back (the CLI has no verb for retire-warm; it is wire-level).
function Send-AuthedCommand([int]$port, [string]$key, [string]$cmd) {
  $c = New-Object System.Net.Sockets.TcpClient
  $c.ReceiveTimeout = 2000
  $c.Connect("127.0.0.1", $port)
  $s = $c.GetStream()
  $w = New-Object System.IO.StreamWriter($s)
  $w.NewLine = "`n"; $w.AutoFlush = $true
  $w.WriteLine("AUTH $key")
  $w.WriteLine($cmd)
  Start-Sleep -m 500
  $buf = New-Object byte[] 4096
  $out = ""
  try { while ($s.DataAvailable) { $n = $s.Read($buf, 0, $buf.Length); if ($n -le 0) { break }; $out += [System.Text.Encoding]::UTF8.GetString($buf, 0, $n) } } catch {}
  $c.Close()
  return $out
}

$realReg = "$env:USERPROFILE\.psmux"
$realBefore = @(Get-ChildItem $realReg -File -EA SilentlyContinue | ForEach-Object Name) | Sort-Object
$testStart = Get-Date

# --- 1. two sessions; registry state must land in the sandbox
& $P new-session -d -s rsb_a -x 100 -y 30
Start-Sleep -m 1500
& $P new-session -d -s rsb_b -x 100 -y 30
Start-Sleep -m 2500

if ((Test-Path "$REG\rsb_a.port") -and (Test-Path "$REG\rsb_b.port")) {
  P "session registry files created inside PSMUX_REGISTRY_DIR"
} else {
  F "session registry files missing from sandbox: $((Get-ChildItem $REG -File -EA SilentlyContinue | ForEach-Object Name) -join ',')"
}
if (Test-Path "$REG\__warm__.port") { P "warm standby registered inside the sandbox" }
else { F "no __warm__.port in sandbox" }

# --- 4. retire-warm against a CLAIMED server must refuse and not kill it
$portB = (Get-Content "$REG\rsb_b.port").Trim()
$keyB  = (Get-Content "$REG\rsb_b.key").Trim()
$resp = Send-AuthedCommand ([int]$portB) $keyB "retire-warm"
if ($resp -match "not warm") { P "retire-warm on a claimed session refused: $($resp.Trim())" }
else { F "expected 'ERR: not warm' from claimed session, got: '$($resp.Trim())'" }
Start-Sleep -m 500
if (Test-Path "$REG\rsb_b.port") { P "claimed session survived the refused retire-warm" }
else { F "claimed session died after refused retire-warm" }

# --- 2. killing a NON-last session keeps the warm standby
& $P kill-session -t rsb_a
Start-Sleep -m 1500
if (Test-Path "$REG\__warm__.port") { P "warm standby survives while another real session lives" }
else { F "warm standby retired too early (rsb_b still alive)" }

# --- 3. killing the LAST session retires the warm standby
& $P kill-session -t rsb_b
Start-Sleep -m 2500
if (-not (Test-Path "$REG\__warm__.port")) { P "warm registry entry removed after last kill-session" }
else { F "__warm__.port still present after last kill-session" }
$leftover = Warm-Born-Since $testStart
if ($leftover -eq 0) { P "no warm processes born during the test remain" }
else { F "warm process leak: $leftover test-born warm server(s) alive after last kill-session" }

# --- 1b. real ~/.psmux must be byte-identical in membership
$realAfter = @(Get-ChildItem $realReg -File -EA SilentlyContinue | ForEach-Object Name) | Sort-Object
if (($realBefore -join ",") -eq ($realAfter -join ",")) { P "real ~/.psmux registry untouched" }
else { F "real ~/.psmux changed: before=[$($realBefore -join ',')] after=[$($realAfter -join ',')]" }

Remove-Item -Recurse -Force $REG -EA SilentlyContinue
Remove-Item Env:\PSMUX_REGISTRY_DIR -EA SilentlyContinue

Write-Host "`nPassed=$pass Failed=$fail"
if ($fail -eq 0) { Write-Host "PASS test_registry_sandbox_e2e" } else { exit 1 }
