$ErrorActionPreference = "Stop"
$env:PSMUX_ALLOW_ELEVATED = "1"
$P = Join-Path $PSScriptRoot "..\target\release\psmux.exe"
$S = "evte2e"
& $P kill-server 2>$null; Start-Sleep -m 500

# --- cursor + dispatch + wait-event (fast-completion race: task finishes before wait starts)
& $P new-session -d -s $S -x 100 -y 30
Start-Sleep -m 800
$cur = (& $P -t $S cursor).Trim()
if ($cur -notmatch '^[0-9a-f]+:[0-9a-f]+:\d+$') { throw "bad cursor: $cur" }

& $P -t $S split-window -d          # %2 spawns
& $P -t "${S}:%2" send-keys "exit" Enter
Start-Sleep -Seconds 2               # let it FULLY exit BEFORE we start waiting (replay must catch it)
$ev = & $P -t $S wait-event --pane %2 --name pane-exited --after $cur --timeout 10000
if ($LASTEXITCODE -ne 0) { throw "wait-event exit $LASTEXITCODE (race not caught by replay)" }
if ($ev -notmatch '"name":"pane-exited"') { throw "wrong event: $ev" }

# --- notify from inside a pane reaches an outside waiter
$cur2 = (& $P -t $S cursor).Trim()
& $P -t "${S}:%1" send-keys "& '$P' notify --done" Enter
$ev2 = & $P -t $S wait-event --name agent-done --after $cur2 --timeout 10000
if ($LASTEXITCODE -ne 0) { throw "agent-done not received: exit $LASTEXITCODE" }

# --- wait-event timeout path
$cur3 = (& $P -t $S cursor).Trim()
& $P -t $S wait-event --name agent-done --after $cur3 --timeout 1500 | Out-Null
if ($LASTEXITCODE -ne 2) { throw "expected timeout exit 2, got $LASTEXITCODE" }

# --- cursor mismatch path
& $P -t $S wait-event --name agent-done --after "deadbeef:deadbeef:1" --timeout 1500 | Out-Null
if ($LASTEXITCODE -ne 4) { throw "expected mismatch exit 4, got $LASTEXITCODE" }

# --- capture-pane --settle returns completed output
# capture-pane -p returns one array element per line, so -notmatch against
# the array applies element-wise (returns the NON-matching lines, which is
# truthy whenever the capture has more than one line) — join before matching.
& $P -t "${S}:%1" send-keys "echo SETTLED-MARKER" Enter
$cap = & $P -t "${S}:%1" capture-pane -p --settle 300 --settle-timeout 5000
if (($cap -join "`n") -notmatch "SETTLED-MARKER") { throw "settle capture missed output" }

# --- capture-pane -t %N --settle targets the RIGHT pane, not whatever is
# active. %1 stays active/focused throughout; %2 (already split above) gets a
# marker only it should show. Regression test for the settle-probe temp-focus
# bug: PaneDataVersion(None) probes used to consume the FocusPaneTemp restore
# and the final capture landed on %1 instead of the -t %2 target.
& $P -t "${S}:%1" send-keys "echo ACTIVE-PANE-MARKER" Enter
Start-Sleep -m 300
& $P -t "${S}:%2" send-keys "echo NONACTIVE-MARKER" Enter
$cap2 = & $P -t "${S}:%2" capture-pane -p --settle 300 --settle-timeout 5000
$joined2 = $cap2 -join "`n"
if ($joined2 -notmatch "NONACTIVE-MARKER") { throw "settle capture on -t %2 missed its own marker: $joined2" }
if ($joined2 -match "ACTIVE-PANE-MARKER") { throw "settle capture on -t %2 leaked the active pane's content: $joined2" }

& $P kill-server 2>$null
Write-Host "PASS test_agent_events_e2e"
