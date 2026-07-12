use super::*;
use std::io::BufReader;

#[test]
fn bounded_read_normal_line() {
    let mut r = BufReader::new(&b"AUTH abcd\nrest"[..]);
    let line = read_line_bounded(&mut r, 1024, None).unwrap().unwrap();
    assert_eq!(line, "AUTH abcd\n");
}

#[test]
fn bounded_read_rejects_oversize() {
    let big = vec![b'x'; 2048];
    let mut r = BufReader::new(&big[..]);
    assert!(read_line_bounded(&mut r, 1024, None).unwrap().is_none());
}

#[test]
fn bounded_read_eof_empty() {
    let mut r = BufReader::new(&b""[..]);
    let line = read_line_bounded(&mut r, 1024, None).unwrap().unwrap();
    assert_eq!(line, "");
}

#[test]
fn bounded_read_expired_deadline_returns_none() {
    let mut r = BufReader::new(&b"AUTH abcd\n"[..]);
    let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
    assert!(read_line_bounded(&mut r, 1024, Some(past)).unwrap().is_none());
}
