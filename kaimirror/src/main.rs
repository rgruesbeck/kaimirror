//! kaimirror -- scrcpy-style screen mirroring for KaiOS 3.x devices.
//!
//! KaiOS runs Gecko (b2g) directly on the Android HAL with no SurfaceFlinger
//! and no Android framework, so scrcpy itself cannot work: its server needs
//! android.jar, SurfaceControl and a MediaCodec encoder bound to a virtual
//! display.  /system/bin/screencap and screenrecord ship on the device but
//! hang forever waiting for a SurfaceFlinger binder that never registers.
//!
//! What KaiOS does provide is b2g's own /dev/socket/gfxdebugger-ipc, which
//! dumps the composited display to a file.  /system/bin/gfxdebugger is a thin
//! client for that socket; the device-side pump (kaipump) speaks it directly,
//! which is what makes the frame rate a device limit rather than a
//! process-startup one.  Input injection writes input_event structs straight
//! to the raw /dev/input nodes.
//!
//! Requires: adb root on a userdebug build, and ffplay for the viewer.

mod adb;
mod cli;
mod control;
mod imemode;
mod keys;
mod multitap;
mod sink;
mod stream;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use stream::FrameStream;

fn main() {
    let a = cli::parse(std::env::args().skip(1).collect());
    let Some(cmd) = a.cmd.clone() else {
        print!("{}", cli::help(None));
        return;
    };

    // `key --list` has nothing to say to a device.
    if cmd == "key" && a.list {
        println!("{}", keys::names().join("\n"));
        return;
    }

    adb::ensure_root();
    // Capture needs the pump installed up front; key and wake reach it too,
    // but install lazily so a keypress does not pay for a staleness check it
    // almost never needs.
    if matches!(cmd.as_str(), "view" | "record" | "shot") {
        adb::push_pump();
        if !a.no_wake {
            adb::wake();
        }
    }
    // `type` and `mode` need the pump too, and both read the input-mode
    // indicator off a captured frame -- which needs a lit panel, since a
    // blanked one still composites and grabs as a valid frame with no status
    // bar in it.  `ensure_lit`, not `wake`: power *toggles*, so tapping it
    // blindly is as likely to blank a lit screen as to light a dark one.
    if matches!(cmd.as_str(), "type" | "mode") {
        adb::ensure_pump();
        adb::ensure_lit();
    }

    match cmd.as_str() {
        "view" => cmd_view(&a),
        "record" => cmd_record(&a),
        "shot" => cmd_shot(&a),
        "key" => cmd_key(&a),
        "type" => cmd_type(&a),
        "mode" => cmd_mode(&a),
        "wake" => cmd_wake(),
        _ => unreachable!("cli::parse only accepts known commands"),
    }
}

fn cmd_view(a: &cli::Args) {
    if !sink::have("ffplay") {
        adb::fail("ffplay not found (install ffmpeg)");
    }
    sink::catch_interrupt();
    let mut stream = FrameStream::new(a.display, a.format == "png", a.fps);

    // ffplay keeps its own keystrokes, so control is driven from the terminal
    // rather than from the mirror window.
    let mut control = None;
    if a.control && control::is_tty() {
        if let Some(mut ctl) = control::Controller::new(a.display) {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            eprintln!("{}", control::NAV_HELP);
            let handle = thread::spawn(move || {
                control::control_loop(&mut ctl, flag, control::Mode::Nav);
                ctl.close();
            });
            control = Some((stop, handle));
        }
    } else if a.control {
        eprintln!("note: --control needs a terminal; ignoring");
    }

    let (scale, fps) = (a.scale, a.fps);
    sink::pipe_to(&mut stream, |s| {
        let mut cmd: Vec<String> = ["ffplay", "-hide_banner", "-loglevel", "error"]
            .iter().map(|x| x.to_string()).collect();
        cmd.extend(sink::sink_input(s, fps));
        if (scale - 1.0).abs() > f64::EPSILON {
            cmd.push("-vf".into());
            cmd.push(format!("scale=iw*{scale}:ih*{scale}:flags=neighbor"));
        }
        cmd.extend(["-window_title", "kaimirror", "-autoexit"].iter().map(|x| x.to_string()));
        cmd
    }, "view");

    if let Some((stop, handle)) = control {
        stop.store(true, Ordering::SeqCst);
        let _ = handle.join();
    }
}

fn cmd_record(a: &cli::Args) {
    if !sink::have("ffmpeg") {
        adb::fail("ffmpeg not found");
    }
    let Some(output) = a.positionals.first().cloned() else {
        adb::fail("record needs an output path, e.g. `kaimirror record out.mp4`");
    };
    sink::catch_interrupt();
    let mut stream = FrameStream::new(a.display, a.format == "png", a.fps);
    let fps = a.fps;
    let out = output.clone();
    sink::pipe_to(&mut stream, |s| {
        let mut cmd: Vec<String> = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y"]
            .iter().map(|x| x.to_string()).collect();
        cmd.extend(sink::sink_input(s, fps));
        cmd.extend(["-pix_fmt", "yuv420p"].iter().map(|x| x.to_string()));
        cmd.push(out.clone());
        cmd
    }, "record");
    eprintln!("wrote {output}");
}

fn cmd_shot(a: &cli::Args) {
    // Always PNG: a single frame is not worth optimising, and PNG carries
    // more colour precision than the RGB565 raw dump.
    let output = a.positionals.first().cloned().unwrap_or_else(|| "kaishot.png".into());
    let mut stream = FrameStream::new(a.display, true, cli::DEFAULT_FPS);
    let frame = stream.next_frame();
    stream.close();
    match frame {
        Some(png) => {
            if let Err(e) = std::fs::write(&output, &png) {
                adb::fail(&format!("could not write {output}: {e}"));
            }
            println!("wrote {output} ({} bytes)", png.len());
        }
        None => adb::fail("no frames from device (is the panel awake?)"),
    }
}

/// Calibrate the input-mode reader and report what it sees.
///
/// Its own command because the answer is worth seeing directly: if this
/// cannot name the mode, multi-tap cannot control case, and the reason
/// belongs on screen rather than buried in a typing run.
fn cmd_mode(a: &cli::Args) {
    let Some(mut ctl) = control::Controller::new(a.display) else {
        adb::fail("could not open the control channel");
    };
    println!("reading the phone's input mode (focus a text field first)...");
    // Deliberately the same calls typing makes, rather than a private
    // shortcut: this command exists to prove that path works.  Both cases,
    // because typing needs both and a cycle can be walkable to one and not
    // the other.
    let mut failed = None;
    for want in [imemode::Case::Lower, imemode::Case::Upper] {
        match ctl.reach_case(want) {
            Ok(()) => println!("input mode: reached {want:?}"),
            Err(e) => {
                println!("input mode: {want:?} FAILED: {e}");
                failed = Some(e);
                // No second attempt after a failure that says nothing is
                // listening: the retry's own keypresses are the damage.
                break;
            }
        }
    }
    ctl.dump_modes();
    ctl.close();
    if let Some(e) = failed {
        adb::fail(&e);
    }
}

/// Type text on the device, without a mirror window.
///
/// The same channel `view --control` types over, opened for one line: it is
/// how typing gets verified on a phone, and how a script fills a field.
fn cmd_type(a: &cli::Args) {
    // Nothing to type means type *live*: the terminal becomes the phone's
    // keyboard until Ctrl-C.  That is the way this is actually used -- a
    // fixed string is for scripts -- so it is what bare `type` does.
    if a.positionals.is_empty() {
        if !control::is_tty() {
            adb::fail("give the text to type, e.g. `kaimirror type \"hello world\"`, \
                       or run it from a terminal to type live");
        }
        return type_live(a);
    }
    // Separate arguments are joined with spaces, so an unquoted `type hello
    // world` does the obvious thing rather than typing "helloworld".
    let text = a.positionals.join(" ");
    let Some(mut ctl) = control::Controller::new(a.display) else {
        adb::fail("could not open the control channel (is adb still connected?)");
    };
    let typed = ctl.type_text(&text);
    // Waiting, not killing: the device is still draining the queued keys,
    // and killing the pump would discard whatever is left of them.
    ctl.finish();
    let skipped = match typed {
        Ok(skipped) => skipped,
        Err(e) => adb::fail(&e),
    };
    if !skipped.is_empty() {
        let list: Vec<String> = skipped.iter().map(|c| format!("{c:?}")).collect();
        eprintln!("note: no way to type {} on this device -- skipped", list.join(", "));
    }
}

/// Forward terminal keystrokes to the phone until Ctrl-C, with no mirror
/// window.
///
/// The same loop `view --control` runs, started in text mode rather than nav:
/// what is wanted here is a keyboard, and the phone's screen is the phone's
/// own to look at.
fn type_live(a: &cli::Args) {
    let Some(mut ctl) = control::Controller::new(a.display) else {
        adb::fail("could not open the control channel (is adb still connected?)");
    };
    eprintln!("{}", control::TYPE_HELP);
    control::control_loop(&mut ctl, Arc::new(AtomicBool::new(false)), control::Mode::Text);
    // Waiting rather than killing: the last character may still be queued on
    // the device.
    ctl.finish();
}

fn cmd_key(a: &cli::Args) {
    if a.positionals.is_empty() {
        adb::fail("give at least one key name, or --list to see them all");
    }
    for name in &a.positionals {
        adb::send_key(name);
    }
}

fn cmd_wake() {
    let b = adb::wake();
    let hint = if b == 0 { "  (still off -- press power manually)" } else { "" };
    println!("backlight={b}{hint}");
}
