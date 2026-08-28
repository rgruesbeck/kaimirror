//! Feeding frames to ffplay or ffmpeg.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use crate::adb;
use crate::stream::FrameStream;

/// Set from the SIGINT handler.  Ctrl-C has to finalize a recording rather
/// than kill the process, or the file is left unplayable -- the default
/// disposition would take the container's trailer with it.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    // Only async-signal-safe work here: set a flag, let the loop notice.
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn catch_interrupt() {
    unsafe { libc::signal(libc::SIGINT, on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t) };
}

/// Ask the frame loop to stop, as Ctrl-C does.  `view --control` uses this
/// for `q`, so quitting from the keyboard tears down the same way.
pub fn request_stop() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// ffmpeg/ffplay input args for whatever the device is sending.
pub fn sink_input(stream: &FrameStream, fps: u32) -> Vec<String> {
    let s = |x: &str| x.to_string();
    if stream.png {
        return vec![s("-f"), s("image2pipe"), s("-vcodec"), s("png"),
                    s("-framerate"), fps.to_string(), s("-i"), s("-")];
    }
    let (w, h) = stream.geom.expect("geometry known once a frame has arrived");
    vec![s("-f"), s("rawvideo"), s("-pixel_format"), s("rgb565le"),
         s("-video_size"), format!("{w}x{h}"), s("-framerate"), fps.to_string(),
         s("-i"), s("-")]
}

pub fn have(tool: &str) -> bool {
    // stdin must be closed here too: a probe that inherits the terminal can
    // block on it, and this one runs before the control loop is even started.
    Command::new(tool)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

pub fn pipe_to<F>(stream: &mut FrameStream, make_cmd: F, label: &str)
where
    F: Fn(&FrameStream) -> Vec<String>,
{
    // The first frame carries the geometry, so the sink cannot be built until
    // it has arrived.
    let Some(first) = stream.next_frame() else {
        stream.close();
        adb::fail("no frames from device (is the panel awake?)");
    };

    let cmd = make_cmd(stream);
    let mut sink: Child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| adb::fail(&format!("could not start {}: {e}", cmd[0])));
    let mut stdin = sink.stdin.take().expect("piped");

    // A raw frame (153KB) is larger than the pipe buffer, so writing it
    // inline blocks until ffmpeg drains -- and while blocked we are not
    // draining adb, which stalls the device pump.  Hand the writes to a
    // thread so reading and feeding overlap.
    let (tx, rx) = sync_channel::<Vec<u8>>(16);
    let writer = thread::spawn(move || {
        while let Ok(frame) = rx.recv() {
            if stdin.write_all(&frame).and_then(|_| stdin.flush()).is_err() {
                return; // sink went away
            }
        }
        drop(stdin); // EOF, so the sink finalizes rather than waiting
    });

    let (t0, mut last, mut n) = (Instant::now(), Instant::now(), 0u64);
    let mut frame = Some(first);
    while let Some(f) = frame.take() {
        if interrupted() {
            break;
        }
        match tx.try_send(f) {
            Err(TrySendError::Disconnected(_)) => break,
            Err(TrySendError::Full(f)) => {
                // Back-pressure: hold the frame rather than dropping it.
                if tx.send(f).is_err() {
                    break;
                }
            }
            Ok(()) => {}
        }
        n += 1;
        if last.elapsed() >= Duration::from_secs(5) {
            let el = t0.elapsed().as_secs_f64();
            eprintln!("[{label}] {n} frames, {:.1} fps", n as f64 / el);
            last = Instant::now();
        }
        frame = stream.next_frame();
    }

    // Teardown order matters: stop the device first, then let the writer
    // drain what is already queued so a recording keeps its last frames,
    // then let the sink see EOF and finalize.
    stream.close();
    drop(tx);
    let _ = writer.join();
    let _ = sink.wait();

    let el = t0.elapsed().as_secs_f64();
    if n > 0 {
        eprintln!("[{label}] {n} frames in {el:.1}s ({:.1} fps)", n as f64 / el);
    }
}
