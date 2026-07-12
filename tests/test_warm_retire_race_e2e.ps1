# E2E: claim-vs-retire interleaving for warm retirement (retire-warm).
#
# kill-session's warm retirement has a registry-level race: between its scan
# ("no other live session") and the retire-warm landing, a concurrent
# new-session can CLAIM the warm standby. Claims and retires serialize through
# the server's CtrlReq channel, so whichever arrives first wins atomically;
# the required outcome is that a claim processed first makes the retire a
# refusal — a real session must NEVER die to a stale retirement.
#
# Part 1 reconstructs the losing interleave DETERMINISTICALLY at the wire
# level: read the warm pointer exactly as retire_warm_server would, claim the
# warm server through the real claim-session path, then deliver retire-warm
# to the (now stale) saved pointer. Expected: "ERR: not warm", session lives.
#
# Part 2 races the real CLI: kill-session (last session) and new-session fired
# concurrently, N times. Whatever interleaving the OS picks, the new session
# must survive and warm processes must stay bounded.
#
# Teardown uses kill-session only (never kill-server: it kills every psmux.exe
# by image name, including real user sessions).

$ErrorActionPreference = "Stop"
$env:PSMUX_ALLOW_ELEVATED = "1"
$P = Join-Path $PSScriptRoot "..\target\release\psmux.exe"
$REG = Join-Path $env:TEMP "psmux-retirerace-e2e-$PID"
$env:PSMUX_REGISTRY_DIR = $REG
if (Test-Path $REG) { Remove-Item -Recurse -Force $REG }

$pass = 0; $fail = 0
function P($m){ Write-Host "[PASS] $m" -ForegroundColor Green; $script:pass++ }
function F($m){ Write-Host "[FAIL] $m" -ForegroundColor Red; $script:fail++ }
function I($m){ Write-Host "[INFO] $m" -ForegroundColor Cyan }

# Count warm-launched processes born AFTER $since. Counting by launch command
# line alone is unreliable across runs: strays from earlier runs both inflate
# a baseline and can be reaped mid-test by other CLIs' orphan reapers (every
# psmux invocation runs one), making simple before/after deltas go negative.
function Warm-Born-Since([datetime]$since) {
  @(Get-CimInstance Win32_Process -Filter "Name='psmux.exe'" -EA SilentlyContinue |
    Where-Object { $_.CommandLine -match '-s __warm__' -and $_.CreationDate -gt $since }).Count
}

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

$testStart = Get-Date

# ============================================================================
# Part 1: deterministic losing interleave (claim lands before the retire)
# ============================================================================
I "Part 1: deterministic stale-pointer retire after claim"

& $P new-session -d -s race_seed -x 100 -y 30    # cold spawn; warm standby follows
Start-Sleep -m 2500
if (-not (Test-Path "$REG\__warm__.port")) { F "no warm standby to race against"; exit 1 }

# Snapshot the warm pointer EXACTLY as retire_warm_server would read it.
$stalePort = [int](Get-Content "$REG\__warm__.port").Trim()
$staleKey  = (Get-Content "$REG\__warm__.key").Trim()

# The claim wins the interleave: goes through the REAL claim-session path
# (same wire command new-session's fast path sends).
$claimResp = Send-AuthedCommand $stalePort $staleKey "claim-session race_claimed"
if ($claimResp -match "OK") { P "warm server claimed as 'race_claimed' (claim won the interleave)" }
else { F "claim-session failed: '$($claimResp.Trim())'"; exit 1 }
Start-Sleep -m 800

# The retire loses: delivered to the stale pointer AFTER the claim.
$retireResp = Send-AuthedCommand $stalePort $staleKey "retire-warm"
if ($retireResp -match "not warm") { P "stale retire-warm refused by the just-claimed server" }
else { F "expected 'ERR: not warm' after losing the interleave, got: '$($retireResp.Trim())'" }

Start-Sleep -m 500
& $P -t race_claimed list-panes *> $null
if ($LASTEXITCODE -eq 0 -and (Test-Path "$REG\race_claimed.port")) {
  P "claimed session fully alive after the stale retire (responds to commands)"
} else {
  F "claimed session damaged by stale retire (exit=$LASTEXITCODE)"
}

& $P kill-session -t race_seed
Start-Sleep -m 1000
& $P kill-session -t race_claimed                 # last real session -> retires replacement warm
Start-Sleep -m 2500

# ============================================================================
# Part 2: true concurrent CLI race, N rounds
# ============================================================================
$ROUNDS = 10
I "Part 2: $ROUNDS rounds of concurrent kill-session (last) vs new-session"

$survived = 0
for ($i = 1; $i -le $ROUNDS; $i++) {
  & $P new-session -d -s "race_a$i" -x 100 -y 30
  Start-Sleep -m 1800                              # let warm standby establish

  # Fire both at once: A's kill-session scans + retires while the racer's
  # new-session claims. No ordering is imposed — the OS schedules the race.
  $procKill = Start-Process -FilePath $P -ArgumentList "kill-session","-t","race_a$i" -NoNewWindow -PassThru
  $procNew  = Start-Process -FilePath $P -ArgumentList "new-session","-d","-s","race_b$i","-x","100","-y","30" -NoNewWindow -PassThru
  $procKill.WaitForExit(15000) | Out-Null
  $procNew.WaitForExit(15000) | Out-Null
  Start-Sleep -m 1200

  # The invariant D2 must uphold: the racer's session server EXISTS and is
  # reachable — eventually. "Eventually" matters: every psmux CLI invocation
  # runs a registry-wide stale-port cleanup at startup (main.rs run_main),
  # and that cleanup can transiently reap a NEWBORN server's .port between
  # its bind-time write and its command loop serving AUTH (the pre-existing
  # "startup stale-port tax"; see test_startup_stale_port_tax.rs). The server
  # itself survives and its 5s registry self-heal restores the entry, so we
  # retry past one self-heal period. A claimant genuinely stranded by a
  # retire (the bug this test guards) never materializes a session at all
  # and still fails every retry.
  $alive = $false
  $tookRetries = 0
  for ($try = 0; $try -lt 16; $try++) {
    & $P -t "race_b$i" list-panes *> $null
    if ($LASTEXITCODE -eq 0 -and (Test-Path "$REG\race_b$i.port")) { $alive = $true; $tookRetries = $try; break }
    Start-Sleep -m 500
  }
  if ($alive) {
    $survived++
    if ($tookRetries -gt 0) { I "round ${i}: racer reachable after $tookRetries retries (startup stale-port tax, self-healed)" }
  } else {
    F "round ${i}: racer session race_b$i never became reachable (new-session exit=$($procNew.ExitCode))"
    I "  registry: $((Get-ChildItem $REG -File -EA SilentlyContinue | ForEach-Object Name) -join '  ')"
  }

  & $P kill-session -t "race_b$i"                  # last real session -> warm retires
  Start-Sleep -m 1200
}

if ($survived -eq $ROUNDS) { P "new session survived the race in all $ROUNDS rounds" }
# NOTE: no per-round "warm count" bound here — Warm-Count matches LAUNCH
# command lines, which claimed (renamed) servers also match, so it cannot
# distinguish a transient claim from a leak. Boundedness is covered by
# repro_warm_server_leak.ps1; this script asserts the end state instead.

# A warm whose pointer was consumed mid-race (e.g. a retire that reached it
# during its own startup window) survives pointerless and re-heals its
# registry entry within one 5s self-heal tick — making it a normal, REUSABLE
# standby, not a leak. kill-session's retirement is best-effort under that
# concurrency, so the leak-freedom property to assert HERE is: every
# test-born warm is either already retired or still reachable and retirable.
# The STRICT guarantee — the last kill-session itself retires the warm — is
# asserted under sequential conditions in test_registry_sandbox_e2e.ps1;
# this fallback exists only for the concurrent case and logs an INFO line
# whenever it actually fires, so silent regressions still show up in output.
Start-Sleep -Seconds 6
if (Test-Path "$REG\__warm__.port") {
  $wp = [int](Get-Content "$REG\__warm__.port").Trim()
  $wk = (Get-Content "$REG\__warm__.key" -EA SilentlyContinue | Select-Object -First 1)
  if ($wk) {
    $r = (Send-AuthedCommand $wp $wk.Trim() "retire-warm").Trim() -replace "`n", " / "
    I "post-test survivor warm retired: $r"
  }
  Start-Sleep -m 1500
}
$leftover = Warm-Born-Since $testStart
if ($leftover -eq 0) { P "every test-born warm process was retired or retirable (0 remain)" }
else { F "warm process leak: $leftover test-born warm server(s) alive and unreachable for retirement" }

Remove-Item -Recurse -Force $REG -EA SilentlyContinue
Remove-Item Env:\PSMUX_REGISTRY_DIR -EA SilentlyContinue

Write-Host "`nPassed=$pass Failed=$fail"
if ($fail -eq 0) { Write-Host "PASS test_warm_retire_race_e2e" } else { exit 1 }
