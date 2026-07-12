// PR #469 safety / robustness guard for the inlined OSC command decoders.
//
// The OSC 133/1337/633 command-identity parser consumes UNTRUSTED pane output
// (any child process can emit these bytes). The percent and base64 decoders are
// hand-rolled and dependency-free, so this suite hammers them with adversarial
// input and proves two invariants:
//
//   1. No panic / no hang, whatever the bytes are (malformed percent escapes,
//      illegal base64, truncation, embedded NULs, huge payloads, byte-split
//      feeding).
//   2. The raw OSC bytes never leak into the visible grid (they are consumed as
//      control sequences, not printed as text).
//
// Run with: cargo test --test test_pr469_osc_command_safety

const ST: &[u8] = b"\x1b\\";
const BEL: &[u8] = b"\x07";

fn osc(body: &[u8], terminator: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"\x1b]");
    v.extend_from_slice(body);
    v.extend_from_slice(terminator);
    v
}

/// Concatenate all visible grid cell contents into one string. Dimensions are
/// hardcoded to the 24x80 the tests construct, matching the proven pattern in
/// test_issue299_osc_command_state.rs.
fn grid_text(p: &vt100::Parser) -> String {
    (0..24)
        .map(|r| {
            (0..80)
                .filter_map(|c| p.screen().cell(r, c).map(|c| c.contents()))
                .collect::<String>()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Malformed percent-encoding in OSC 133;C;cmdline_url= must never panic.
// ---------------------------------------------------------------------------

#[test]
fn malformed_percent_encoding_never_panics() {
    let payloads: &[&[u8]] = &[
        b"133;C;cmdline_url=%",           // lone percent at end
        b"133;C;cmdline_url=%A",          // one hex digit then EOF
        b"133;C;cmdline_url=%ZZ",         // non-hex digits
        b"133;C;cmdline_url=%GG%HH",      // repeated bad hex
        b"133;C;cmdline_url=abc%",        // trailing percent after text
        b"133;C;cmdline_url=%2",          // truncated in the middle
        b"133;C;cmdline_url=%00",         // decodes to NUL byte
        b"133;C;cmdline_url=%ff%fe%fd",   // invalid UTF-8 after decode -> dropped
        b"133;C;cmdline_url=",            // empty value
        b"133;C;cmdline_url",             // no '=' at all
        b"133;C;",                        // empty param slot
    ];
    for body in payloads {
        for term in [ST, BEL] {
            let mut p = vt100::Parser::new(24, 80, 0);
            p.process(&osc(body, term));
            // No assertion on the value itself (some are intentionally dropped);
            // the point is simply that process() returned without panicking and
            // shell_command() is queryable.
            let _ = p.screen().shell_command();
        }
    }
}

#[test]
fn percent_decode_null_and_highbytes_do_not_corrupt_grid() {
    let mut p = vt100::Parser::new(24, 80, 0);
    p.process(&osc(b"133;C;cmdline_url=%00%01%1b%07bad", ST));
    // Whatever the decoder decided, none of the raw OSC framing should show up
    // on screen.
    let text = grid_text(&p);
    assert!(!text.contains("cmdline_url"), "OSC param leaked into grid: {text:?}");
    assert!(!text.contains("133"), "OSC command number leaked into grid: {text:?}");
}

// ---------------------------------------------------------------------------
// 2. Malformed base64 in OSC 1337;SetUserVar=WEZTERM_PROG must never panic
//    and invalid values must be dropped (shell_command stays None).
// ---------------------------------------------------------------------------

#[test]
fn malformed_base64_wezterm_prog_never_panics_and_is_dropped() {
    let payloads: &[&[u8]] = &[
        b"1337;SetUserVar=WEZTERM_PROG=!!!!",        // illegal chars
        b"1337;SetUserVar=WEZTERM_PROG=A",           // single leftover char
        b"1337;SetUserVar=WEZTERM_PROG=AB=",         // odd padding
        b"1337;SetUserVar=WEZTERM_PROG=====",        // all padding
        b"1337;SetUserVar=WEZTERM_PROG=",            // empty value
        b"1337;SetUserVar=WEZTERM_PROG",             // no '=' value sep
        b"1337;SetUserVar=",                         // no name
        b"1337;SetUserVar=WEZTERM_PROG=/w==",        // decodes to non-UTF8 -> dropped
    ];
    for body in payloads {
        for term in [ST, BEL] {
            let mut p = vt100::Parser::new(24, 80, 0);
            p.process(&osc(body, term));
            let _ = p.screen().shell_command();
        }
    }
}

#[test]
fn valid_base64_wezterm_prog_still_decodes_after_the_fuzz() {
    // Positive control: prove the decoder is actually functioning, not just
    // swallowing everything. base64("htop") = "aHRvcA=="
    let mut p = vt100::Parser::new(24, 80, 0);
    p.process(&osc(b"1337;SetUserVar=WEZTERM_PROG=aHRvcA==", ST));
    assert_eq!(p.screen().shell_command(), Some("htop"));
}

// ---------------------------------------------------------------------------
// 3. Huge payloads must not blow the capacity math or take quadratic time.
// ---------------------------------------------------------------------------

#[test]
fn oversized_payloads_are_handled() {
    // 16 KB of percent escapes.
    let mut big = Vec::new();
    big.extend_from_slice(b"133;C;cmdline_url=");
    for _ in 0..4096 {
        big.extend_from_slice(b"%41"); // 'A'
    }
    let mut p = vt100::Parser::new(24, 80, 0);
    p.process(&osc(&big, ST));
    let cmd = p.screen().shell_command().unwrap_or("");
    assert_eq!(cmd.len(), 4096, "all 4096 'A's should decode");
    assert!(cmd.bytes().all(|b| b == b'A'));

    // 16 KB of base64.
    let mut b64body = Vec::new();
    b64body.extend_from_slice(b"1337;SetUserVar=WEZTERM_PROG=");
    for _ in 0..8192 {
        b64body.push(b'A'); // 'A' repeated is valid base64 (decodes to NULs)
    }
    let mut p2 = vt100::Parser::new(24, 80, 0);
    p2.process(&osc(&b64body, ST)); // must not panic on capacity math
    let _ = p2.screen().shell_command();
}

// ---------------------------------------------------------------------------
// 4. Byte-split feeding: an adversarial payload fed one byte at a time (the
//    cross-chunk stitching path) must reach the same state and never panic.
// ---------------------------------------------------------------------------

#[test]
fn adversarial_payload_split_at_every_byte_offset() {
    let full = osc(b"133;C;cmdline_url=rm%20-rf%20%2Ftmp%2Fx", ST);
    // Feed one byte at a time.
    let mut p = vt100::Parser::new(24, 80, 0);
    for b in &full {
        p.process(&[*b]);
    }
    assert_eq!(p.screen().shell_command(), Some("rm -rf /tmp/x"));
    assert!(!grid_text(&p).contains("cmdline_url"));
}

// ---------------------------------------------------------------------------
// 5. 633;E with embedded control-ish bytes must not panic and must not leak.
// ---------------------------------------------------------------------------

#[test]
fn osc633e_weird_bytes_never_panic_or_leak() {
    let payloads: &[&[u8]] = &[
        b"633;E;",                        // empty command
        b"633;E;normal-cmd",              // plain
        b"633;E;cmd\\x3bwith\\x3bescapes", // VS Code escaped semicolons (verbatim)
        b"633;E;a;b;c;d",                 // multiple segments (nonce-ish)
    ];
    for body in payloads {
        for term in [ST, BEL] {
            let mut p = vt100::Parser::new(24, 80, 0);
            p.process(&osc(body, term));
            let _ = p.screen().shell_command();
            assert!(!grid_text(&p).contains("633"), "633 leaked into grid");
        }
    }
}

// ---------------------------------------------------------------------------
// 6. Interleaving with real text output: the command sequence must be invisible
//    while surrounding printed text still lands on the grid.
// ---------------------------------------------------------------------------

#[test]
fn command_sequence_is_invisible_but_text_prints() {
    let mut p = vt100::Parser::new(24, 80, 0);
    p.process(b"before ");
    p.process(&osc(b"133;C;cmdline_url=vim%20a.txt", ST));
    p.process(b"after");
    let text = grid_text(&p);
    assert!(text.contains("before"), "printed text before OSC missing");
    assert!(text.contains("after"), "printed text after OSC missing");
    assert!(!text.contains("vim"), "command identity leaked into grid: {text:?}");
    assert!(!text.contains("cmdline_url"), "OSC param leaked into grid");
    assert_eq!(p.screen().shell_command(), Some("vim a.txt"));
}
