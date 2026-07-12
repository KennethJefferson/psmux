# Issue #450: new shells born broken (blank pane / pwsh FailFast flash)
#
# Root cause: the server's FreeConsole/AttachConsole dances (Ctrl+C delivery,
# mouse/VT injection) leave the process std handle slots dangling on freed,
# recycled handle values, and CreateProcessW stamped those into every newborn
# ConPTY child.  Fixed by console_state_lock() serialization plus parking the
# std handle slots on NULL around CreateProcessW in portable-pty-psmux.
#
# This test spawns handle-probe children (tests/handlecheck.cs) while a
# persistent TCP connection hammers send-keys C-c at another window.  Before
# the fix 21 of 25 storm births were corrupt; after the fix all must be GOOD.
$ErrorActionPreference = "Continue"
$PSMUX = (Get-Command psmux -EA Stop).Source
$SESSION = "test450std"
$psmuxDir = "$env:USERPROFILE\.psmux"
$script:TestsPassed = 0
$script:TestsFailed = 0

function Write-Pass($msg) { Write-Host "  [PASS] $msg" -ForegroundColor Green; $script:TestsPassed++ }
function Write-Fail($msg) { Write-Host "  [FAIL] $msg" -ForegroundColor Red; $script:TestsFailed++ }

$CHECKER = "$env:TEMP\psmux450_handlecheck.exe"
$csc = "C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
& $csc /nologo /optimize /out:$CHECKER "$PSScriptRoot\handlecheck.cs" 2>&1 | Out-Null
if (-not (Test-Path $CHECKER)) { Write-Fail "handlecheck.cs failed to compile"; exit 1 }

$LOG_CTRL = "$env:TEMP\psmux450_ctrl.log"
$LOG_STORM = "$env:TEMP\psmux450_storm.log"
Remove-Item $LOG_CTRL,$LOG_STORM -Force -EA SilentlyContinue

& $PSMUX kill-session -t $SESSION 2>&1 | Out-Null
Start-Sleep -Milliseconds 500
Remove-Item "$psmuxDir\$SESSION.*" -Force -EA SilentlyContinue

$env:PSMUX_NO_WARM = "1"   # cold spawns, matching the original crash captures
& $PSMUX new-session -d -s $SESSION
Start-Sleep -Seconds 3
& $PSMUX has-session -t $SESSION 2>$null
if ($LASTEXITCODE -ne 0) { Write-Fail "session creation"; exit 1 }

function Spawn-Checkers {
    param([string]$LogFile, [int]$N)
    for ($i = 0; $i -lt $N; $i++) {
        & $PSMUX new-window -t $SESSION -- $CHECKER $LogFile 2>&1 | Out-Null
        Start-Sleep -Milliseconds 700
    }
    Start-Sleep -Seconds 3
}

Write-Host "`n=== Issue #450 std-handle corruption tests ===" -ForegroundColor Cyan

Write-Host "[Test 1] Control: quiet spawns are healthy" -ForegroundColor Yellow
Spawn-Checkers $LOG_CTRL 5

Write-Host "[Test 2] Storm: spawns under send-keys C-c hammer stay healthy" -ForegroundColor Yellow
$port = (Get-Content "$psmuxDir\$SESSION.port" -Raw).Trim()
$key = (Get-Content "$psmuxDir\$SESSION.key" -Raw).Trim()
$hammer = Start-Job -ScriptBlock {
    param($port, $key, $sess)
    $tcp = [System.Net.Sockets.TcpClient]::new("127.0.0.1", [int]$port)
    $tcp.NoDelay = $true
    $stream = $tcp.GetStream()
    $writer = [System.IO.StreamWriter]::new($stream)
    $reader = [System.IO.StreamReader]::new($stream)
    $writer.Write("AUTH $key`n"); $writer.Flush()
    $null = $reader.ReadLine()
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt 30) {
        $writer.Write("send-keys -t ${sess}:0 C-c`n"); $writer.Flush()
        Start-Sleep -Milliseconds 15
    }
    $tcp.Close()
} -ArgumentList $port, $key, $SESSION

Start-Sleep -Milliseconds 500
Spawn-Checkers $LOG_STORM 15
Receive-Job -Job $hammer -Wait | Out-Null
Remove-Job $hammer -Force -EA SilentlyContinue

& $PSMUX kill-session -t $SESSION 2>&1 | Out-Null
$env:PSMUX_NO_WARM = $null

foreach ($pair in @(@("control",$LOG_CTRL,5), @("storm",$LOG_STORM,15))) {
    $name, $file, $expected = $pair
    if (-not (Test-Path $file)) { Write-Fail "${name}: checker never ran (no log)"; continue }
    $lines = @(Get-Content $file)
    $bad = @($lines | Where-Object { $_ -match "VERDICT=CORRUPT" })
    if ($lines.Count -lt $expected) { Write-Fail "${name}: only $($lines.Count)/$expected children reported" }
    elseif ($bad.Count -gt 0) {
        Write-Fail "${name}: $($bad.Count)/$($lines.Count) children born with corrupt std handles"
        $bad | Select-Object -First 3 | ForEach-Object { Write-Host "    $_" -ForegroundColor Yellow }
    }
    else { Write-Pass "${name}: all $($lines.Count) children born with healthy console std handles" }
}

Remove-Item $LOG_CTRL,$LOG_STORM,$CHECKER -Force -EA SilentlyContinue

Write-Host "`n=== Results ===" -ForegroundColor Cyan
Write-Host "  Passed: $($script:TestsPassed)" -ForegroundColor Green
Write-Host "  Failed: $($script:TestsFailed)" -ForegroundColor $(if ($script:TestsFailed -gt 0) { "Red" } else { "Green" })
exit $script:TestsFailed
