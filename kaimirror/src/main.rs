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
mod keys;
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
        adb::push_pump(adb::local_pump());
        if !a.no_wake {
            adb::wake();
        }
    }

    match cmd.as_str() {
        "view" => cmd_view(&a),
        "record" => cmd_record(&a),
        "shot" => cmd_shot(&a),
        "key" => cmd_key(&a),
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
        if let Some(mut ctl) = control::Controller::new() {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            eprintln!(
                "control: TYPE IN THIS TERMINAL -- the mirror window keeps its own keystrokes.\n\
                 \x20        arrows/enter navigate, backspace=back, digits, m=menu, q=quit.\n\
                 \x20        each forwarded key is echoed below."
            );
            let handle = thread::spawn(move || {
                control::control_loop(&mut ctl, flag);
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
