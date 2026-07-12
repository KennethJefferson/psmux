use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{PtySize, native_pty_system};
use ratatui::prelude::*;

use crate::types::{AppState, Mode, Pane, Node, LayoutKind, DragState, Window, FocusDir};
use crate::tree::{active_pane, active_pane_mut, compute_rects, compute_split_borders,
    split_sizes_at, adjust_split_sizes, get_split_mut, resize_all_panes};
use crate::pane::{detect_shell, build_default_shell, set_tmux_env};
use crate::copy_mode::{enter_copy_mode, exit_copy_mode, scroll_copy_up, scroll_copy_down, scroll_pane_scrollback, yank_selection};
use crate::platform::mouse_inject;

/// Mouse debug logger — writes to ~/.psmux/mouse_debug.log when
/// PSMUX_MOUSE_DEBUG=1 is set.
fn mouse_log(msg: &str) {
    use std::sync::LazyLock;
    static ENABLED: LazyLock<bool> = LazyLock::new(|| {
        std::env::var("PSMUX_MOUSE_DEBUG").unwrap_or_default() == "1"
    });
    if !*ENABLED { return; }

    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    if n > 2000 { return; }

    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_default();
    let path = format!("{}/.psmux/mouse_debug.log", home);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {}", chrono::Local::now().format("%H:%M:%S%.3f"), msg);
    }
}

/// Convert screen coordinates to 0-based pane-local coordinates.
/// No border offset — panes are borderless (tmux-style).
fn pane_inner_cell_0based(area: Rect, abs_x: u16, abs_y: u16) -> (i16, i16) {
    let col = abs_x as i16 - area.x as i16;
    let row = abs_y as i16 - area.y as i16;
    (col, row)
}

/// Convert screen coordinates to 1-based pane-local coordinates.
fn pane_inner_cell(area: Rect, abs_x: u16, abs_y: u16) -> (u16, u16) {
    let col = abs_x.saturating_sub(area.x) + 1;
    let row = abs_y.saturating_sub(area.y) + 1;
    (col, row)
}

/// Map mouse coordinates from a client's terminal space to the server's effective
/// layout space.  When a client's terminal is larger or smaller than the effective
/// size used for layout computation, raw pixel coordinates don't match pane boundaries.
/// This ratio-based mapping is a "good enough" fallback for any interaction not yet
/// handled by client-side semantic commands.
fn map_client_coords(app: &AppState, x: u16, y: u16) -> (u16, u16) {
    let cid = match app.latest_client_id {
        Some(id) => id,
        None => return (x, y),
    };
    let (cw, ch) = match app.client_sizes.get(&cid) {
        Some(&size) => size,
        None => return (x, y),
    };
    let ew = app.last_window_area.width;
    let eh = app.last_window_area.height;
    if cw == ew && ch == eh {
        return (x, y);
    }
    let mx = if cw > 0 { ((x as u32) * (ew as u32) / (cw as u32)) as u16 } else { x };
    let my = if ch > 0 { ((y as u32) * (eh as u32) / (ch as u32)) as u16 } else { y };
    (mx.min(ew.saturating_sub(1)), my.min(eh.saturating_sub(1)))
}

/// Write a mouse event to the child PTY using the encoding the child requested.
pub fn write_mouse_event_remote(master: &mut dyn std::io::Write, button: u8, col: u16, row: u16, press: bool, enc: vt100::MouseProtocolEncoding) {
    match enc {
        vt100::MouseProtocolEncoding::Sgr => {
            let ch = if press { 'M' } else { 'm' };
            let _ = write!(master, "\x1b[<{};{};{}{}", button, col, row, ch);
            let _ = master.flush();
        }
        _ => {
            if press {
                let cb = (button + 32) as u8;
                let cx = ((col as u8).min(223)) + 32;
                let cy = ((row as u8).min(223)) + 32;
                let _ = master.write_all(&[0x1b, b'[', b'M', cb, cx, cy]);
                let _ = master.flush();
            }
        }
    }
}

/// Inject a mouse event into a pane via Windows Console API (WriteConsoleInputW).
///
/// For native Windows console apps: WriteConsoleInputW injects MOUSE_EVENT records
/// that ReadConsoleInput returns.  This works for apps like pstop, Far Manager, etc.
fn inject_mouse(pane: &mut Pane, col: i16, row: i16, button_state: u32, event_flags: u32) -> bool {
    if pane.child_pid.is_none() {
        pane.child_pid = mouse_inject::get_child_pid(&*pane.child);
    }
    if let Some(pid) = pane.child_pid {
        mouse_inject::send_mouse_event(pid, col, row, button_state, event_flags, false)
    } else {
        false
    }
}

/// Returns true if the window's foreground process is a VT bridge (wsl, ssh)
/// that needs VT mouse injection instead of Console API mouse injection.
fn is_vt_bridge(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("wsl") || lower.contains("ssh")
}

/// Permissive TUI detection for hover events — matches layout.rs heuristic.
///
/// Returns true when the last row of the pane screen has non-blank content,
/// which indicates a fullscreen app (status bar, menu bar, etc.).
///
/// This is deliberately less strict than `is_fullscreen_tui()`:
///   - `is_fullscreen_tui()` also requires the cursor in the bottom 3 rows,
///     which fails for apps like opencode whose cursor sits at a mid-screen
///     text input.
///   - For hover events, false positives are harmless — shells ignore bare
///     motion (SGR button 35).  False negatives break TUI hover (opencode,
///     etc.), so we use the permissive check.
pub(crate) fn screen_has_tui_content(pane: &Pane) -> bool {
    if let Ok(parser) = pane.term.lock() {
        let screen = parser.screen();
        if screen.alternate_screen() {
            return true;
        }
        let last_row = pane.last_rows.saturating_sub(1);
        for col in 0..pane.last_cols.min(80) {
            if let Some(cell) = screen.cell(last_row, col) {
                let t = cell.contents();
                if !t.is_empty() && t != " " {
                    return true;
                }
            }
        }
    }
    false
}

/// Check if the pane is likely running a fullscreen TUI app (htop, vim, etc.)
/// by detecting alternate screen buffer usage.
///
/// ConPTY never passes DECSET 1049h (alternate screen) to the output pipe,
/// so `screen.alternate_screen()` is always false.  Use the same heuristic
/// as layout.rs: if the last row of the screen has non-blank content, the
/// pane is running a fullscreen app.
pub(crate) fn is_fullscreen_tui(pane: &Pane) -> bool {
    if let Ok(parser) = pane.term.lock() {
        let screen = parser.screen();
        // Fast check: if the parser reports alternate screen, trust it
        if screen.alternate_screen() {
            return true;
        }

        // Issue #381: a plain shell whose output has merely filled the screen
        // is NOT a fullscreen TUI. Under git bash the fill heuristic below
        // false-positives once enough output fills the bottom rows with the
        // prompt at the cursor, so psmux forwarded mouse motion to the shell,
        // which echoed it as raw SGR text ("15M65;61;..."). If the foreground
        // process is a shell, skip the heuristic and report "not fullscreen".
        //
        // Gate on `== Some(true)`, NOT `.is_some()`: foreground_is_shell returns
        // Some(false) for a genuine NON-shell fullscreen app (nvim, htop), and
        // that case must still fall through to the heuristic that #285 relies on
        // for mouse support on ConPTY builds that strip the mouse DECSETs. Using
        // `.is_some()` here fires for Some(false) too and would suppress
        // detection of real TUIs, regressing #285.
        let foreground_is_shell = pane
            .child_pid
            .and_then(crate::platform::process_info::foreground_is_shell)
            == Some(true);

        if foreground_is_shell {
            return false;
        }

        // Heuristic: check if many of the last rows are non-blank AND the
        // cursor is near the bottom.  Fullscreen TUI apps fill the entire
        // screen and keep the cursor near the bottom (status bars, menus).
        // A shell after `dir` may have content on the last row, but the
        // cursor sits at the current prompt line — not necessarily at the
        // bottom — and the rows below the cursor are blank.
        let rows = pane.last_rows;
        if rows < 3 { return false; }
        let (cursor_row, _) = screen.cursor_position();
        let last_row = rows.saturating_sub(1);
        // Cursor must be in the bottom 3 rows for a fullscreen TUI
        if cursor_row < last_row.saturating_sub(2) {
            return false;
        }
        // Check that at least 3 of the last 4 rows have non-blank content
        let check_rows = 4u16.min(rows);
        let mut filled = 0u16;
        for r in (last_row + 1 - check_rows)..=last_row {
            let mut has_content = false;
            for col in 0..pane.last_cols.min(40) { // only check first 40 cols
                if let Some(cell) = screen.cell(r, col) {
                    let t = cell.contents();
                    if !t.is_empty() && t != " " {
                        has_content = true;
                        break;
                    }
                }
            }
            if has_content { filled += 1; }
        }
        return filled >= 3;
    }
    false
}

/// Check if the child process in this pane wants to receive mouse events.
///
/// Uses a three-tier detection strategy:
///
///   1. **mouse_protocol_mode** (DECSET 1000/1002/1003) — authoritative for
///      VT bridge children (WSL, SSH) where escape sequences pass through.
///   2. **alternate_screen** (DECSET 1049h) — works on Windows 11+ where
///      ConPTY passes DECSET 1049h to the output stream.
///   3. **is_fullscreen_tui heuristic** — fallback for older Windows 10
///      builds where ConPTY strips both DECSET 1000 and DECSET 1049h.
///      Detects fullscreen TUI apps (nvim, htop, vim) by checking that the
///      last rows are filled and the cursor is near the bottom.
///
/// Without tier 3, native TUI apps on older Windows never receive mouse
/// events because ConPTY makes both tier 1 and tier 2 return false.
/// (fixes #285, regression from commit 719e604)
pub(crate) fn pane_wants_mouse(pane: &Pane) -> bool {
    if let Ok(parser) = pane.term.lock() {
        let screen = parser.screen();
        // Tier 1: did the child enable mouse protocol? (VT bridge children)
        if screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None {
            return true;
        }
        // Tier 2: alternate screen active (newer ConPTY passes DECSET 1049h)
        if screen.alternate_screen() {
            return true;
        }
    }
    // Tier 3: heuristic for older ConPTY that strips DECSET 1049h —
    // detect fullscreen TUI apps by screen content analysis.
    is_fullscreen_tui(pane)
}

/// Stricter than `pane_wants_mouse`, used ONLY for the scroll-wheel decision.
///
/// The wheel must auto-enter copy mode for an ordinary shell pane (tmux parity,
/// issue #360).  `pane_wants_mouse`'s tier-3 `is_fullscreen_tui` content
/// heuristic returns true for a normal shell that has filled the screen with
/// the prompt sitting at the bottom, which wrongly forwarded the wheel to the
/// shell (it ignores SGR wheel) instead of entering copy mode.  For scroll we
/// only forward when the child RELIABLY wants the mouse: it enabled a mouse
/// protocol (e.g. nvim `set mouse=a`) or is on the alternate screen.  TUI apps
/// that genuinely consume the wheel satisfy one of these even on older ConPTY
/// (the mouse protocol DECSETs are not stripped); apps that satisfy neither do
/// not interpret the wheel anyway, so copy-mode scrollback is the right thing.
pub(crate) fn pane_wants_scroll_forward(pane: &Pane) -> bool {
    if let Ok(parser) = pane.term.lock() {
        let screen = parser.screen();
        if screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None {
            return true;
        }
        if screen.alternate_screen() {
            return true;
        }
    }
    false
}

/// Strict check for hover/motion events.  Returns true only when the child
/// has EXPLICITLY enabled mouse motion tracking (DECSET 1002 ButtonMotion or
/// DECSET 1003 AnyMotion).
///
/// Unlike `pane_wants_mouse()`, this does NOT use alt-screen or fullscreen
/// heuristics.  Sending unsolicited SGR motion sequences to apps that haven't
/// enabled mouse tracking (e.g. nvim without `set mouse=a`, or any TUI app
/// that only uses alt-screen for rendering) corrupts their input and makes
/// them appear hung.  (fixes #296)
pub(crate) fn pane_wants_hover(pane: &Pane) -> bool {
    if let Ok(parser) = pane.term.lock() {
        let screen = parser.screen();
        matches!(screen.mouse_protocol_mode(),
            vt100::MouseProtocolMode::ButtonMotion | vt100::MouseProtocolMode::AnyMotion)
    } else {
        false
    }
}

/// Detect whether a pane has a VT bridge descendant (wsl.exe, ssh.exe, etc.)
/// by walking the process tree.  Result is cached for 2 seconds per pane
/// to avoid expensive CreateToolhelp32Snapshot on every mouse event.
fn detect_vt_bridge(pane: &mut Pane) -> bool {
    // Check cache first (2 second TTL)
    if let Some((ts, cached)) = pane.vt_bridge_cache {
        if ts.elapsed().as_secs() < 2 {
            return cached;
        }
    }
    // Ensure child_pid is resolved
    if pane.child_pid.is_none() {
        pane.child_pid = mouse_inject::get_child_pid(&*pane.child);
    }
    let result = if let Some(pid) = pane.child_pid {
        crate::platform::process_info::has_vt_bridge_descendant(pid)
    } else {
        false
    };
    pane.vt_bridge_cache = Some((std::time::Instant::now(), result));
    result
}

/// Detect whether the child's console has ENABLE_MOUSE_INPUT (0x0010) set.
///
/// When true, the child reads MOUSE_EVENT records via ReadConsoleInputW
/// (crossterm/ratatui apps like pstop, claude).  When false, the child
/// reads input as text / VT sequences (nvim, vim, opencode).
///
/// Result is cached for 2 seconds per pane.
fn detect_mouse_input(pane: &mut Pane) -> bool {
    if let Some((ts, cached)) = pane.mouse_input_cache {
        if ts.elapsed().as_secs() < 2 {
            return cached;
        }
    }
    if pane.child_pid.is_none() {
        pane.child_pid = mouse_inject::get_child_pid(&*pane.child);
    }
    let result = if let Some(pid) = pane.child_pid {
        mouse_inject::query_mouse_input_enabled(pid).unwrap_or(false)
    } else {
        false
    };
    pane.mouse_input_cache = Some((std::time::Instant::now(), result));
    result
}

/// Helper: inject SGR mouse via WriteConsoleInputW KEY_EVENT records.
///
/// Used ONLY for WSL/SSH bridge children where the PTY pipe doesn't reach
/// the remote TUI.  For native ConPTY children, use write_mouse_to_pty().
fn inject_sgr_mouse(pane: &mut Pane, col: i16, row: i16, vt_button: u8, press: bool) -> bool {
    let vt_col = (col + 1).max(1) as u16;
    let vt_row = (row + 1).max(1) as u16;
    let ch = if press { 'M' } else { 'm' };
    let sgr_seq = format!("\x1b[<{};{};{}{}", vt_button, vt_col, vt_row, ch);
    mouse_log(&format!("  -> Console VT injection (KEY_EVENTs): seq={:?}", sgr_seq));
    if pane.child_pid.is_none() {
        pane.child_pid = mouse_inject::get_child_pid(&*pane.child);
    }
    if let Some(pid) = pane.child_pid {
        let ok = mouse_inject::send_vt_sequence(pid, sgr_seq.as_bytes());
        mouse_log(&format!("  -> Console VT inject result: {}", ok));
        ok
    } else {
        false
    }
}

/// Write a SGR mouse event to the pane's PTY master pipe.
///
/// This is the same mechanism Windows Terminal uses: write VT SGR mouse
/// escape sequences directly to the ConPTY input pipe.  ConPTY/conhost
/// then automatically:
///  - Translates SGR → MOUSE_EVENT records for apps using ReadConsoleInputW
///    (crossterm/ratatui: pstop, claude, opencode, etc.)
///  - Passes VT through for apps reading text/VT input (nvim, vim)
///
/// This works universally for ALL native ConPTY children — no need to
/// distinguish between crossterm vs nvim.  (fixes #60)
fn write_mouse_to_pty(pane: &mut Pane, col: i16, row: i16, vt_button: u8, press: bool) {
    use std::io::Write as _;
    let vt_col = (col + 1).max(1) as u16;
    let vt_row = (row + 1).max(1) as u16;
    let ch = if press { b'M' } else { b'm' };
    // Stack-allocated buffer — avoids heap allocation per mouse event.
    // Max SGR sequence: ESC[<btn;col;rowM = ~20 bytes worst case.
    let mut buf = [0u8; 32];
    let len = {
        let mut cursor = std::io::Cursor::new(&mut buf[..]);
        let _ = write!(cursor, "\x1b[<{};{};{}{}", vt_button, vt_col, vt_row, ch as char);
        cursor.position() as usize
    };
    mouse_log(&format!("  -> PTY pipe SGR mouse: seq={:?}", std::str::from_utf8(&buf[..len]).unwrap_or("?")));
    let _ = pane.writer.write_all(&buf[..len]);
    let _ = pane.writer.flush();
}

/// Inject a mouse event into a pane using the best available method.
///
/// Architecture (mirrors Windows Terminal):
///
///   For native ConPTY children, write SGR mouse escape sequences directly
///   to the PTY master pipe (pane.writer).  This is the same mechanism
///   Windows Terminal uses.  ConPTY/conhost handles all translation:
///   - Apps using ReadConsoleInputW (crossterm/ratatui) get MOUSE_EVENT records
///   - Apps reading VT input (nvim/vim) get the SGR sequences directly
///
///   For WSL/SSH bridge children, bypass ConPTY using WriteConsoleInputW
///   with KEY_EVENT records, delivering escape sequences to the bridge
///   process (wsl.exe/ssh.exe) which relays them to the Linux PTY.
///
///   At shell prompts (no TUI), no mouse forwarding is needed — the shell
///   doesn't handle mouse events.  Callers should handle shell-level
///   behavior (right-click=paste, scroll=copy-mode) before calling this.
pub(crate) fn inject_mouse_combined(pane: &mut Pane, col: i16, row: i16, vt_button: u8, press: bool,
                          _button_state: u32, _event_flags: u32, win_name: &str) {
    let vt_bridge = detect_vt_bridge(pane);

    if vt_bridge {
        // WSL/SSH bridge — bypass ConPTY, inject as KEY_EVENT records.
        // The bridge (wsl.exe, ssh.exe) relays these to the Linux PTY.
        //
        // Gate on mouse_protocol_mode (tmux + Windows Terminal parity):
        // Only forward mouse events when the remote app has explicitly
        // enabled mouse tracking (DECSET 1000/1002/1003).  For VT bridge
        // children, VT escape sequences pass through unmodified, so
        // mouse_protocol_mode() accurately reflects the remote app's
        // actual mouse tracking state.
        //
        // Without this gate, SGR mouse sequences are injected as KEY_EVENT
        // records → ssh.exe/wsl.exe relays them as literal text → the
        // remote shell prints raw escape sequences at the prompt.
        // This is the root cause of issue #77 (mouse events leak as raw
        // text into SSH panes).
        let wants = pane.term.lock().ok()
            .map_or(false, |t| t.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None);
        if !wants {
            mouse_log(&format!("inject_mouse_combined: col={} row={} vt_btn={} press={} win={} vt_bridge=true -> SUPPRESSED (remote has no mouse tracking)",
                col, row, vt_button, press, win_name));
            return;
        }
        mouse_log(&format!("inject_mouse_combined: col={} row={} vt_btn={} press={} win={} vt_bridge=true -> WriteConsoleInputW KEY_EVENT injection",
            col, row, vt_button, press, win_name));
        inject_sgr_mouse(pane, col, row, vt_button, press);
    } else {
        // Native ConPTY child — write SGR mouse to PTY pipe.
        // This is the same mechanism Windows Terminal uses.
        // ConPTY translates SGR → MOUSE_EVENT for crossterm apps,
        // and passes VT through for nvim/vim.
        mouse_log(&format!("inject_mouse_combined: col={} row={} vt_btn={} press={} win={} -> PTY pipe SGR mouse (Windows Terminal method)",
            col, row, vt_button, press, win_name));
        write_mouse_to_pty(pane, col, row, vt_button, press);

        // For wheel events, also inject a Win32 MOUSE_EVENT record.
        //
        // Some TUI frameworks (Bubble Tea / Go apps like opencode) enable
        // VT input mode (ENABLE_VIRTUAL_TERMINAL_INPUT) for keyboard but
        // read mouse events as MOUSE_EVENT records via ReadConsoleInput.
        // When VTI is on, ConPTY passes the SGR mouse sequence through
        // as KEY_EVENT text instead of converting to MOUSE_EVENT, so the
        // app's ReadConsoleInput loop never sees a mouse event.
        //
        // The Win32 MOUSE_EVENT injection bypasses ConPTY entirely and
        // delivers the event directly to the child's console input buffer.
        //
        // This is done only for wheel events (not click/drag/hover) to
        // minimize risk of duplicate events for apps where ConPTY already
        // converts SGR to MOUSE_EVENT (e.g. crossterm with VTI off).
        // (fixes #277)
        if _event_flags & mouse_inject::MOUSE_WHEELED != 0 {
            mouse_log(&format!("  -> also injecting Win32 MOUSE_EVENT (wheel, fixes #277)"));
            inject_mouse(pane, col, row, _button_state, _event_flags);
        }
    }
}

/// Temporarily unzoom for an operation, saving the zoom state so it can be
/// restored via `pop_zoom()` afterwards (tmux push/pop semantics).
/// Returns true if zoom was active and was suspended.
pub fn push_zoom(app: &mut AppState) -> bool {
    if app.windows[app.active_idx].zoom_saved.is_some() {
        // Mark that we had zoom active, unzoom, but DON'T clear zoom_saved
        // — we move it to a temp slot so pop_zoom can re-apply it.
        unzoom_if_zoomed(app);
        true
    } else {
        false
    }
}

/// Re-apply zoom after a push_zoom operation (tmux push/pop semantics).
/// Only re-zooms if `was_zoomed` is true.
pub fn pop_zoom(app: &mut AppState, was_zoomed: bool) {
    if was_zoomed && app.windows[app.active_idx].zoom_saved.is_none() {
        toggle_zoom(app);
    }
}

/// If zoom is currently active, unzoom (restore saved sizes) and resize panes.
/// Returns true if zoom was active and was cancelled.
pub fn unzoom_if_zoomed(app: &mut AppState) -> bool {
    if let Some(saved) = app.windows[app.active_idx].zoom_saved.take() {
        let win = &mut app.windows[app.active_idx];
        for (p, sz) in saved.into_iter() {
            if let Some(Node::Split { sizes, .. }) = get_split_mut(&mut win.root, &p) { *sizes = sz; }
        }
        resize_all_panes(app);
        true
    } else {
        false
    }
}

pub fn toggle_zoom(app: &mut AppState) {
    let win = &mut app.windows[app.active_idx];
    if win.zoom_saved.is_none() {
        let mut saved: Vec<(Vec<usize>, Vec<u16>)> = Vec::new();
        for depth in 0..win.active_path.len() {
            let p = win.active_path[..depth].to_vec();
            if let Some(Node::Split { sizes, .. }) = get_split_mut(&mut win.root, &p) {
                let idx = win.active_path.get(depth).copied().unwrap_or(0);
                saved.push((p.clone(), sizes.clone()));
                for i in 0..sizes.len() { sizes[i] = if i == idx { 100 } else { 0 }; }
            }
        }
        win.zoom_saved = Some(saved);
    } else {
        if let Some(saved) = app.windows[app.active_idx].zoom_saved.take() {
            let win = &mut app.windows[app.active_idx];
            for (p, sz) in saved.into_iter() {
                if let Some(Node::Split { sizes, .. }) = get_split_mut(&mut win.root, &p) { *sizes = sz; }
            }
        }
    }
    // Resize all panes so child PTYs are notified of the new dimensions.
    // Without this, zoomed panes keep their pre-zoom size and child apps
    // (neovim, bottom, etc.) render in only half the screen. (issue #35)
    resize_all_panes(app);
}

/// Compute tab positions on the server side to match the client's status bar layout.
/// The client renders: "[session_name] idx: window_name idx: window_name ..."
/// NOTE: No longer called — tab clicks are now handled client-side with exact
/// rendered positions.  Kept for reference / potential embedded-mode use.
#[allow(dead_code)]
pub fn update_tab_positions(app: &mut AppState) {
    let mut tab_pos: Vec<(usize, u16, u16)> = Vec::new();
    let mut cursor_x: u16 = 0;
    // Session label: "[session_name] "
    let session_label_len = app.session_name.len() as u16 + 3; // '[' + name + ']' + ' '
    cursor_x += session_label_len;
    // Window tabs: "idx: window_name " for each window
    for (i, w) in app.windows.iter().enumerate() {
        let display_idx = app.win_display_index(i);
        let label = format!("{}: {} ", display_idx, w.name);
        let start_x = cursor_x;
        cursor_x += label.len() as u16;
        tab_pos.push((i, start_x, cursor_x));
    }
    app.tab_positions = tab_pos;
}

pub fn remote_mouse_down(app: &mut AppState, x: u16, y: u16) {
    let (x, y) = map_client_coords(app, x, y);
    // Status bar tab clicks are handled client-side via select-window.
    // Only handle pane focus and border resize here.
    let status_row = app.last_window_area.y + app.last_window_area.height;
    if y == status_row {
        return;
    }

    // Floating panes sit above the tiled layout: a click inside a float grabs
    // it (tmux moves/resizes floats by dragging) and gives it focus. The
    // bottom-right edge starts a resize; the body starts a move.
    {
        let ox = app.last_window_area.x;
        let oy = app.last_window_area.y;
        let hit = {
            let win = &app.windows[app.active_idx];
            win.floating.iter().enumerate().rev().find_map(|(i, fp)| {
                let x0 = ox + fp.x; let y0 = oy + fp.y;
                if x >= x0 && x < x0 + fp.w && y >= y0 && y < y0 + fp.h {
                    Some((i, x0, y0, fp.w, fp.h))
                } else { None }
            })
        };
        if let Some((i, x0, y0, w, h)) = hit {
            let on_edge = x >= x0 + w.saturating_sub(1) || y >= y0 + h.saturating_sub(1);
            let mode = if on_edge {
                crate::types::FloatDragMode::Resize
            } else {
                crate::types::FloatDragMode::Move { dx: x - x0, dy: y - y0 }
            };
            app.windows[app.active_idx].floating_focus = Some(i);
            app.float_drag = Some(crate::types::FloatDrag { index: i, mode });
            return;
        }
    }

    let win = &mut app.windows[app.active_idx];
    let mut rects: Vec<(Vec<usize>, Rect)> = Vec::new();
    compute_rects(&win.root, app.last_window_area, &mut rects);
    let mut active_area: Option<Rect> = None;
    for (path, area) in rects.iter() {
        if area.contains(ratatui::layout::Position { x, y }) {
            win.active_path = path.clone();
            // Update MRU for clicked pane (tmux parity #70)
            if let Some(pid) = crate::tree::get_active_pane_id(&win.root, path) {
                crate::tree::touch_mru(&mut win.pane_mru, pid);
            }
            active_area = Some(*area);
        }
    }

    if matches!(app.mode, Mode::CopyMode | Mode::CopySearch { .. }) {
        app.copy_anchor = None;
        if let Some(area) = active_area {
            let (row, col) = copy_cell_for_area(area, x, y);
            app.copy_pos = Some((row, col));
            app.copy_mouse_down_cell = Some((row, col));
        }
        return;
    }

    let mut on_border = false;
    // Skip border detection when zoomed — no visible borders (#82)
    let mut borders: Vec<(Vec<usize>, LayoutKind, usize, u16, u16)> = Vec::new();
    if win.zoom_saved.is_none() {
        compute_split_borders(&win.root, app.last_window_area, &mut borders);
    }
    let tol = 1u16;
    for (path, kind, idx, pos, total_px) in borders.iter() {
        match kind {
            LayoutKind::Horizontal => {
                if x >= pos.saturating_sub(tol) && x <= pos + tol { if let Some((left,right)) = split_sizes_at(&win.root, path.clone(), *idx) { app.drag = Some(DragState { split_path: path.clone(), kind: *kind, index: *idx, start_x: *pos, start_y: y, left_initial: left, _right_initial: right, total_pixels: *total_px }); } on_border = true; break; }
            }
            LayoutKind::Vertical => {
                if y >= pos.saturating_sub(tol) && y <= pos + tol { if let Some((left,right)) = split_sizes_at(&win.root, path.clone(), *idx) { app.drag = Some(DragState { split_path: path.clone(), kind: *kind, index: *idx, start_x: x, start_y: *pos, left_initial: left, _right_initial: right, total_pixels: *total_px }); } on_border = true; break; }
            }
        }
    }

    // Forward left-click only when active pane wants mouse input.
    if !on_border {
        if let Some(area) = active_area {
            let (col, row) = pane_inner_cell_0based(area, x, y);
            let win_name = win.name.clone();
            if let Some(active) = active_pane_mut(&mut win.root, &win.active_path) {
                if pane_wants_mouse(active) {
                    inject_mouse_combined(active, col, row, 0, true,
                        mouse_inject::FROM_LEFT_1ST_BUTTON_PRESSED, 0, &win_name);
                }
            }
        }
    }
}

pub fn remote_mouse_drag(app: &mut AppState, x: u16, y: u16) {
    let (x, y) = map_client_coords(app, x, y);

    // A floating-pane drag moves or resizes the grabbed float, following the
    // cursor. Runs before any tiled handling and short-circuits it.
    if let Some(fd) = app.float_drag {
        let ox = app.last_window_area.x;
        let oy = app.last_window_area.y;
        let win_w = app.last_window_area.width.max(10);
        let win_h = app.last_window_area.height.max(10);
        let win = &mut app.windows[app.active_idx];
        if let Some(fp) = win.floating.get_mut(fd.index) {
            match fd.mode {
                crate::types::FloatDragMode::Move { dx, dy } => {
                    let nx = x.saturating_sub(ox).saturating_sub(dx);
                    let ny = y.saturating_sub(oy).saturating_sub(dy);
                    let (cx, cy) = crate::floating::clamp_into(nx, ny, fp.w, fp.h, win_w, win_h);
                    fp.x = cx; fp.y = cy;
                }
                crate::types::FloatDragMode::Resize => {
                    let x0 = ox + fp.x;
                    let y0 = oy + fp.y;
                    fp.w = (x.saturating_sub(x0) + 1).max(3).min(win_w);
                    fp.h = (y.saturating_sub(y0) + 1).max(3).min(win_h);
                    let (cx, cy) = crate::floating::clamp_into(fp.x, fp.y, fp.w, fp.h, win_w, win_h);
                    fp.x = cx; fp.y = cy;
                    let inner_h = fp.h.saturating_sub(2).max(1);
                    let inner_w = fp.w.saturating_sub(2).max(1);
                    if fp.pane.last_rows != inner_h || fp.pane.last_cols != inner_w {
                        let _ = fp.pane.master.resize(portable_pty::PtySize { rows: inner_h, cols: inner_w, pixel_width: 0, pixel_height: 0 });
                        if let Ok(mut parser) = fp.pane.term.lock() { parser.screen_mut().set_size(inner_h, inner_w); }
                        fp.pane.last_rows = inner_h;
                        fp.pane.last_cols = inner_w;
                    }
                }
            }
        }
        return;
    }

    let win = &mut app.windows[app.active_idx];
    let mut rects: Vec<(Vec<usize>, Rect)> = Vec::new();
    compute_rects(&win.root, app.last_window_area, &mut rects);

    if matches!(app.mode, Mode::CopyMode | Mode::CopySearch { .. }) {
        if let Some((path, area)) = rects.iter().find(|(_, area)| area.contains(ratatui::layout::Position { x, y })) {
            win.active_path = path.clone();
            let (row, col) = copy_cell_for_area(*area, x, y);
            if app.copy_anchor.is_none() {
                // Only start selection when mouse moves to a different cell
                // than the click position. Prevents micro-drag jitter (#199).
                if app.copy_pos == Some((row, col)) {
                    return;
                }
                app.copy_anchor = Some(app.copy_pos.unwrap_or((row, col)));
                app.copy_anchor_scroll_offset = app.copy_scroll_offset;
                app.copy_selection_mode = crate::types::SelectionMode::Char;
            }
            app.copy_pos = Some((row, col));
        }
        return;
    }

    if let Some(d) = &app.drag {
        adjust_split_sizes(&mut win.root, d, x, y);
    } else {
        // Forward drag only when active pane wants mouse input.
        if let Some(area) = rects.iter().find(|(path, _)| *path == win.active_path).map(|(_, a)| *a) {
            let (col, row) = pane_inner_cell_0based(area, x, y);
            let win_name = win.name.clone();
            if let Some(active) = active_pane_mut(&mut win.root, &win.active_path) {
                if pane_wants_mouse(active) {
                    inject_mouse_combined(active, col, row, 32, true,
                        mouse_inject::FROM_LEFT_1ST_BUTTON_PRESSED, mouse_inject::MOUSE_MOVED, &win_name);
                }
            }
        }
    }
}

pub fn remote_mouse_up(app: &mut AppState, x: u16, y: u16) {
    let (x, y) = map_client_coords(app, x, y);
    // End any floating-pane drag.
    if app.float_drag.is_some() {
        app.float_drag = None;
        return;
    }
    let win = &mut app.windows[app.active_idx];
    let mut rects: Vec<(Vec<usize>, Rect)> = Vec::new();
    compute_rects(&win.root, app.last_window_area, &mut rects);

    if matches!(app.mode, Mode::CopyMode | Mode::CopySearch { .. }) {
        if let Some((path, area)) = rects.iter().find(|(_, area)| area.contains(ratatui::layout::Position { x, y })) {
            win.active_path = path.clone();
            let (row, col) = copy_cell_for_area(*area, x, y);
            app.copy_pos = Some((row, col));
        }
        // If mouse-up is within 1 cell of mouse-down, it was a plain click
        // (any anchor set by jittery drag events is spurious). Clear it. (#199)
        // Mouse jitter during a click can shift the cursor by 1 cell.
        let click_origin = app.copy_mouse_down_cell.take();
        if let (Some((dr, dc)), Some((ur, uc))) = (click_origin, app.copy_pos) {
            let row_diff = (dr as i32 - ur as i32).unsigned_abs();
            let col_diff = (dc as i32 - uc as i32).unsigned_abs();
            if row_diff <= 1 && col_diff <= 1 {
                app.copy_anchor = None;
                app.copy_pos = Some((dr, dc)); // snap to the original click position
                return;
            }
        }
        // Auto-yank if real selection exists (anchor != pos), else clear stale anchor
        if let (Some(a), Some(p)) = (app.copy_anchor, app.copy_pos) {
            if a != p {
                let _ = yank_selection(app);
            } else {
                app.copy_anchor = None;
            }
        }
        return;
    }

    // If we were dragging a border, resize all panes to match new layout
    let was_dragging = app.drag.is_some();
    app.drag = None;
    if was_dragging {
        resize_all_panes(app);
        return;
    }

    // Forward mouse release only when active pane wants mouse input.
    if let Some(area) = rects.iter().find(|(path, _)| *path == win.active_path).map(|(_, a)| *a) {
        let (col, row) = pane_inner_cell_0based(area, x, y);
        let win_name = win.name.clone();
        if let Some(active) = active_pane_mut(&mut win.root, &win.active_path) {
            if pane_wants_mouse(active) {
                inject_mouse_combined(active, col, row, 0, false,
                    0, 0, &win_name);
            }
        }
    }
}

/// Forward a non-left mouse button press/release to the child.
pub fn remote_mouse_button(app: &mut AppState, x: u16, y: u16, button: u8, press: bool) {
    let (x, y) = map_client_coords(app, x, y);
    let win = &mut app.windows[app.active_idx];
    let mut rects: Vec<(Vec<usize>, Rect)> = Vec::new();
    compute_rects(&win.root, app.last_window_area, &mut rects);
    if let Some(area) = rects.iter().find(|(path, _)| *path == win.active_path).map(|(_, a)| *a) {
        let (col, row) = pane_inner_cell_0based(area, x, y);
        let win_name = win.name.clone();
        if let Some(active) = active_pane_mut(&mut win.root, &win.active_path) {
            if pane_wants_mouse(active) {
                let sgr_btn = match button {
                    1 => 1u8, // middle
                    2 => 2u8, // right
                    _ => 0u8,
                };
                let button_state = if press {
                    match button {
                        1 => mouse_inject::FROM_LEFT_2ND_BUTTON_PRESSED,
                        2 => mouse_inject::RIGHTMOST_BUTTON_PRESSED,
                        _ => 0,
                    }
                } else {
                    0
                };
                inject_mouse_combined(active, col, row, sgr_btn, press,
                    button_state, 0, &win_name);
            }
        }
    }
}

/// Forward bare mouse motion (hover) to the child PTY.
///
/// Only forwarded when the child has EXPLICITLY enabled mouse motion
/// tracking (`pane_wants_hover`, DECSET 1002/1003).  Do NOT use the
/// permissive pane_wants_mouse() heuristic here: its is_fullscreen_tui
/// tier false-positives on a filled screen with a NON-shell foreground
/// (podman/docker interactive containers, discussion #349), spraying raw
/// SGR motion bytes (35;x;yM...) into the container tty as visible garbage.
/// This matches the local input path, which was fixed the same way in #296.
///
/// SGR button 35 = bare motion with no button held (WT parity).
/// Windows Terminal encodes hover as WM_MOUSEMOVE -> button 3 + 0x20 = 35.
///
/// Same-coordinate events are suppressed (Windows Terminal parity: the
/// terminal only sends motion when coordinates actually change).
pub fn remote_mouse_motion(app: &mut AppState, x: u16, y: u16) {
    let (x, y) = map_client_coords(app, x, y);
    // WT parity: suppress same-coordinate duplicates
    if app.last_hover_pos == Some((x, y)) {
        return;
    }
    app.last_hover_pos = Some((x, y));

    let win = &mut app.windows[app.active_idx];
    let mut rects: Vec<(Vec<usize>, Rect)> = Vec::new();
    compute_rects(&win.root, app.last_window_area, &mut rects);

    // Forward hover only when the child explicitly enabled motion tracking.
    // This avoids leaking raw SGR motion bytes (ESC[<35;...) into shell-style
    // prompts such as claudecode input boxes and container ttys (#349).
    mouse_log(&format!("remote_mouse_motion: x={} y={}", x, y));

    if let Some(area) = rects.iter().find(|(path, _)| *path == win.active_path).map(|(_, a)| *a) {
        let (col, row) = pane_inner_cell_0based(area, x, y);
        let win_name = win.name.clone();
        if let Some(active) = active_pane_mut(&mut win.root, &win.active_path) {
            if pane_wants_hover(active) {
                inject_mouse_combined(active, col, row, 35, true,
                    0, mouse_inject::MOUSE_MOVED, &win_name);
            }
        }
    }
}

fn wheel_cell_for_area(area: Rect, x: u16, y: u16) -> (u16, u16) {
    // Convert global terminal coordinates to 1-based pane-local coordinates (no border offset).
    let col = x.saturating_sub(area.x).min(area.width.saturating_sub(1)).saturating_add(1);
    let row = y.saturating_sub(area.y).min(area.height.saturating_sub(1)).saturating_add(1);
    (col, row)
}

fn copy_cell_for_area(area: Rect, x: u16, y: u16) -> (u16, u16) {
    // Convert global terminal coordinates to 0-based pane-local coordinates (no border offset).
    let col = x.saturating_sub(area.x).min(area.width.saturating_sub(1));
    let row = y.saturating_sub(area.y).min(area.height.saturating_sub(1));
    (row, col)
}

fn remote_scroll_wheel(app: &mut AppState, x: u16, y: u16, up: bool) {
    let (x, y) = map_client_coords(app, x, y);
    let mode_str = match &app.mode {
        Mode::Passthrough => "Passthrough",
        Mode::CopyMode => "CopyMode",
        Mode::CopySearch { .. } => "CopySearch",
        _ => "Other",
    };
    mouse_log(&format!("remote_scroll_wheel: x={} y={} up={} mode={}", x, y, up, mode_str));

    // Ignore scroll in popup mode — don't enter copy-mode (#110)
    if matches!(app.mode, Mode::PopupMode { .. }) {
        mouse_log("  -> popup mode, ignoring scroll");
        return;
    }

    // Handle scroll while already in copy mode
    if matches!(app.mode, Mode::CopyMode | Mode::CopySearch { .. }) {
        mouse_log("  -> already in copy mode, scrolling within");
        if up {
            scroll_copy_up(app, 3);
        } else {
            scroll_copy_down(app, 3);
            // Auto-exit copy mode when scrolled back to live output
            if app.copy_scroll_offset == 0 && app.copy_anchor.is_none() {
                exit_copy_mode(app);
            }
        }
        return;
    }

    // Determine target pane, switch focus, and check if child is a TUI app
    // that should receive scroll events.
    //
    // Detection strategy (same as pane_wants_mouse, fixes #285):
    //   1. alternate_screen() — authoritative on newer Windows 11+ ConPTY
    //   2. is_fullscreen_tui() heuristic — fallback for older Windows 10
    //      builds where ConPTY strips DECSET 1049h.
    //
    // Note: is_fullscreen_tui() may false-positive after `ls`/`dir` fills
    // the screen (preventing scroll-to-copy-mode briefly), but this is far
    // less harmful than completely breaking scroll in TUI apps like Neovim
    // on older Windows.
    let (child_in_alt_screen, target_area_opt, sgr_btn, button_state) = {
        let win = &mut app.windows[app.active_idx];
        let mut rects: Vec<(Vec<usize>, Rect)> = Vec::new();
        compute_rects(&win.root, app.last_window_area, &mut rects);

        let mut target_area: Option<Rect> = None;
        for (path, area) in &rects {
            if area.contains(ratatui::layout::Position { x, y }) {
                win.active_path = path.clone();
                target_area = Some(*area);
                break;
            }
        }
        if target_area.is_none() {
            target_area = rects
                .iter()
                .find(|(path, _)| *path == win.active_path)
                .map(|(_, area)| *area);
        }

        let alt = active_pane(&win.root, &win.active_path)
            .map_or(false, |p| pane_wants_mouse(p));
        let sgr_btn: u8 = if up { 64 } else { 65 };
        let wheel_delta: i16 = if up { 120 } else { -120 };
        let bs = ((wheel_delta as i32) << 16) as u32;
        (alt, target_area, sgr_btn, bs)
    };

    mouse_log(&format!("  -> alt_screen={}", child_in_alt_screen));

    if child_in_alt_screen {
        // Forward scroll to child TUI app (alternate screen = real TUI)
        mouse_log("  -> forwarding scroll to child TUI (alt screen)");
        let win = &mut app.windows[app.active_idx];
        let (col, row) = target_area_opt.map_or((0, 0), |area| pane_inner_cell_0based(area, x, y));
        let win_name = win.name.clone();
        if let Some(p) = active_pane_mut(&mut win.root, &win.active_path) {
            inject_mouse_combined(p, col, row, sgr_btn, true,
                button_state, mouse_inject::MOUSE_WHEELED, &win_name);
        }
    } else if up && app.scroll_enter_copy_mode {
        // Shell prompt — auto-enter copy mode and scroll up (tmux parity)
        mouse_log("  -> entering copy mode (shell scroll-up)");
        enter_copy_mode(app);
        scroll_copy_up(app, 3);
    } else if !app.scroll_enter_copy_mode {
        // scroll-enter-copy-mode off: scroll scrollback directly (#193)
        mouse_log("  -> direct scrollback (scroll-enter-copy-mode off)");
        scroll_pane_scrollback(app, 3, up);
    } else {
        mouse_log("  -> scroll-down at shell (no-op)");
    }
}

pub fn remote_scroll_up(app: &mut AppState, x: u16, y: u16) { remote_scroll_wheel(app, x, y, true); }
pub fn remote_scroll_down(app: &mut AppState, x: u16, y: u16) { remote_scroll_wheel(app, x, y, false); }

/// Handle a semantic mouse event from the client.
/// The client has already determined the target pane and computed pane-relative
/// coordinates, so no coordinate translation is needed.
pub fn handle_pane_mouse(app: &mut AppState, pane_id: usize, button: u8, col: i16, row: i16, press: bool) {
    // Find the pane by ID and focus it
    let win = &mut app.windows[app.active_idx];
    let mut found_path: Option<Vec<usize>> = None;
    let mut rects: Vec<(Vec<usize>, Rect)> = Vec::new();
    compute_rects(&win.root, app.last_window_area, &mut rects);
    for (path, _area) in &rects {
        if let Some(pid) = crate::tree::get_active_pane_id(&win.root, path) {
            if pid == pane_id {
                found_path = Some(path.clone());
                break;
            }
        }
    }

    let Some(path) = found_path else { return; };

    // Focus the target pane only on actual clicks (not drag/hover).
    // tmux behavior: click-to-focus, not focus-follows-mouse.
    let is_click = matches!(button, 0 | 1 | 2) && press;
    if is_click && win.active_path != path {
        win.active_path = path.clone();
        if let Some(pid) = crate::tree::get_active_pane_id(&win.root, &path) {
            crate::tree::touch_mru(&mut win.pane_mru, pid);
        }
    }

    // Handle copy mode: position cursor with pane-relative coordinates
    if matches!(app.mode, Mode::CopyMode | Mode::CopySearch { .. }) {
        let r = row.max(0) as u16;
        let c = col.max(0) as u16;
        if button == 0 && press {
            // Left press: position cursor, clear selection
            app.copy_anchor = None;
            app.copy_pos = Some((r, c));
            app.copy_mouse_down_cell = Some((r, c));
        } else if button == 32 {
            // Left drag: extend selection, but ignore same-cell micro-jitter (#199)
            if app.copy_anchor.is_none() {
                if app.copy_pos == Some((r, c)) {
                    return; // same cell as click, ignore jitter
                }
                app.copy_anchor = Some(app.copy_pos.unwrap_or((r, c)));
                app.copy_anchor_scroll_offset = app.copy_scroll_offset;
                app.copy_selection_mode = crate::types::SelectionMode::Char;
            }
            app.copy_pos = Some((r, c));
        } else if button == 0 && !press {
            // Left release: finalize position
            app.copy_pos = Some((r, c));
            // If close to the original click, treat as click (no selection) (#199)
            if let Some((dr, dc)) = app.copy_mouse_down_cell.take() {
                if (dr as i32 - r as i32).unsigned_abs() <= 1
                    && (dc as i32 - c as i32).unsigned_abs() <= 1
                {
                    app.copy_anchor = None;
                    app.copy_pos = Some((dr, dc));
                    return;
                }
            }
            // Auto-yank if real selection exists (anchor != pos)
            if let (Some(a), Some(p)) = (app.copy_anchor, app.copy_pos) {
                if a != p { let _ = yank_selection(app); }
            }
        }
        return;
    }

    // Forward mouse event to PTY if pane wants it.
    //
    // Bare motion (SGR button 35, no button held) requires the child to have
    // EXPLICITLY enabled motion tracking (pane_wants_hover, DECSET 1002/1003).
    // The permissive pane_wants_mouse() heuristic false-positives on a filled
    // screen with a non-shell foreground (podman/docker interactive containers,
    // discussion #349), which sprayed "35;x;yM" as visible garbage into the
    // container tty on every mouse move.  Clicks/drags/wheel keep the
    // permissive gate so TUI mouse support on ConPTY builds that strip the
    // DECSETs keeps working (#285).
    let win = &mut app.windows[app.active_idx];
    let win_name = win.name.clone();
    if let Some(pane) = active_pane_mut(&mut win.root, &win.active_path) {
        let wants = if button == 35 { pane_wants_hover(pane) } else { pane_wants_mouse(pane) };
        if wants {
            let button_state = match (button, press) {
                (0, true) => mouse_inject::FROM_LEFT_1ST_BUTTON_PRESSED,
                (1, true) => mouse_inject::FROM_LEFT_2ND_BUTTON_PRESSED,
                (2, true) => mouse_inject::RIGHTMOST_BUTTON_PRESSED,
                _ => 0,
            };
            let event_flags = if button == 32 || button == 35 { mouse_inject::MOUSE_MOVED } else { 0 };
            inject_mouse_combined(pane, col, row, button, press, button_state, event_flags, &win_name);
        }
    }
}

/// Handle a semantic scroll event targeted at a specific pane.
pub fn handle_pane_scroll(app: &mut AppState, pane_id: usize, up: bool) {
    // Ignore scroll in popup mode (#110)
    if matches!(app.mode, Mode::PopupMode { .. }) { return; }

    // Handle scroll while already in copy mode (coordinates irrelevant)
    if matches!(app.mode, Mode::CopyMode | Mode::CopySearch { .. }) {
        if up {
            scroll_copy_up(app, 3);
        } else {
            scroll_copy_down(app, 3);
            if app.copy_scroll_offset == 0 && app.copy_anchor.is_none() {
                exit_copy_mode(app);
            }
        }
        return;
    }

    // Focus the target pane
    let win = &mut app.windows[app.active_idx];
    let mut rects: Vec<(Vec<usize>, Rect)> = Vec::new();
    compute_rects(&win.root, app.last_window_area, &mut rects);
    for (path, _area) in &rects {
        if let Some(pid) = crate::tree::get_active_pane_id(&win.root, path) {
            if pid == pane_id {
                win.active_path = path.clone();
                break;
            }
        }
    }

    // Use the stricter scroll-forward check (mouse protocol or alternate
    // screen only).  The permissive pane_wants_mouse() heuristic misclassifies
    // a normal shell that has filled the screen (prompt at the bottom) as a TUI
    // app, so the wheel was forwarded to the shell instead of entering copy
    // mode (#360).
    let alt = active_pane(&win.root, &win.active_path)
        .map_or(false, |p| pane_wants_scroll_forward(p));

    if alt {
        // Forward scroll to TUI app
        let win = &mut app.windows[app.active_idx];
        let win_name = win.name.clone();
        let sgr_btn: u8 = if up { 64 } else { 65 };
        let wheel_delta: i16 = if up { 120 } else { -120 };
        let button_state = ((wheel_delta as i32) << 16) as u32;
        // Use center of pane for coordinates — some TUI frameworks
        // (Bubble Tea) may ignore events at position (0,0) if it's
        // outside the scrollable viewport.
        let pane_area = rects.iter()
            .find(|(p, _)| *p == win.active_path)
            .map(|(_, a)| *a);
        let (col, row) = pane_area.map_or((5, 5), |a| {
            ((a.width / 2) as i16, (a.height / 2) as i16)
        });
        if let Some(pane) = active_pane_mut(&mut win.root, &win.active_path) {
            inject_mouse_combined(pane, col, row, sgr_btn, true,
                button_state, mouse_inject::MOUSE_WHEELED, &win_name);
        }
    } else if up && app.scroll_enter_copy_mode {
        // Shell prompt — enter copy mode and scroll
        enter_copy_mode(app);
        scroll_copy_up(app, 3);
    } else if !app.scroll_enter_copy_mode {
        // scroll-enter-copy-mode off: scroll scrollback directly (#193)
        scroll_pane_scrollback(app, 3, up);
    }
}

/// Set split sizes at a given tree path during border drag.
pub fn handle_split_set_sizes(app: &mut AppState, path: &[usize], sizes: &[u16]) {
    let win = &mut app.windows[app.active_idx];
    let mut cur: &mut Node = &mut win.root;
    for &idx in path.iter() {
        match cur {
            Node::Split { children, .. } => {
                if idx < children.len() {
                    cur = &mut children[idx];
                } else {
                    return;
                }
            }
            Node::Leaf(_) => return,
        }
    }
    if let Node::Split { sizes: node_sizes, children, .. } = cur {
        if sizes.len() == children.len() && sizes.len() == node_sizes.len() {
            *node_sizes = sizes.to_vec();
        }
    }
}

/// Finalize a border resize: apply PTY resizes to match the new layout.
pub fn handle_split_resize_done(app: &mut AppState) {
    resize_all_panes(app);
}

pub fn swap_pane(app: &mut AppState, dir: FocusDir) -> bool {
    let win = &mut app.windows[app.active_idx];
    let mut rects: Vec<(Vec<usize>, Rect)> = Vec::new();
    compute_rects(&win.root, app.last_window_area, &mut rects);
    
    let mut active_idx = None;
    for (i, (path, _)) in rects.iter().enumerate() { 
        if *path == win.active_path { active_idx = Some(i); break; } 
    }
    let Some(ai) = active_idx else { return false; };
    let (_, arect) = &rects[ai];
    
    // Collect pane IDs for MRU-based tie-breaking (issue #70)
    let pane_ids: Vec<usize> = rects.iter().map(|(path, _)| {
        crate::tree::get_active_pane_id(&win.root, path).unwrap_or(usize::MAX)
    }).collect();
    // Pick the pane to swap with.  tmux defines `swap-pane -U`/`-D` by pane
    // *index* order, not spatial geometry: -U swaps with the previous pane and
    // -D with the next pane in the window's pane list, wrapping at the ends
    // (cmd-swap-pane.c uses TAILQ_PREV / TAILQ_NEXT with wrap to LAST / FIRST).
    // `rects` is already in pane-index order (DFS leaf order, same as
    // pane_index_in_window), so prev/next is simply ai-1 / ai+1 with wrap.
    // `-L`/`-R` have no tmux swap-pane equivalent (they are psmux extensions),
    // so they keep the spatial neighbour search. (issue #400)
    let n = rects.len();
    let target = match dir {
        FocusDir::Up   => Some(if ai == 0 { n - 1 } else { ai - 1 }),
        FocusDir::Down => Some(if ai + 1 >= n { 0 } else { ai + 1 }),
        _ => crate::input::find_best_pane_in_direction(&rects, ai, arect, dir, &pane_ids, &win.pane_mru)
            .or_else(|| crate::input::find_wrap_target(&rects, ai, arect, dir, &pane_ids, &win.pane_mru)),
    }.filter(|&ni| ni != ai); // single-pane window: nothing to swap with
    let mut swapped = false;
    if let Some(ni) = target {
        let active_path = rects[ai].0.clone();
        let target_path = rects[ni].0.clone();
        // Actually exchange the two panes in the layout tree (keeping the split
        // sizes) instead of merely moving focus.  This is the real swap-pane
        // behaviour expected from tmux.
        if crate::tree::swap_nodes(&mut win.root, &active_path, &target_path) {
            // Focus follows the pane that was just moved into the new slot.
            win.active_path = target_path;
            if let Some(focused_id) = crate::tree::get_active_pane_id(&win.root, &win.active_path) {
                crate::tree::touch_mru(&mut win.pane_mru, focused_id);
            }
            swapped = true;
        }
    }
    // Resize the moved panes' PTYs to fit their new slots (tmux re-lays-out
    // after a swap).  Without this the program keeps its old terminal size.
    if swapped { crate::tree::resize_all_panes(app); }
    swapped
}

/// Swap the active pane with the pane at an explicit tree `path`
/// (used by `swap-pane -t <target>`).  Geometry is preserved; focus follows
/// the moved pane to its new slot.
pub fn swap_pane_with_path(app: &mut AppState, target_path: Vec<usize>) -> bool {
    let swapped = {
        let win = &mut app.windows[app.active_idx];
        let active_path = win.active_path.clone();
        if active_path == target_path { false }
        else {
            if crate::tree::swap_nodes(&mut win.root, &active_path, &target_path) {
                win.active_path = target_path;
                if let Some(focused_id) = crate::tree::get_active_pane_id(&win.root, &win.active_path) {
                    crate::tree::touch_mru(&mut win.pane_mru, focused_id);
                }
                true
            } else { false }
        }
    };
    // Resize moved panes to fit their new slots (see swap_pane).
    if swapped { crate::tree::resize_all_panes(app); }
    swapped
}

/// Swap two explicit panes named by their tree paths (`swap-pane -s <src>
/// -t <dst>`, issue #442).  Neither pane needs to be the active one.  Geometry
/// is preserved (the layout nodes exchange slots).  Focus follows tmux
/// `cmd-swap-pane.c`: without `-d` the `-t` (dst) pane becomes active — after
/// the swap it occupies the src slot, so focus lands on `src_path`.  With `-d`
/// the previously active pane keeps focus, following it to its new slot.
pub fn swap_pane_between(app: &mut AppState, src_path: Vec<usize>, dst_path: Vec<usize>, detach: bool) -> bool {
    let swapped = {
        let win = &mut app.windows[app.active_idx];
        if src_path == dst_path { false }
        else {
            // Remember the focused pane id so `-d` can keep focus on it even if
            // it was one of the two panes being swapped.
            let active_id = crate::tree::get_active_pane_id(&win.root, &win.active_path);
            if crate::tree::swap_nodes(&mut win.root, &src_path, &dst_path) {
                if detach {
                    if let Some(aid) = active_id {
                        if let Some(p) = crate::tree::find_path_by_id(&win.root, aid) {
                            win.active_path = p;
                        }
                    }
                } else {
                    // tmux default: the -t pane becomes active; it now sits in
                    // the src slot after the exchange.
                    win.active_path = src_path.clone();
                }
                if let Some(fid) = crate::tree::get_active_pane_id(&win.root, &win.active_path) {
                    crate::tree::touch_mru(&mut win.pane_mru, fid);
                }
                true
            } else { false }
        }
    };
    // Resize moved panes to fit their new slots (see swap_pane).
    if swapped { crate::tree::resize_all_panes(app); }
    swapped
}

/// Resolve a tmux-style position token (e.g. `{top-right}`) to the path of the
/// pane occupying that corner/edge of the current window.  Layout-independent:
/// always finds whatever pane currently sits there.
pub fn pane_path_at_position(app: &AppState, token: &str) -> Option<Vec<usize>> {
    if app.windows.is_empty() { return None; }
    let area = app.last_window_area;
    let win = &app.windows[app.active_idx];
    let mut rects: Vec<(Vec<usize>, Rect)> = Vec::new();
    compute_rects(&win.root, area, &mut rects);
    resolve_position_token(token, area, &rects)
}

/// Map a tmux-style position token to the path of the pane covering that
/// corner/edge point.  Pure geometry, separated out so it can be unit-tested.
pub fn resolve_position_token(token: &str, area: Rect, rects: &[(Vec<usize>, Rect)]) -> Option<Vec<usize>> {
    if area.width == 0 || area.height == 0 { return None; }
    let x0 = area.x;
    let y0 = area.y;
    let xmax = area.x + area.width - 1;
    let ymax = area.y + area.height - 1;
    let xmid = area.x + area.width / 2;
    let ymid = area.y + area.height / 2;
    let (px, py) = match token {
        "{top-left}"     => (x0, y0),
        "{top-right}"    => (xmax, y0),
        "{bottom-left}"  => (x0, ymax),
        "{bottom-right}" => (xmax, ymax),
        "{top}"          => (xmid, y0),
        "{bottom}"       => (xmid, ymax),
        "{left}"         => (x0, ymid),
        "{right}"        => (xmax, ymid),
        _ => return None,
    };
    rects.iter()
        .find(|(_, r)| px >= r.x && px < r.x + r.width && py >= r.y && py < r.y + r.height)
        .map(|(p, _)| p.clone())
}

#[cfg(test)]
mod position_token_tests {
    use super::resolve_position_token;
    use ratatui::layout::Rect;
    fn layout() -> (Rect, Vec<(Vec<usize>, Rect)>) {
        // ABTOP top-left, SMALL bottom-left, BIG right (mirrors the user's panel).
        let area = Rect { x: 0, y: 0, width: 160, height: 40 };
        let rects = vec![
            (vec![0, 0], Rect { x: 0,  y: 0,  width: 79, height: 19 }),
            (vec![0, 1], Rect { x: 0,  y: 20, width: 79, height: 20 }),
            (vec![1],    Rect { x: 80, y: 0,  width: 80, height: 40 }),
        ];
        (area, rects)
    }
    #[test]
    fn top_right_finds_big_pane() {
        let (area, rects) = layout();
        assert_eq!(resolve_position_token("{top-right}", area, &rects), Some(vec![1]));
        assert_eq!(resolve_position_token("{bottom-right}", area, &rects), Some(vec![1]));
        assert_eq!(resolve_position_token("{right}", area, &rects), Some(vec![1]));
    }
    #[test]
    fn corners_left() {
        let (area, rects) = layout();
        assert_eq!(resolve_position_token("{top-left}", area, &rects), Some(vec![0, 0]));
        assert_eq!(resolve_position_token("{bottom-left}", area, &rects), Some(vec![0, 1]));
    }
    #[test]
    fn unknown_token_is_none() {
        let (area, rects) = layout();
        assert_eq!(resolve_position_token("{active}", area, &rects), None);
    }
}

#[cfg(test)]
mod swap_mru_tests {
    use super::swap_pane_with_path;
    use crate::proxy_pane::create_proxy_pane;
    use crate::types::{AppState, LayoutKind, Node, Window};
    use ratatui::layout::Rect;
    use std::net::{TcpListener, TcpStream};

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let accept_thr = std::thread::spawn(move || listener.accept().expect("accept").0);
        let client = TcpStream::connect(addr).expect("connect");
        let server = accept_thr.join().expect("join accept thread");
        (client, server)
    }

    fn proxy_pane(id: usize, rows: u16, cols: u16) -> crate::types::Pane {
        let (reader, _peer1) = tcp_pair();
        let (writer, _peer2) = tcp_pair();
        create_proxy_pane(
            reader,
            writer,
            "127.0.0.1:1".to_string(),
            "test-key".to_string(),
            "test-session".to_string(),
            id as u64,
            None,
            format!("pane-{}", id),
            rows,
            cols,
            id,
            None,
        ).expect("create proxy pane")
    }

    fn make_window_with_two_panes(left_id: usize, right_id: usize) -> Window {
        Window {
            root: Node::Split {
                kind: LayoutKind::Horizontal,
                sizes: vec![1, 1],
                children: vec![Node::Leaf(proxy_pane(left_id, 10, 5)), Node::Leaf(proxy_pane(right_id, 10, 5))],
            },
            active_path: vec![0],
            name: "w0".to_string(),
            id: 0,
            activity_flag: false,
            bell_flag: false,
            silence_flag: false,
            last_output_time: std::time::Instant::now(),
            last_seen_version: 0,
            manual_rename: false,
            layout_index: 0,
            pane_mru: vec![right_id, left_id],
            zoom_saved: None,
            linked_from: None,
            floating: Vec::new(),
            floating_focus: None,
        }
    }

    #[test]
    fn swap_with_path_updates_mru_for_focused_pane_after_swap() {
        let mut app = AppState::new("swap-mru".to_string());
        app.last_window_area = Rect { x: 0, y: 0, width: 10, height: 10 };
        app.windows.push(make_window_with_two_panes(11, 22));
        app.active_idx = 0;

        let swapped = swap_pane_with_path(&mut app, vec![1]);
        assert!(swapped, "swap should succeed");
        assert_eq!(app.windows[0].active_path, vec![1], "focus should follow moved active pane");
        assert_eq!(app.windows[0].pane_mru.first().copied(), Some(11), "MRU should be the focused pane id after swap");
    }
}

pub fn resize_pane_vertical(app: &mut AppState, amount: i16) {
    let win = &mut app.windows[app.active_idx];
    if win.active_path.is_empty() { return; }
    
    for depth in (0..win.active_path.len()).rev() {
        let parent_path = win.active_path[..depth].to_vec();
        if let Some(Node::Split { kind, sizes, .. }) = get_split_mut(&mut win.root, &parent_path) {
            if *kind == LayoutKind::Vertical {
                let idx = win.active_path[depth];
                if idx < sizes.len() {
                    if idx + 1 < sizes.len() {
                        let new_size = (sizes[idx] as i16 + amount).max(1) as u16;
                        let diff = new_size as i16 - sizes[idx] as i16;
                        sizes[idx] = new_size;
                        sizes[idx + 1] = (sizes[idx + 1] as i16 - diff).max(1) as u16;
                    } else if idx > 0 {
                        // tmux parity (#81): last child has no bottom border.
                        // Resize the previous sibling with the same amount so
                        // the border moves in the arrow direction.
                        let new_size = (sizes[idx - 1] as i16 + amount).max(1) as u16;
                        let diff = new_size as i16 - sizes[idx - 1] as i16;
                        sizes[idx - 1] = new_size;
                        sizes[idx] = (sizes[idx] as i16 - diff).max(1) as u16;
                    }
                }
                return;
            }
        }
    }
}

pub fn resize_pane_horizontal(app: &mut AppState, amount: i16) {
    let win = &mut app.windows[app.active_idx];
    if win.active_path.is_empty() { return; }
    
    for depth in (0..win.active_path.len()).rev() {
        let parent_path = win.active_path[..depth].to_vec();
        if let Some(Node::Split { kind, sizes, .. }) = get_split_mut(&mut win.root, &parent_path) {
            if *kind == LayoutKind::Horizontal {
                let idx = win.active_path[depth];
                if idx < sizes.len() {
                    if idx + 1 < sizes.len() {
                        let new_size = (sizes[idx] as i16 + amount).max(1) as u16;
                        let diff = new_size as i16 - sizes[idx] as i16;
                        sizes[idx] = new_size;
                        sizes[idx + 1] = (sizes[idx + 1] as i16 - diff).max(1) as u16;
                    } else if idx > 0 {
                        // tmux parity (#81): last child has no right border.
                        // Resize the previous sibling with the same amount so
                        // the border moves in the arrow direction.
                        let new_size = (sizes[idx - 1] as i16 + amount).max(1) as u16;
                        let diff = new_size as i16 - sizes[idx - 1] as i16;
                        sizes[idx - 1] = new_size;
                        sizes[idx] = (sizes[idx] as i16 - diff).max(1) as u16;
                    }
                }
                return;
            }
        }
    }
}

/// Absolute resize: set the active pane's share to an exact size.
/// axis is "x" (width/horizontal) or "y" (height/vertical).
pub fn resize_pane_absolute(app: &mut AppState, axis: &str, target: u16) {
    let win = &mut app.windows[app.active_idx];
    if win.active_path.is_empty() { return; }
    let target_kind = if axis == "x" { LayoutKind::Horizontal } else { LayoutKind::Vertical };
    for depth in (0..win.active_path.len()).rev() {
        let parent_path = win.active_path[..depth].to_vec();
        if let Some(Node::Split { kind, sizes, .. }) = get_split_mut(&mut win.root, &parent_path) {
            if *kind == target_kind {
                let idx = win.active_path[depth];
                if idx < sizes.len() {
                    let old = sizes[idx];
                    let new = target.max(1);
                    let diff = new as i16 - old as i16;
                    sizes[idx] = new;
                    // Absorb the difference from a neighbour
                    if idx + 1 < sizes.len() {
                        sizes[idx + 1] = (sizes[idx + 1] as i16 - diff).max(1) as u16;
                    } else if idx > 0 {
                        sizes[idx - 1] = (sizes[idx - 1] as i16 - diff).max(1) as u16;
                    }
                }
                return;
            }
        }
    }
}

pub fn rotate_panes(app: &mut AppState, reverse: bool) {
    let win = &mut app.windows[app.active_idx];
    match &mut win.root {
        Node::Split { children, .. } if children.len() >= 2 => {
            if reverse {
                // Rotate counter-clockwise: first element goes to end
                let first = children.remove(0);
                children.push(first);
            } else {
                // Rotate clockwise: last element goes to front
                let last = children.pop().unwrap();
                children.insert(0, last);
            }
        }
        _ => {}
    }
}

pub fn break_pane_to_window(app: &mut AppState) {
    let src_idx = app.active_idx;
    let src_path = app.windows[src_idx].active_path.clone();
    
    // Extract the active pane from the current window using tree operations
    let src_root = std::mem::replace(&mut app.windows[src_idx].root,
        Node::Split { kind: LayoutKind::Horizontal, sizes: vec![], children: vec![] });
    let (remaining, extracted) = crate::tree::extract_node(src_root, &src_path);
    
    if let Some(pane_node) = extracted {
        let src_empty = remaining.is_none();
        if let Some(rem) = remaining {
            app.windows[src_idx].root = rem;
            app.windows[src_idx].active_path = crate::tree::first_leaf_path(&app.windows[src_idx].root);
        }
        
        // Determine the window name from the pane
        let win_name = match &pane_node {
            Node::Leaf(p) => p.title.clone(),
            _ => format!("win {}", app.windows.len() + 1),
        };
        
        // Create new window containing the extracted pane
        let initial_mru = crate::tree::collect_pane_ids(&pane_node);
        app.windows.push(Window {
            root: pane_node,
            active_path: vec![],
            name: win_name,
            id: app.next_win_id,
            activity_flag: false,
            bell_flag: false,
            silence_flag: false,
            last_output_time: std::time::Instant::now(),
            last_seen_version: 0,
            manual_rename: false,
            layout_index: 0,
            pane_mru: initial_mru,
            zoom_saved: None,
            linked_from: None,
            floating: Vec::new(),
            floating_focus: None,
        });
        app.next_win_id += 1;
        app.on_window_appended();

        if src_empty {
            app.windows.remove(src_idx);
            app.on_window_removed(src_idx);
        }

        // Switch to the new window
        app.active_idx = app.windows.len() - 1;
    } else {
        // Extraction failed — restore
        if let Some(rem) = remaining {
            app.windows[src_idx].root = rem;
        }
    }
}

pub fn respawn_active_pane(app: &mut AppState, pty_system_ref: Option<&dyn portable_pty::PtySystem>, workdir: Option<&str>, kill: bool, command: Option<&str>, empty: bool) -> io::Result<()> {
    // tmux semantics: without -k, respawn only works on dead panes.
    // With -k, kill the running process first and respawn.
    {
        let win = &app.windows[app.active_idx];
        if let Some(pane) = crate::tree::active_pane(&win.root, &win.active_path) {
            if !pane.dead && !kill {
                return Err(io::Error::new(io::ErrorKind::Other, "pane still active"));
            }
        }
    }
    // If -k and pane is alive, kill the child process first
    if kill {
        let win = &mut app.windows[app.active_idx];
        if let Some(pane) = active_pane_mut(&mut win.root, &win.active_path) {
            if !pane.dead {
                crate::platform::process_kill::kill_process_tree(&mut pane.child);
                pane.dead = true;
            }
        }
    }

    // -E: respawn as an EMPTY pane (no command). Replace the pane in place with
    // a childless empty pane, keeping its id/title/size, and return.
    if empty {
        let win = &mut app.windows[app.active_idx];
        if let Some(pane) = active_pane_mut(&mut win.root, &win.active_path) {
            let (r, c, id, title) = (pane.last_rows, pane.last_cols, pane.id, pane.title.clone());
            if let Some(mut ep) = crate::popup::create_empty_pane(r.max(1), c.max(1), id) {
                ep.title = title;
                *pane = ep;
            }
        }
        return Ok(());
    }

    // Reuse provided PTY system or create one as fallback
    let owned_pty;
    let pty_system: &dyn portable_pty::PtySystem = if let Some(ps) = pty_system_ref {
        ps
    } else {
        owned_pty = native_pty_system();
        &*owned_pty
    };
    // Expand format variables like #{pane_current_path} at spawn time (#111).
    // Must happen before the mutable borrow of app.windows below.
    let expanded_shell = crate::format::expand_format(&app.default_shell, &app);
    // Respawn keeps the same pane id but must never reuse an old instance —
    // downstream event consumers use (pane_id, instance) to detect a respawn.
    // Allocated (and identity strings snapshotted) before the mutable borrow
    // of app.windows below.
    let new_instance = app.alloc_pane_instance();
    let session_uid = app.session_uid.clone();
    let bus_id = app.bus_id_string();

    let win = &mut app.windows[app.active_idx];
    let Some(pane) = active_pane_mut(&mut win.root, &win.active_path) else { return Ok(()); };
    let pane_id = pane.id;
    
    let size = PtySize { rows: pane.last_rows, cols: pane.last_cols, pixel_width: 0, pixel_height: 0 };
    let pair = pty_system.openpty(size).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("openpty error: {e}")))?;
    // Issue #399: honor an explicit `-- <command>` (e.g. Claude Code agent-teams
    // respawning a pane with the teammate launch command). Without a command,
    // fall back to the configured default shell (original behavior).
    let mut shell_cmd = if command.is_some() {
        crate::pane::build_command(command, app.env_shim, app.allow_predictions)
    } else if !expanded_shell.is_empty() {
        build_default_shell(&expanded_shell, app.env_shim, app.allow_predictions)
    } else {
        detect_shell()
    };
    crate::pane::apply_user_environment(&mut shell_cmd, &app.environment);
    set_tmux_env(&mut shell_cmd, pane_id, app.control_port, app.socket_name.as_deref(), &app.session_name, new_instance, &session_uid, &bus_id, app.claude_code_fix_tty, app.claude_code_force_interactive);
    if let Some(dir) = workdir {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        let expanded = dir.replace("~/", &format!("{}/", home))
            .replace("~\\", &format!("{}\\", home));
        shell_cmd.cwd(std::path::Path::new(&expanded));
    }
    let child = pair.slave.spawn_command(shell_cmd).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("spawn shell error: {e}")))?;
    // Close the slave handle immediately – required for ConPTY.
    drop(pair.slave);
    let term: Arc<Mutex<vt100::Parser>> = Arc::new(Mutex::new(vt100::Parser::new(size.rows, size.cols, app.history_limit)));
    let term_reader = term.clone();
    let reader = pair.master.try_clone_reader().map_err(|e| io::Error::new(io::ErrorKind::Other, format!("clone reader error: {e}")))?;
    
    let data_version = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dv_writer = data_version.clone();
    let cursor_shape = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::pane::CURSOR_SHAPE_UNSET));
    let cs_writer = cursor_shape.clone();
    
    let bell_pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bell_writer = bell_pending.clone();
    let cpr_pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cpr_writer = cpr_pending.clone();

    let output_ring = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    crate::pane::spawn_reader_thread(reader, term_reader, dv_writer, cs_writer, bell_writer, cpr_writer, output_ring.clone(), pane_id);
    pane.output_ring = output_ring;

    let mut pty_writer = pair.master.take_writer().map_err(|e| io::Error::new(io::ErrorKind::Other, format!("take writer error: {e}")))?;
    crate::pane::conpty_preemptive_dsr_response(&mut *pty_writer);

    pane.master = pair.master;
    pane.writer = pty_writer;
    pane.child = child;
    pane.term = term;
    pane.instance = new_instance;
    pane.data_version = data_version;
    pane.cursor_shape = cursor_shape;
    pane.bell_pending = bell_pending;
    pane.cpr_pending = cpr_pending;
    pane.child_pid = None;
    pane.vt_bridge_cache = None;
    pane.vti_mode_cache = None;
    pane.mouse_input_cache = None;
    pane.dead = false;
    pane.spawned_at = Some(std::time::Instant::now());

    Ok(())
}

/// Respawn a fresh default shell into a SPECIFIC pane (by window index + tree
/// path) that crashed shortly after spawn. Mirrors `respawn_active_pane`'s spawn
/// core but always uses the default shell and targets an arbitrary pane. Used by
/// the opt-in `@heal-crashed-panes` self-heal for issue #450, where a pwsh whose
/// PSReadLine is not the active reader FailFasts on its first ConPTY read right
/// after a warm-pane transplant, leaving a broken/empty window.
pub fn heal_respawn_pane(
    app: &mut AppState,
    pty_system_ref: &dyn portable_pty::PtySystem,
    win_idx: usize,
    path: &Vec<usize>,
) -> io::Result<()> {
    // Expand format vars (e.g. #{pane_current_path}) before the mutable borrow.
    let expanded_shell = crate::format::expand_format(&app.default_shell, &app);
    // A healed pane keeps its pane id but gets a FRESH shell process, so it
    // must never reuse the crashed shell's instance — downstream event
    // consumers use (pane_id, instance) to detect the swap. Same contract as
    // respawn_active_pane above; snapshotted before the mutable borrow.
    let new_instance = app.alloc_pane_instance();
    let session_uid = app.session_uid.clone();
    let bus_id = app.bus_id_string();

    let Some(win) = app.windows.get_mut(win_idx) else { return Ok(()); };
    let Some(pane) = active_pane_mut(&mut win.root, path) else { return Ok(()); };
    let pane_id = pane.id;
    let size = PtySize { rows: pane.last_rows.max(1), cols: pane.last_cols.max(1), pixel_width: 0, pixel_height: 0 };

    let pair = pty_system_ref.openpty(size).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("openpty error: {e}")))?;
    let mut shell_cmd = if !expanded_shell.is_empty() {
        build_default_shell(&expanded_shell, app.env_shim, app.allow_predictions)
    } else {
        detect_shell()
    };
    set_tmux_env(&mut shell_cmd, pane_id, app.control_port, app.socket_name.as_deref(), &app.session_name, new_instance, &session_uid, &bus_id, app.claude_code_fix_tty, app.claude_code_force_interactive);
    crate::pane::apply_user_environment(&mut shell_cmd, &app.environment);
    let child = pair.slave.spawn_command(shell_cmd).map_err(|e| io::Error::new(io::ErrorKind::Other, format!("spawn shell error: {e}")))?;
    drop(pair.slave);
    let child_pid = crate::platform::mouse_inject::get_child_pid(&*child);

    let term: Arc<Mutex<vt100::Parser>> = Arc::new(Mutex::new(vt100::Parser::new(size.rows, size.cols, app.history_limit)));
    let term_reader = term.clone();
    let reader = pair.master.try_clone_reader().map_err(|e| io::Error::new(io::ErrorKind::Other, format!("clone reader error: {e}")))?;
    let data_version = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dv_writer = data_version.clone();
    let cursor_shape = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::pane::CURSOR_SHAPE_UNSET));
    let cs_writer = cursor_shape.clone();
    let bell_pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bell_writer = bell_pending.clone();
    let cpr_pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cpr_writer = cpr_pending.clone();
    let output_ring = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    crate::pane::spawn_reader_thread(reader, term_reader, dv_writer, cs_writer, bell_writer, cpr_writer, output_ring.clone(), pane_id);

    let mut pty_writer = pair.master.take_writer().map_err(|e| io::Error::new(io::ErrorKind::Other, format!("take writer error: {e}")))?;
    crate::pane::conpty_preemptive_dsr_response(&mut *pty_writer);

    // Re-acquire the pane and swap in the fresh shell.
    let Some(win) = app.windows.get_mut(win_idx) else { return Ok(()); };
    let Some(pane) = active_pane_mut(&mut win.root, path) else { return Ok(()); };
    pane.master = pair.master;
    pane.writer = pty_writer;
    pane.child = child;
    pane.term = term;
    pane.instance = new_instance;
    pane.data_version = data_version;
    pane.cursor_shape = cursor_shape;
    pane.bell_pending = bell_pending;
    pane.cpr_pending = cpr_pending;
    pane.output_ring = output_ring;
    pane.child_pid = child_pid;
    pane.vt_bridge_cache = None;
    pane.vti_mode_cache = None;
    pane.mouse_input_cache = None;
    pane.dead = false;
    pane.spawned_at = Some(std::time::Instant::now());
    Ok(())
}

#[cfg(test)]
#[path = "../tests-rs/test_issue81_resize_direction.rs"]
mod test_issue81_resize_direction;

#[cfg(test)]
#[path = "../tests-rs/test_issue381_gitbash_fullscreen_falsepositive.rs"]
mod test_issue381_gitbash_fullscreen_falsepositive;

#[cfg(test)]
#[path = "../tests-rs/test_issue400_swap_pane_index_order.rs"]
mod test_issue400_swap_pane_index_order;

#[cfg(test)]
#[path = "../tests-rs/test_issue442_swap_pane_source.rs"]
mod test_issue442_swap_pane_source;

#[cfg(test)]
#[path = "../tests-rs/test_discussion349_podman_motion_leak.rs"]
mod test_discussion349_podman_motion_leak;
