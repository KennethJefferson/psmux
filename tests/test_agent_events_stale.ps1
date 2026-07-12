$ErrorActionPreference = "Stop"
$env:PSMUX_ALLOW_ELEVATED = "1"
$P = Join-Path $PSScriptRoot "..\target\release\psmux.exe"
$S = "evstale"
& $P kill-server 2>$null; Start-Sleep -m 500
& $P new-session -d -s $S
Start-Sleep -m 800

# Capture %1's identity env, then respawn %1 so that identity goes stale.
& $P -t "${S}:%1" send-keys "`$env:PSMUX_PANE_INSTANCE + ':' + `$env:PSMUX_SESSION_UID > `$env:TEMP\psmux-stale-id.txt" Enter
Start-Sleep -Seconds 2
$old = (Get-Content "$env:TEMP\psmux-stale-id.txt").Trim().Split(':')
& $P -t "${S}:%1" respawn-pane -k
Start-Sleep -Seconds 1

# Replay the OLD identity from outside (simulates a delayed hook from the dead process).
$cur = (& $P -t $S cursor).Trim()
$json = '{"pane_id":1,"pane_instance":' + $old[0] + ',"session_uid":"' + $old[1] + '","name":"agent-done","title_len":0,"content":null}'
# The server's wire tokenizer (parse_command_line) treats bare double-quotes
# as toggling its own quoting state and STRIPS them — sending raw JSON as a
# bare token corrupts it before it reaches serde_json. The real CLI (see
# main.rs wait-event/notify) wraps the JSON arg in single quotes for exactly
# this reason; do the same here since we're driving the wire directly.
$wireJson = "'" + $json + "'"
# drive the wire directly (stale caller has no live pane env):
$port = Get-Content "$env:USERPROFILE\.psmux\$S.port"; $key = Get-Content "$env:USERPROFILE\.psmux\$S.key"
$c = New-Object Net.Sockets.TcpClient("127.0.0.1", [int]$port)
$w = New-Object IO.StreamWriter($c.GetStream()); $r = New-Object IO.StreamReader($c.GetStream())
$w.WriteLine("AUTH $key"); $w.WriteLine("notify-event $wireJson"); $w.Flush()
$null = $r.ReadLine()               # OK
$verdict = $r.ReadLine()
$c.Close()
if ($verdict -ne "STALE") { throw "expected STALE, got: $verdict" }

# The stale attempt must NOT satisfy an agent-done waiter, but MUST appear as stale-notify.
& $P -t $S wait-event --name agent-done --after $cur --timeout 1500 | Out-Null
if ($LASTEXITCODE -ne 2) { throw "stale notify satisfied an agent-done wait!" }
& $P -t $S wait-event --name stale-notify --after $cur --timeout 5000 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "stale-notify event missing" }

# notify OUTSIDE any pane: silent no-op success
$env:PSMUX_PANE_INSTANCE = $null; $env:PSMUX_SESSION_UID = $null
& $P notify --done
if ($LASTEXITCODE -ne 0) { throw "outside notify must exit 0" }

# notify with TMUX_PANE set but PSMUX_PANE_INSTANCE absent: this is the
# warm-claimed-initial-pane signature (see docs/agent-events.md warm caveat).
# Must still exit 0 (silent-no-op contract preserved) but print exactly one
# diagnostic line to stderr so it doesn't read as psmux silently swallowing
# a real failure.
$env:TMUX_PANE = "%1"
$env:PSMUX_PANE_INSTANCE = $null
$env:PSMUX_SESSION_UID = $null
$stderrFile = "$env:TEMP\psmux-warm-notify-stderr.txt"
& $P notify --done 2> $stderrFile
if ($LASTEXITCODE -ne 0) { throw "warm-claimed-pane notify must exit 0" }
$stderrText = (Get-Content $stderrFile -Raw)
if ($stderrText -notmatch "no identity") { throw "expected warm-pane diagnostic on stderr, got: $stderrText" }
Remove-Item $stderrFile -ErrorAction SilentlyContinue
$env:TMUX_PANE = $null

# case-folded protected env cannot override TMUX_PANE
# Panes at this point: %1 (respawned in place, id reused) + %2 (split-window
# below creates the first NEW pane id since respawn-pane -k reuses the
# existing slot rather than allocating a new one) — NOT %3.
& $P -t $S set-environment tmux_pane FAKE
& $P -t $S split-window -d
Start-Sleep -m 800
& $P -t "${S}:%2" send-keys "`$env:TMUX_PANE > `$env:TEMP\psmux-envprot.txt" Enter
Start-Sleep -Seconds 2
if ((Get-Content "$env:TEMP\psmux-envprot.txt").Trim() -eq "FAKE") { throw "protected env overridden!" }

& $P kill-server 2>$null
Write-Host "PASS test_agent_events_stale"
