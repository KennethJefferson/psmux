// Issue #381: git bash prints raw mouse escape sequences after running commands.
//
// ROOT CAUSE (reporter jassanw, PR #407):
//   `is_fullscreen_tui()` heuristic false-positives for an ordinary shell once
//   enough output has filled the bottom rows with the cursor near the bottom.
//   When it returns true, `pane_wants_mouse()` returns true, so psmux forwards
//   mouse-motion SGR sequences to the shell, which — being a plain shell that
//   never enabled a mouse protocol — echoes them as raw text
//   ("15M65;61;15M64;61...").  The reporter's own logs show "Filled = 3", i.e.
//   the heuristic's `filled >= 3` branch firing for a bash prompt.
//
// These tests exercise the REAL `is_fullscreen_tui` / `pane_wants_mouse` and the
// REAL `foreground_is_shell` process-tree walk with LIVE helper processes. The
// vt100 screen is filled deterministically the way ordinary command output
// fills it, decoupling the proof from MSYS/ConPTY rendering quirks that only
// occur when a raw test harness (not psmux) spawns git bash.
//
// Registered from src/window_ops.rs so it can call the pub(crate) helpers.

use super::*;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const GIT_BASH: &str = "C:\\Program Files\\Git\\bin\\bash.exe";

/// Build a vt100 parser whose screen is FILLED the way ordinary shell output
/// fills it: many lines scrolled off the top, content in the bottom rows, and
/// the prompt (cursor) sitting on the last row — exactly the state the reporter
/// and commenter (AJBuilder: "enough output to fill the screen") describe.
fn filled_parser(rows: u16, cols: u16) -> Arc<Mutex<vt100::Parser>> {
    let mut p = vt100::Parser::new(rows, cols, 0);
    // Print more lines than the screen has rows so the buffer scrolls.
    let mut bytes = Vec::new();
    for i in 1..=(rows as usize + 4) {
        bytes.extend_from_slice(format!("L{i} output line here\r\n").as_bytes());
    }
    // Prompt on the final row, cursor left sitting after it (no newline).
    bytes.extend_from_slice(b"user@host MINGW64 ~\r\n$ ");
    p.process(&bytes);
    Arc::new(Mutex::new(p))
}

/// An empty/fresh parser: prompt at the TOP, cursor near the top.
fn fresh_parser(rows: u16, cols: u16) -> Arc<Mutex<vt100::Parser>> {
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(b"user@host MINGW64 ~\r\n$ ");
    Arc::new(Mutex::new(p))
}

/// Assemble a Pane around a given parser + optional live child pid. Only the
/// fields `is_fullscreen_tui` / `pane_wants_mouse` read are meaningful; the pty
/// plumbing is a throwaway cmd.exe so the struct is valid.
fn make_pane(term: Arc<Mutex<vt100::Parser>>, rows: u16, cols: u16, child_pid: Option<u32>) -> crate::types::Pane {
    let pty = portable_pty::native_pty_system();
    let pair = pty
        .openpty(portable_pty::PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");
    let mut cmd = portable_pty::CommandBuilder::new("cmd.exe");
    cmd.arg("/c");
    cmd.arg("exit");
    let child = pair.slave.spawn_command(cmd).expect("spawn dummy");
    let writer = pair.master.take_writer().expect("writer");
    let epoch = Instant::now() - Duration::from_secs(2);
    crate::types::Pane {
        master: pair.master,
        writer,
        child,
        term,
        last_rows: rows,
        last_cols: cols,
        id: 0,
        instance: 0,
        title: "bash".to_string(),
        title_locked: false,
        child_pid,
        data_version: Arc::new(AtomicU64::new(0)),
        last_title_check: epoch,
        last_infer_title: epoch,
        dead: false,
        last_text_input: None,
        last_special_key: None,
        vt_bridge_cache: None,
        vti_mode_cache: None,
        mouse_input_cache: None,
        cursor_shape: Arc::new(AtomicU8::new(0)),
        bell_pending: Arc::new(AtomicBool::new(false)),
        cpr_pending: Arc::new(AtomicBool::new(false)),
        copy_state: None,
        pane_style: None,
        squelch_until: None,
        output_ring: Arc::new(Mutex::new(std::collections::VecDeque::new())),
    }
}

/// Spawn a live interactive git bash that sits at its prompt (no child of its
/// own). Its foreground leaf is bash itself → `foreground_is_shell` == Some(true).
/// Returns None if git bash is unavailable.
fn spawn_live_bash() -> Option<Child> {
    if !std::path::Path::new(GIT_BASH).exists() {
        return None;
    }
    Command::new(GIT_BASH)
        .args(["--norc", "-i"])
        .stdin(Stdio::piped())   // held open so bash blocks on read and stays alive
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// Spawn a live NON-shell foreground process (ping) to stand in for a genuine
/// fullscreen TUI as far as the process classifier is concerned:
/// `foreground_is_shell` == Some(false).
fn spawn_live_nonshell() -> Option<Child> {
    Command::new("ping.exe")
        .args(["-n", "60", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

// ─────────────────────────────────────────────────────────────────────────
// PART 1 — ROOT CAUSE: the heuristic false-positives on a filled shell screen
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn issue381_filled_shell_screen_is_misdetected_as_fullscreen_tui() {
    let term = filled_parser(10, 40);
    let pane = make_pane(term, 10, 40, None);

    let fullscreen = is_fullscreen_tui(&pane);
    let wants_mouse = pane_wants_mouse(&pane);
    eprintln!("[filled shell] is_fullscreen_tui={fullscreen} pane_wants_mouse={wants_mouse}");

    // The false positive: a plain shell whose output filled the screen is
    // classified as a fullscreen TUI, which is what forwards mouse motion to
    // the shell as the raw SGR text reported in #381.
    assert!(fullscreen, "root cause not reproduced: heuristic should false-positive on a filled shell screen");
    assert!(wants_mouse, "consequence: pane_wants_mouse must be true when the heuristic false-positives");
}

#[test]
fn issue381_fresh_shell_prompt_is_not_fullscreen() {
    // Control: prompt at the top → correctly NOT a fullscreen TUI. Isolates the
    // trigger to "screen filled + cursor at bottom".
    let term = fresh_parser(10, 40);
    let pane = make_pane(term, 10, 40, None);
    assert!(!is_fullscreen_tui(&pane), "a fresh top-of-screen prompt must not be a fullscreen TUI");
}

// ─────────────────────────────────────────────────────────────────────────
// PART 2 — the process classifier the fix relies on, exercised with LIVE pids
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn issue381_foreground_is_shell_classifies_live_processes() {
    // Live bash → Some(true); live ping → Some(false). This is the exact
    // distinction the fix must key on. If this test's assumptions are wrong,
    // every downstream conclusion is wrong, so pin them here.
    if let Some(mut bash) = spawn_live_bash() {
        std::thread::sleep(Duration::from_millis(400));
        let verdict = crate::platform::process_info::foreground_is_shell(bash.id());
        eprintln!("[classifier] live bash pid={} -> {:?}", bash.id(), verdict);
        let _ = bash.kill();
        let _ = bash.wait();
        assert_eq!(verdict, Some(true), "a live git bash foreground must classify as a shell");
    } else {
        eprintln!("git bash unavailable — skipping shell-classification assertion");
    }

    if let Some(mut ping) = spawn_live_nonshell() {
        std::thread::sleep(Duration::from_millis(400));
        let verdict = crate::platform::process_info::foreground_is_shell(ping.id());
        eprintln!("[classifier] live ping pid={} -> {:?}", ping.id(), verdict);
        let _ = ping.kill();
        let _ = ping.wait();
        assert_eq!(verdict, Some(false), "a live ping foreground must classify as NON-shell");
    }
}

// ─────────────────────────────────────────────────────────────────────────
// PART 3 — PROVE PR #407's `.is_some()` logic would REGRESS issue #285
// ─────────────────────────────────────────────────────────────────────────
//
// PR #407 gates on:   child_pid.and_then(foreground_is_shell).is_some()
// But foreground_is_shell returns Some(false) for a NON-shell fullscreen TUI,
// and `.is_some()` is TRUE for Some(false). So the PR returns "not fullscreen"
// for a real TUI too — killing mouse support that tier-3 exists to provide
// (#285). The correct predicate is `== Some(true)`.

#[test]
fn issue381_pr407_is_some_predicate_would_break_real_tui() {
    let ping = match spawn_live_nonshell() {
        Some(c) => c,
        None => { eprintln!("ping unavailable — skipping PR-regression proof"); return; }
    };
    let mut ping = ping;
    std::thread::sleep(Duration::from_millis(400));
    let pid = Some(ping.id());

    // Exactly the PR's expression:
    let pr407_flag = pid.and_then(crate::platform::process_info::foreground_is_shell).is_some();
    // The correct expression:
    let correct_flag = pid.and_then(crate::platform::process_info::foreground_is_shell) == Some(true);

    eprintln!("[pr-defect] non-shell foreground: pr407(.is_some)={pr407_flag}  correct(==Some(true))={correct_flag}");
    let _ = ping.kill();
    let _ = ping.wait();

    assert!(pr407_flag, "demonstrates the PR predicate fires even for a NON-shell (Some(false).is_some()==true)");
    assert!(!correct_flag, "the correct predicate must NOT fire for a non-shell foreground");
    // Net: with the PR as written, a genuine fullscreen TUI (Some(false)) would
    // be forced to `return false` from is_fullscreen_tui → regression of #285.
}

// ─────────────────────────────────────────────────────────────────────────
// PART 4 — the FIXED behavior contract (passes only after the corrected fix)
// ─────────────────────────────────────────────────────────────────────────
//
// After the corrected fix (`== Some(true)` gate), is_fullscreen_tui must:
//   • return FALSE for a filled screen whose foreground is a live shell  (#381)
//   • still return TRUE for a filled screen whose foreground is non-shell (#285)

#[test]
fn issue381_fixed_shell_foreground_not_fullscreen() {
    let bash = match spawn_live_bash() {
        Some(c) => c,
        None => { eprintln!("git bash unavailable — skipping fixed-behavior (shell) assertion"); return; }
    };
    let mut bash = bash;
    std::thread::sleep(Duration::from_millis(400));
    let term = filled_parser(10, 40);
    let pane = make_pane(term, 10, 40, Some(bash.id()));
    let fullscreen = is_fullscreen_tui(&pane);
    eprintln!("[fixed:shell] filled screen + live bash foreground -> is_fullscreen_tui={fullscreen}");
    let _ = bash.kill();
    let _ = bash.wait();
    assert!(!fullscreen, "FIX CONTRACT: filled shell screen must NOT be a fullscreen TUI once the foreground is a shell");
}

#[test]
fn issue381_fixed_nonshell_foreground_still_fullscreen() {
    let ping = match spawn_live_nonshell() {
        Some(c) => c,
        None => { eprintln!("ping unavailable — skipping fixed-behavior (non-shell) assertion"); return; }
    };
    let mut ping = ping;
    std::thread::sleep(Duration::from_millis(400));
    let term = filled_parser(10, 40);
    let pane = make_pane(term, 10, 40, Some(ping.id()));
    let fullscreen = is_fullscreen_tui(&pane);
    eprintln!("[fixed:nonshell] filled screen + live non-shell foreground -> is_fullscreen_tui={fullscreen}");
    let _ = ping.kill();
    let _ = ping.wait();
    assert!(fullscreen, "FIX CONTRACT: a filled non-shell screen must STILL be a fullscreen TUI (no #285 regression)");
}
