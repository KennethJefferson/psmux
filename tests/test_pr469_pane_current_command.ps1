# PR #469: pane_current_command from shell-integration OSC sequences.
#
# End-to-end proof through the REAL runtime path:
#   pane child process writes OSC bytes to stdout
#     -> ConPTY -> psmux pane vt100 parser (perform.rs) records shell_command
#       -> #{pane_current_command} (format.rs cascade) returns it.
#
# Each emitter also sleeps so the shell stays "busy" (no new prompt 133;A) during
# the query window, making the assertion deterministic regardless of the host
# shell's prompt.

$ErrorActionPreference = "Continue"
$PSMUX = (Get-Command psmux -EA Stop).Source
$psmuxDir = "$env:USERPROFILE\.psmux"
$SESSION = "pr469_pcc"
$emitDir = "$env:TEMP\pr469_emit"
$script:TestsPassed = 0
$script:TestsFailed = 0

function Write-Pass($msg) { Write-Host "  [PASS] $msg" -ForegroundColor Green; $script:TestsPassed++ }
function Write-Fail($msg) { Write-Host "  [FAIL] $msg" -ForegroundColor Red; $script:TestsFailed++ }

function Cleanup {
    & $PSMUX kill-session -t $SESSION 2>&1 | Out-Null
    Start-Sleep -Milliseconds 500
    Remove-Item "$psmuxDir\$SESSION.*" -Force -EA SilentlyContinue
}

# Poll pane_current_command until it matches (or timeout), so we do not depend on
# a fixed sleep for output propagation.
function Wait-PCC {
    param([string]$Expected, [int]$TimeoutMs = 8000)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $last = ""
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        $last = (& $PSMUX display-message -t $SESSION -p '#{pane_current_command}' 2>&1 | Out-String).Trim()
        if ($last -eq $Expected) { return @{ Ok=$true; Val=$last } }
        Start-Sleep -Milliseconds 150
    }
    return @{ Ok=$false; Val=$last }
}

function Get-PCC {
    (& $PSMUX display-message -t $SESSION -p '#{pane_current_command}' 2>&1 | Out-String).Trim()
}

# Build emitter scripts. Each writes exact OSC bytes then sleeps to hold state.
New-Item -ItemType Directory -Path $emitDir -Force | Out-Null

# Scenario 1: OSC 133;C;cmdline_url= (kitty fish / psmux pwsh snippet)
@'
$e = [char]27
[Console]::Out.Write("$e]133;C;cmdline_url=copilot%20--yolo$e\")
[Console]::Out.Flush()
Start-Sleep -Seconds 8
'@ | Set-Content -Path "$emitDir\emit_cmdline_url.ps1" -Encoding ASCII

# Scenario 2: OSC 1337;SetUserVar=WEZTERM_PROG=<base64> (iTerm2 / WezTerm)
# base64("htop") = aHRvcA==
@'
$e = [char]27
[Console]::Out.Write("$e]1337;SetUserVar=WEZTERM_PROG=aHRvcA==$e\")
[Console]::Out.Flush()
Start-Sleep -Seconds 8
'@ | Set-Content -Path "$emitDir\emit_wezterm.ps1" -Encoding ASCII

# Scenario 3: OSC 633;E;<command> (VS Code shell integration)
@'
$e = [char]27
[Console]::Out.Write("$e]633;E;Get-ChildItem$e\")
[Console]::Out.Flush()
Start-Sleep -Seconds 8
'@ | Set-Content -Path "$emitDir\emit_vscode.ps1" -Encoding ASCII

# Scenario 4: set a command, then clear with OSC 133;D (command done -> idle)
@'
$e = [char]27
[Console]::Out.Write("$e]133;C;cmdline_url=make%20test$e\")
[Console]::Out.Flush()
Start-Sleep -Seconds 3
[Console]::Out.Write("$e]133;D$e\")
[Console]::Out.Flush()
Start-Sleep -Seconds 8
'@ | Set-Content -Path "$emitDir\emit_set_then_clear.ps1" -Encoding ASCII

# === SETUP ===
Cleanup
& $PSMUX new-session -d -s $SESSION
Start-Sleep -Seconds 3
& $PSMUX has-session -t $SESSION 2>$null
if ($LASTEXITCODE -ne 0) { Write-Fail "Session creation failed"; exit 1 }

Write-Host "`n=== PR #469 pane_current_command E2E ===" -ForegroundColor Cyan

# Baseline: fresh idle pane should fall back to the process-tree/shell heuristic,
# never the empty string.
Write-Host "`n[Baseline] idle pane uses heuristic fallback" -ForegroundColor Yellow
$base = Get-PCC
if ($base -and $base.Length -gt 0) { Write-Pass "Fallback non-empty: '$base'" }
else { Write-Fail "Fallback empty" }

# === TEST 1: cmdline_url= (percent-decoded) ===
Write-Host "`n[Test 1] OSC 133;C;cmdline_url=copilot%20--yolo" -ForegroundColor Yellow
& $PSMUX send-keys -t $SESSION "& '$emitDir\emit_cmdline_url.ps1'" Enter 2>&1 | Out-Null
$r1 = Wait-PCC -Expected "copilot --yolo"
if ($r1.Ok) { Write-Pass "pane_current_command = 'copilot --yolo' (percent-decoded)" }
else { Write-Fail "expected 'copilot --yolo', got '$($r1.Val)'" }
# Let the emitter's sleep finish before next scenario.
Start-Sleep -Seconds 9

# === TEST 2: WEZTERM_PROG base64 ===
Write-Host "`n[Test 2] OSC 1337;SetUserVar=WEZTERM_PROG=<base64 htop>" -ForegroundColor Yellow
& $PSMUX send-keys -t $SESSION "& '$emitDir\emit_wezterm.ps1'" Enter 2>&1 | Out-Null
$r2 = Wait-PCC -Expected "htop"
if ($r2.Ok) { Write-Pass "pane_current_command = 'htop' (base64-decoded)" }
else { Write-Fail "expected 'htop', got '$($r2.Val)'" }
Start-Sleep -Seconds 9

# === TEST 3: 633;E VS Code ===
Write-Host "`n[Test 3] OSC 633;E;Get-ChildItem" -ForegroundColor Yellow
& $PSMUX send-keys -t $SESSION "& '$emitDir\emit_vscode.ps1'" Enter 2>&1 | Out-Null
$r3 = Wait-PCC -Expected "Get-ChildItem"
if ($r3.Ok) { Write-Pass "pane_current_command = 'Get-ChildItem'" }
else { Write-Fail "expected 'Get-ChildItem', got '$($r3.Val)'" }
Start-Sleep -Seconds 9

# === TEST 4: set then clear (133;D returns to fallback) ===
Write-Host "`n[Test 4] OSC 133;C then 133;D clears back to heuristic" -ForegroundColor Yellow
& $PSMUX send-keys -t $SESSION "& '$emitDir\emit_set_then_clear.ps1'" Enter 2>&1 | Out-Null
$r4set = Wait-PCC -Expected "make test" -TimeoutMs 3000
if ($r4set.Ok) { Write-Pass "set phase: 'make test' captured" }
else { Write-Fail "set phase: expected 'make test', got '$($r4set.Val)'" }
# After 133;D, value must no longer be 'make test' (fell back to heuristic).
$cleared = $false
$sw = [System.Diagnostics.Stopwatch]::StartNew()
while ($sw.ElapsedMilliseconds -lt 8000) {
    $v = Get-PCC
    if ($v -ne "make test" -and $v.Length -gt 0) { $cleared = $true; $clearedVal = $v; break }
    Start-Sleep -Milliseconds 150
}
if ($cleared) { Write-Pass "clear phase: 133;D reverted to heuristic ('$clearedVal')" }
else { Write-Fail "clear phase: still 'make test' after 133;D" }
Start-Sleep -Seconds 6

# === TEARDOWN + TUI verification ===
Cleanup

Write-Host "`n$('='*50)" -ForegroundColor Cyan
Write-Host "Win32 TUI VISUAL VERIFICATION" -ForegroundColor Cyan
$SESSION = "pr469_tui"
Cleanup
$proc = Start-Process -FilePath $PSMUX -ArgumentList "new-session","-s",$SESSION -PassThru
Start-Sleep -Seconds 4
& $PSMUX has-session -t $SESSION 2>$null
if ($LASTEXITCODE -eq 0) { Write-Pass "TUI: visible session booted" } else { Write-Fail "TUI: session not booted" }
& $PSMUX send-keys -t $SESSION "& '$emitDir\emit_cmdline_url.ps1'" Enter 2>&1 | Out-Null
$rt = Wait-PCC -Expected "copilot --yolo"
if ($rt.Ok) { Write-Pass "TUI: pane_current_command from OSC in live window" }
else { Write-Fail "TUI: expected 'copilot --yolo', got '$($rt.Val)'" }
& $PSMUX kill-session -t $SESSION 2>&1 | Out-Null
try { Stop-Process -Id $proc.Id -Force -EA SilentlyContinue } catch {}
Cleanup
Remove-Item $emitDir -Recurse -Force -EA SilentlyContinue

Write-Host "`n=== Results ===" -ForegroundColor Cyan
Write-Host "  Passed: $($script:TestsPassed)" -ForegroundColor Green
Write-Host "  Failed: $($script:TestsFailed)" -ForegroundColor $(if ($script:TestsFailed -gt 0) { "Red" } else { "Green" })
exit $script:TestsFailed
