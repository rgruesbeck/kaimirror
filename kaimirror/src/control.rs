//! Driving the device from the terminal while the mirror runs.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::{adb, keys};

/// A persistent key-injection channel to the device.
///
/// Each `adb shell` costs ~140ms, nearly all of it the round trip and the
/// process spawns rather than the injection itself.  One long-lived shell
/// amortises that away, leaving a pipe write.
///
/// This cannot ride the frame connection: the stream needs `adb exec-out`,
/// because `adb shell` mangles binary output, and `exec-out` does not forward
/// stdin at all.  So control gets its own connection.
pub struct Controller {
    proc: Child,
}

impl Controller {
    pub fn new() -> Option<Self> {
        Command::new("adb")
            .args(["shell", adb::REMOTE_PUMP, "control"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
            .map(|proc| Controller { proc })
    }

    pub fn send(&mut self, name: &str) {
        let Some((node, code)) = keys::lookup(name) else { return };
        if let Some(stdin) = self.proc.stdin.as_mut() {
            // A dead channel must not take the mirror down with it.
            let _ = write!(stdin, "{node} {code}\n");
            let _ = stdin.flush();
        }
    }

    pub fn close(&mut self) {
        drop(self.proc.stdin.take());
        let _ = self.proc.kill();
        let _ = self.proc.wait();
    }
}

/// Terminal state, restored on drop.
///
/// Leaving a terminal in cbreak is worse than any failure this can hit, so
/// restoration rides on Drop rather than on reaching the end of a function.
struct RawTerminal {
    fd: libc::c_int,
    saved: libc::termios,
}

impl RawTerminal {
    fn new(fd: libc::c_int) -> Option<Self> {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            // cbreak, not full raw: keep ISIG so Ctrl-C still signals.
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawTerminal { fd, saved })
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSADRAIN, &self.saved) };
    }
}

pub fn is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// Wait up to `ms` for stdin to have something, so a stop set elsewhere is
/// noticed without needing one more keystroke to wake this up.
fn readable(fd: libc::c_int, ms: libc::c_int) -> bool {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
}

/// Read one byte straight from the descriptor.
///
/// Deliberately not `io::Stdin`, which buffers: a 1-byte read there pulls the
/// whole `ESC [ B` of an arrow key into its private buffer, and the poll that
/// follows then sees an empty descriptor and concludes there is no sequence
/// to finish.  Arrow keys silently stop working.
fn read_byte(fd: libc::c_int) -> Option<u8> {
    let mut b = 0u8;
    let n = unsafe { libc::read(fd, &mut b as *mut u8 as *mut libc::c_void, 1) };
    (n == 1).then_some(b)
}

pub fn control_loop(ctl: &mut Controller, stop: Arc<AtomicBool>) {
    let fd = libc::STDIN_FILENO;
    let Some(_restore) = RawTerminal::new(fd) else { return };

    while !stop.load(Ordering::SeqCst) {
        if !readable(fd, 200) {
            continue;
        }
        let Some(first) = read_byte(fd) else { return };
        let mut seq = String::from(first as char);
        if first == 0x1b {
            // Escape sequence: arrow keys arrive as ESC [ A..D.
            for _ in 0..2 {
                if !readable(fd, 50) {
                    break;
                }
                match read_byte(fd) {
                    Some(b) => seq.push(b as char),
                    None => break,
                }
            }
        }
        if seq == "q" || seq == "\x03" {
            // Quit the whole mirror, not just this loop -- the same path
            // Ctrl-C takes, so a recording still finalizes.
            stop.store(true, Ordering::SeqCst);
            crate::sink::request_stop();
            return;
        }
        if let Some(name) = keys::from_keystroke(&seq) {
            ctl.send(name);
            // Echo what was forwarded.  Without it there is no way to tell a
            // key that never arrived -- typed into the mirror window, which
            // keeps its own keystrokes -- from one that arrived and moved
            // nothing on screen.
            eprintln!("  -> {name}");
        }
    }
}
