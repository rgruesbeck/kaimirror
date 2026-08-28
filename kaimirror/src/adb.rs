//! Talking to the device.

use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use crate::keys;

pub const REMOTE_PUMP: &str = "/data/local/tmp/kaipump";
pub const REMOTE_STAGING: &str =
    "/data/local/tmp/.kaimirror_* /data/local/tmp/kaipump.stats";

/// Reaping a pump means killing its whole process group: an orphan can wedge
/// blocked writing to a half-open adb socket that never returns EPIPE, so
/// signalling it alone leaves the write pending.  The bracketed letter keeps
/// the pattern from matching the shell running the sweep -- whose own command
/// line contains this text -- and the pgid compare keeps that shell from
/// killing its own session.
pub const PUMP_SWEEP: &str = concat!(
    "me=$(cut -d\" \" -f5 /proc/$$/stat); ",
    "for p in $(pgrep -f \"kaipum[p]\"); do ",
    "g=$(cut -d\" \" -f5 /proc/$p/stat 2>/dev/null); ",
    "[ -n \"$g\" ] && [ \"$g\" != \"$me\" ] && kill -9 -\"$g\" 2>/dev/null; ",
    "done; true"
);

/// Never let adb inherit our stdin.  Nothing here needs it, and an adb client
/// sharing the terminal competes for the keystrokes --control wants to read.
pub fn adb(args: &[&str]) -> Output {
    Command::new("adb")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| fail(&format!("could not run adb: {e}")))
}

pub fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Explain why we could not talk to the device, then exit.  Only reached on
/// the failure path, so the happy path costs no extra round trip -- and
/// `adb wait-for-device` is never used, because it blocks forever.
fn diagnose_no_device() -> ! {
    let out = stdout_of(&adb(&["devices"]));
    let attached = out
        .lines()
        .skip(1)
        .any(|l| l.split_whitespace().nth(1) == Some("device"));
    if !attached {
        fail("no device (check the cable; `adb devices` should list it)");
    }
    fail("could not get adb root (needs a userdebug/ro.debuggable build)");
}

pub fn ensure_root() {
    // One round trip in the common case: `adb shell id` answers both "is a
    // device there" and "are we root", and only its failure needs a second
    // call to say which went wrong.
    if stdout_of(&adb(&["shell", "id"])).contains("uid=0") {
        return;
    }
    adb(&["root"]);
    thread::sleep(Duration::from_secs(2));
    adb(&["wait-for-device"]);
    if !stdout_of(&adb(&["shell", "id"])).contains("uid=0") {
        diagnose_no_device();
    }
}

/// The device pump, built into this binary so a released kaimirror is one
/// file.  Empty only when built without one -- see build.rs.
static PUMP: &[u8] = include_bytes!(env!("KAIPUMP_BIN"));

/// Install the device-side pump, skipping the push when the device already
/// has this build.
///
/// Every command needs the pump, `key` included, and pushing 350KB each time
/// tripled what a keypress cost.  Comparing sizes is one cheap round trip and
/// a rebuild effectively always changes the size.
pub fn push_pump() {
    if PUMP.is_empty() {
        fail("this kaimirror was built without a device pump (run ./build.sh)");
    }
    let remote = stdout_of(&adb(&["shell", &format!("stat -c %s {REMOTE_PUMP} 2>/dev/null")]));
    if remote.trim() != PUMP.len().to_string() {
        // adb push wants a path, so the embedded bytes only touch disk on the
        // rare push -- not on the staleness check that answers "current"
        // almost every time.
        let staged = std::env::temp_dir().join(format!("kaipump.{}", std::process::id()));
        if let Err(e) = std::fs::write(&staged, PUMP) {
            fail(&format!("could not stage the device pump at {}: {e}", staged.display()));
        }
        adb(&["push", &staged.to_string_lossy(), REMOTE_PUMP]);
        adb(&["shell", "chmod", "755", REMOTE_PUMP]);
        let _ = std::fs::remove_file(&staged);
    }
    // A pump orphaned by a host crash would fight this session over the same
    // staging paths, so clear any before starting.
    adb(&["shell", PUMP_SWEEP]);
}

/// Inject one key press over its own adb round trip.  Fine for `kaimirror
/// key`; `view --control` uses the persistent channel instead, because this
/// costs ~140ms of round trip and process spawns.
pub fn send_key(name: &str) {
    let Some((node, code)) = keys::lookup(name) else {
        fail(&format!(
            "unknown key {name:?}; known: {}",
            keys::names().join(", ")
        ));
    };
    let (node, code) = (node.to_string(), code.to_string());
    let out = adb(&["shell", REMOTE_PUMP, "key", &node, &code]);
    // Assume the pump is already installed -- it usually is -- and only pay
    // for a push when that turns out to be wrong.  Checking first would cost
    // a round trip on every keypress to answer "yes" almost every time.
    if !out.status.success() || String::from_utf8_lossy(&out.stderr).contains("not found") {
        push_pump();
        adb(&["shell", REMOTE_PUMP, "key", &node, &code]);
    }
}

/// Tap power so the panel is lit -- a blanked screen still composites, and
/// captures as a valid solid-black frame that looks like success.
pub fn wake() -> i32 {
    send_key("POWER");
    thread::sleep(Duration::from_secs(1));
    let out = stdout_of(&adb(&["shell", "cat /sys/class/leds/lcd-backlight/brightness"]));
    out.trim().parse().unwrap_or(0)
}
