//! Device-side frame pump for kaimirror.
//!
//! usage: kaipump [delay_us] [display] [raw|png] [guard 0|1] [limit]
//!               [ipc|exec] [max_fps] [staging_tag]
//!        kaipump probe
//!        kaipump key NODE CODE [hold_ms]
//!        kaipump control
//!
//! `control` reads one line per key: "NODE CODE [hold_ms]", a phone key on
//! its own /dev/input node.  Text goes on the same nodes -- the host types by
//! multi-tap on the keypad, so from here a letter is indistinguishable from
//! any other keypress.
//!
//! This replaces kaimirror_device.sh, whose cost was dominated by process
//! startup rather than by capture.  Every external command on this hardware
//! costs ~34ms of fork, exec and dynamic linking, and the shell loop spent
//! three or four of them per frame (`gfxdebugger`, `stat`, `mv`, `cat`) for
//! ~136ms of overhead against ~41ms of actual capture.
//!
//! Two backends remain, kept side by side so the difference stays measurable:
//!
//! exec  spawn `gfxdebugger` per frame, as the shell did, but issue it before
//!       shipping the previous frame so its startup overlaps the transfer.
//! ipc   speak /dev/socket/gfxdebugger-ipc directly and skip the process
//!       entirely.  This is what `gfxdebugger` itself does, and the request
//!       is one 40-48 byte write -- see screencap_request.
//!
//! b2g picks the output format from the file extension and offers only two:
//! a path ending in .png gets a PNG, anything else gets an uncompressed
//! RGB565 dump -- a 16-byte header (w, h, format, planes; uint32 LE) then
//! w*h*2 bytes.  Frames go out back to back with no length prefix; both
//! formats are self-describing enough for the host to split them for free.

use std::fs::{self, File};
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const IPC_SOCKET: &str = "/dev/socket/gfxdebugger-ipc";
const STATS_PATH: &str = "/data/local/tmp/kaipump.stats";
/// A key press costs ~140ms through `adb shell sendevent`, nearly all of it
/// the round trip and the two process spawns -- the same startup cost that
/// dominated frame capture.  Writing the events straight to the input node
/// from a process that is already running removes both.
const KEY_HOLD_MS: u64 = 50;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;

/// One `struct input_event` as the 4.9 kernel expects it on 32-bit ARM:
/// a 2x32-bit timeval, then type, code and value -- 16 bytes.  The timestamp
/// is left zero because the kernel stamps injected events itself, which is
/// what `sendevent` relies on too.
fn input_event(kind: u16, code: u16, value: i32) -> [u8; 16] {
    let mut e = [0u8; 16];
    e[8..10].copy_from_slice(&kind.to_le_bytes());
    e[10..12].copy_from_slice(&code.to_le_bytes());
    e[12..16].copy_from_slice(&value.to_le_bytes());
    e
}

/// Take key presses on stdin for as long as the host sends them, one
/// "node code [hold_ms]" line each.
///
/// This runs as its own `adb shell` invocation rather than riding the frame
/// connection, because `adb exec-out` -- which the frame stream needs, since
/// `adb shell` mangles binary output -- does not forward stdin at all.  One
/// long-lived shell still amortises away the ~104ms per-key round trip that
/// made `adb shell sendevent` feel sluggish.
fn control() {
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { return };
        let mut f = line.split_whitespace();
        let Some(head) = f.next() else { continue };

        let Some(code) = f.next() else { continue };
        let (Ok(node), Ok(code)) = (head.parse(), code.parse()) else { continue };
        let hold = f.next().and_then(|s| s.parse().ok()).unwrap_or(KEY_HOLD_MS);
        // A key that fails to inject must not take the channel down with it.
        let _ = send_key(node, code, Duration::from_millis(hold));
    }
}

/// Press and release one key, with the same hold the host used to get from
/// `sendevent; sleep; sendevent`.
fn send_key(node: u32, code: u16, hold: Duration) -> io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .open(format!("/dev/input/event{node}"))?;
    for value in [1, 0] {
        f.write_all(&input_event(EV_KEY, code, value))?;
        f.write_all(&input_event(EV_SYN, 0, 0))?;
        if value == 1 {
            thread::sleep(hold);
        }
    }
    Ok(())
}

const PNG_IEND: [u8; 8] = [0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82];

/// The screencap request, as observed on the wire from `gfxdebugger` itself:
/// four uint32 LE words then the destination path, NUL-terminated and
/// zero-padded to a 4-byte boundary.  The first three words are constant
/// across every invocation; only the display id and the path vary.
///
/// Confirmed by varying the inputs rather than by reading the binary: `-d 1`
/// flips word 4 alone, and a 28-character path yields 48 bytes against 40 for
/// a 21-character one, which is exactly the padding rule below.
fn screencap_request(display: u32, path: &Path) -> Vec<u8> {
    let mut m = Vec::with_capacity(64);
    for word in [4u32, 1, 2, display] {
        m.extend_from_slice(&word.to_le_bytes());
    }
    m.extend_from_slice(path.as_os_str().as_encoded_bytes());
    m.push(0);
    while m.len() % 4 != 0 {
        m.push(0);
    }
    m
}

#[derive(PartialEq, Clone, Copy)]
enum Backend {
    Ipc,
    Exec,
}

struct Pump {
    w: PathBuf,     // b2g writes here
    r: PathBuf,     // renamed aside, then shipped
    png: bool,
    display: u32,
    delay: Duration,
    guard: bool,
    backend: Backend,
    request: Vec<u8>,
    /// Without the per-frame fork the pump outruns everything it feeds:
    /// b2g's CPU goes from 8% to 74% and the staging file churns 22 MB/s of
    /// flash, all to re-capture a screen that is not changing that fast.  So
    /// the request rate is capped rather than left to find its own ceiling.
    min_interval: Option<Duration>,
    last_capture: Option<Instant>,
    /// Raw frames are a fixed size, but which size depends on the display
    /// (240x320 versus the 128x128 cover), so it is learned from frame one.
    size: Option<u64>,
    buf: Vec<u8>,
    /// Shipping the same staged frame twice would inflate the frame rate
    /// while showing a stale screen, and on a static UI it is invisible in
    /// the output -- so the loop counts instead of trusting itself.
    captures: u64,
    shipped: u64,
    restage_failed: u64,
}

impl Pump {
    fn new(delay_us: u64, display: u32, png: bool, guard: bool, backend: Backend,
           max_fps: u32, tag: &str) -> Self {
        let ext = if png { "png" } else { "raw" };
        // Staged frames keep the .kaimirror_ prefix so the host's existing
        // REMOTE_STAGING glob still sweeps them, and carry the pid so two
        // pumps never share a path.  They do overlap now: reading the IME
        // mode off the screen means grabbing a frame while a mirror is
        // already streaming, and on one staging path each would truncate the
        // other's frames.
        let w = PathBuf::from(format!("/data/local/tmp/.kaimirror_w{tag}.{ext}"));
        let r = PathBuf::from(format!("/data/local/tmp/.kaimirror_r{tag}.{ext}"));
        let request = screencap_request(display, &w);
        Pump {
            w, r, png, display, guard, backend, request,
            min_interval: (max_fps > 0).then(|| Duration::from_micros(1_000_000 / max_fps as u64)),
            last_capture: None,
            delay: Duration::from_micros(delay_us),
            size: None,
            buf: Vec::with_capacity(256 * 1024),
            captures: 0,
            shipped: 0,
            restage_failed: 0,
        }
    }

    fn cleanup(&self) {
        let _ = fs::remove_file(&self.w);
        let _ = fs::remove_file(&self.r);
    }

    /// Ask b2g for a frame.  Returns without waiting for the pixels: b2g
    /// writes the file asynchronously either way, so completeness is the
    /// guard's job, not this function's.
    ///
    /// gfxdebugger sends its request and closes without ever reading a reply,
    /// so there is nothing to wait for on the socket and no status to check.
    fn capture(&mut self) -> io::Result<()> {
        if let (Some(gap), Some(last)) = (self.min_interval, self.last_capture) {
            let since = last.elapsed();
            if since < gap {
                thread::sleep(gap - since);
            }
        }
        self.last_capture = Some(Instant::now());
        self.captures += 1;
        match self.backend {
            Backend::Ipc => {
                let mut sock = UnixStream::connect(IPC_SOCKET)?;
                sock.write_all(&self.request)
            }
            Backend::Exec => {
                // gfxdebugger's own output must go nowhere near our stdout --
                // that carries the frame stream.
                Command::new("gfxdebugger")
                    .args(["-c", "screencap", "-d", &self.display.to_string(), "-p"])
                    .arg(&self.w)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?
                    .wait()
                    .map(|_| ())
            }
        }
    }

    /// b2g's write lands asynchronously, and measured back to back most
    /// frames are still incomplete when the request returns.  So wait for
    /// completeness before handing the frame over.
    fn settle(&mut self) {
        if self.png {
            while !self.png_complete() {
                thread::sleep(self.delay);
            }
        } else {
            self.settle_raw();
        }
    }

    fn png_complete(&self) -> bool {
        // PNG grows in chunks, so completeness is the IEND chunk landing.
        let Ok(mut f) = File::open(&self.w) else { return false };
        if f.seek(SeekFrom::End(-8)).is_err() {
            return false;
        }
        let mut tail = [0u8; 8];
        f.read_exact(&mut tail).is_ok() && tail == PNG_IEND
    }

    fn size_of(&self) -> u64 {
        fs::metadata(&self.w).map(|m| m.len()).unwrap_or(0)
    }

    fn settle_raw(&mut self) {
        match self.size {
            // Known geometry: completeness is one size compare.
            Some(want) => {
                while self.size_of() != want {
                    thread::sleep(self.delay);
                }
            }
            // First frame: wait for the size to stop changing, then keep it.
            None => {
                let (mut prev, mut cur) = (u64::MAX, 0u64);
                while cur == 0 || cur != prev {
                    prev = cur;
                    thread::sleep(self.delay);
                    cur = self.size_of();
                }
                self.size = Some(cur);
            }
        }
    }

    /// Rename the settled frame aside so a late writer can never truncate the
    /// copy being sent.
    fn stage(&mut self) {
        if fs::rename(&self.w, &self.r).is_err() {
            // The frame we were about to stage is not there, so the next ship
            // resends the previous one.  Counted, because that is a duplicate.
            self.restage_failed += 1;
        }
    }

    /// Ship the staged frame.  Returns false once the host has hung up.
    fn ship(&mut self, out: &mut impl Write) -> bool {
        self.buf.clear();
        match File::open(&self.r).and_then(|mut f| f.read_to_end(&mut self.buf)) {
            Ok(0) | Err(_) => return true, // nothing staged yet; not fatal
            Ok(_) => {}
        }
        self.shipped += 1;
        // Rust ignores SIGPIPE, so a hung-up host surfaces as an error here
        // rather than a signal -- which is what lets this exit cleanly
        // instead of wedging as an orphan re-creating staging files forever.
        out.write_all(&self.buf).and_then(|_| out.flush()).is_ok()
    }

    fn run(&mut self, limit: Option<u64>) -> io::Result<()> {
        let stdout = io::stdout();
        let mut out = stdout.lock();

        // Prime the pipeline: the first frame is always guarded so the stream
        // starts aligned, whatever the guard setting asks for later.
        self.capture()?;
        self.settle();
        self.stage();

        loop {
            if limit.is_some_and(|n| self.shipped >= n) {
                return Ok(());
            }
            // Issue the next capture *before* shipping the current frame, so
            // b2g's write overlaps the transfer instead of serialising
            // behind it.
            self.capture()?;

            if !self.ship(&mut out) {
                return Ok(());
            }
            if self.guard {
                self.settle();
            }
            self.stage();
        }
    }

    fn write_stats(&self) {
        // Never write diagnostics to stdout *or* stderr: `adb exec-out`
        // merges the two, so anything printed lands in the middle of the
        // frame stream.  Stats go to a file the host can read separately.
        let _ = fs::write(
            STATS_PATH,
            format!(
                "captures={} shipped={} restage_failed={}\n",
                self.captures, self.shipped, self.restage_failed
            ),
        );
    }
}

fn probe() {
    println!("kaipump probe");
    println!("arch           {}", std::env::consts::ARCH);
    println!("pointer        {} bits", usize::BITS);
    let kernel = fs::read_to_string("/proc/version").unwrap_or_default();
    println!("kernel         {}", kernel.lines().next().unwrap_or("?").trim());
    match fs::metadata(IPC_SOCKET) {
        Ok(m) => {
            println!("ipc socket     present, is_socket={}", m.file_type().is_socket());
            match UnixStream::connect(IPC_SOCKET) {
                Ok(_) => println!("ipc connect    ok"),
                Err(e) => println!("ipc connect    FAILED: {e}"),
            }
        }
        Err(e) => println!("ipc socket     FAILED: {e}"),
    }
    // Typing goes on the keypad itself, so what matters is that this
    // domain can open the node the keys are written to -- SELinux is
    // Enforcing here, and a refusal there is the difference between "typing
    // is slow" and "typing does nothing".
    for node in [0u32, 1, 2] {
        let path = format!("/dev/input/event{node}");
        match fs::OpenOptions::new().write(true).open(&path) {
            Ok(_) => println!("input event{node}   ok, writable"),
            Err(e) => println!("input event{node}   FAILED: {e}"),
        }
    }
    let found: Vec<&str> = ["/system/bin/gfxdebugger", "/system/xbin/gfxdebugger"]
        .into_iter()
        .filter(|p| Path::new(p).exists())
        .collect();
    println!("gfxdebugger    {}", if found.is_empty() { "<not found>".into() } else { found.join(" ") });
    let req = screencap_request(0, Path::new("/data/local/tmp/.kaimirror_w.raw"));
    println!("request        {} bytes: {}", req.len(),
             req.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(""));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("probe") {
        probe();
        return;
    }
    if args.first().map(String::as_str) == Some("control") {
        control();
        return;
    }
    if args.first().map(String::as_str) == Some("key") {
        let node: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
        let code: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        let hold = Duration::from_millis(
            args.get(3).and_then(|s| s.parse().ok()).unwrap_or(KEY_HOLD_MS));
        if let Err(e) = send_key(node, code, hold) {
            let _ = fs::write(STATS_PATH, format!("key error={e}\n"));
            std::process::exit(1);
        }
        return;
    }

    let arg = |i: usize, d: &str| args.get(i).cloned().unwrap_or_else(|| d.to_string());
    let delay_us: u64 = arg(0, "5000").parse().unwrap_or(5000);
    let display: u32 = arg(1, "0").parse().unwrap_or(0);
    let png = arg(2, "raw") == "png";
    let guard = arg(3, "1") != "0";
    // Optional frame limit, for benchmarking a finite run.  0 means
    // unlimited, so the host can pass a placeholder and still reach the
    // positional arguments after it.
    let limit: Option<u64> = args
        .get(4)
        .and_then(|s| s.parse().ok())
        .filter(|n: &u64| *n > 0);
    let backend = if arg(5, "ipc") == "exec" { Backend::Exec } else { Backend::Ipc };
    // 0 uncaps.  30 is a deliberate default: the panel does not update faster,
    // and uncapped costs most of b2g's CPU for frames nobody sees.
    let max_fps: u32 = arg(6, "30").parse().unwrap_or(30);
    // The staging tag keeps two pumps off each other's files -- reading the
    // IME mode grabs a frame while a mirror is streaming.  A caller that
    // intends to kill this pump rather than let it finish passes its own tag,
    // so it can clear the files afterwards; everything else uses its pid.
    let tag = args.get(7).cloned().unwrap_or_else(|| std::process::id().to_string());

    let mut pump = Pump::new(delay_us, display, png, guard, backend, max_fps, &tag);
    pump.cleanup();
    let result = pump.run(limit);
    pump.cleanup();
    pump.write_stats();

    if let Err(e) = result {
        let _ = fs::write(STATS_PATH, format!("error={e}\n"));
        std::process::exit(1);
    }
}
