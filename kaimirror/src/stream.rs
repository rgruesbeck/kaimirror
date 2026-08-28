//! Reading the frame stream the device pump produces.
//!
//! Frames arrive back to back with no length prefix -- framing them
//! device-side would cost work per frame for something the host can do for
//! free.  Both formats are self-describing enough to split here, and both
//! parsers resynchronise rather than dying if the stream is damaged:
//!
//! raw  16-byte header (w, h, format, planes) then w*h*2 bytes of RGB565;
//!      the header doubles as the sync marker and reports the geometry, so
//!      the cover display's 128x128 needs no special casing.
//! png  walk the chunk headers to the IEND chunk.

use std::io::Read;
use std::process::{Child, ChildStdout, Command, Stdio};

use crate::adb;

const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
const MAX_CHUNK: u32 = 1 << 24; // anything larger is garbage, not a frame
const RAW_HDR: usize = 16; // w, h, format, planes -- uint32 LE each
const POLL_DELAY_US: u32 = 5000;

pub struct FrameStream {
    pub png: bool,
    /// (w, h), learned from the first raw frame header.
    pub geom: Option<(u32, u32)>,
    proc: Child,
    out: ChildStdout,
    buf: Vec<u8>,
    pos: usize, // chunk-walk offset within the frame being parsed
    eof: bool,
}

impl FrameStream {
    pub fn new(display: u32, png: bool, fps: u32) -> Self {
        let fmt = if png { "png" } else { "raw" };
        // limit=0 streams without end; the cap is what keeps b2g off the CPU,
        // since uncapped the pump outruns the panel several times over.
        let mut proc = Command::new("adb")
            .args(["exec-out", adb::REMOTE_PUMP])
            .args([
                &POLL_DELAY_US.to_string(), &display.to_string(), fmt,
                "1", "0", "ipc", &fps.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| adb::fail(&format!("could not start adb: {e}")));
        let out = proc.stdout.take().expect("piped");
        FrameStream { png, geom: None, proc, out, buf: Vec::new(), pos: 8, eof: false }
    }

    /// One read, returning as soon as any bytes are available rather than
    /// waiting for a full buffer, which would add a frame of latency.
    fn fill(&mut self) -> bool {
        if self.eof {
            return false;
        }
        let mut chunk = [0u8; 65536];
        match self.out.read(&mut chunk) {
            Ok(0) | Err(_) => {
                self.eof = true;
                false
            }
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
        }
    }

    /// Read a raw frame header at `at`, or None if it is not a plausible one.
    fn raw_geom(&self, at: usize) -> Option<(u32, u32)> {
        let end = at.checked_add(RAW_HDR)?;
        if end > self.buf.len() {
            return None;
        }
        let word = |i: usize| {
            u32::from_le_bytes(self.buf[at + i * 4..at + i * 4 + 4].try_into().unwrap())
        };
        let (w, h, fmt, planes) = (word(0), word(1), word(2), word(3));
        ((1..=1024).contains(&w) && (1..=1024).contains(&h) && fmt < 64
            && (1..=4).contains(&planes))
            .then_some((w, h))
    }

    fn next_raw(&mut self) -> Option<Vec<u8>> {
        loop {
            let Some((w, h)) = self.raw_geom(0) else {
                if self.buf.len() < RAW_HDR {
                    if !self.fill() {
                        return None;
                    }
                    continue;
                }
                // Desync: hunt forward for the next plausible header rather
                // than giving up on the stream.
                let found = (1..=self.buf.len() - RAW_HDR).find(|&j| self.raw_geom(j).is_some());
                match found {
                    Some(j) => {
                        self.buf.drain(..j);
                    }
                    None => {
                        let keep = self.buf.len() - (RAW_HDR - 1);
                        self.buf.drain(..keep);
                        if !self.fill() {
                            return None;
                        }
                    }
                }
                continue;
            };
            let need = RAW_HDR + (w as usize) * (h as usize) * 2;
            if self.buf.len() < need {
                if !self.fill() {
                    return None;
                }
                continue;
            }
            self.geom = Some((w, h));
            let frame: Vec<u8> = self.buf.drain(..need).skip(RAW_HDR).collect();
            return Some(frame);
        }
    }

    fn next_png(&mut self) -> Option<Vec<u8>> {
        loop {
            if self.buf.len() < 8 {
                if !self.fill() {
                    return None;
                }
                continue;
            }
            if !self.buf.starts_with(PNG_SIG) {
                match find(&self.buf[1..], PNG_SIG) {
                    Some(j) => {
                        self.buf.drain(..j + 1);
                    }
                    None => {
                        let keep = self.buf.len() - (PNG_SIG.len() - 1);
                        self.buf.drain(..keep);
                        if !self.fill() {
                            return None;
                        }
                    }
                }
                self.pos = 8;
                continue;
            }
            if self.pos + 8 > self.buf.len() {
                if !self.fill() {
                    return None;
                }
                continue;
            }
            let length =
                u32::from_be_bytes(self.buf[self.pos..self.pos + 4].try_into().unwrap());
            let ctype = &self.buf[self.pos + 4..self.pos + 8];
            if length > MAX_CHUNK || !ctype.iter().all(|c| c.is_ascii_alphabetic()) {
                self.buf.drain(..1); // not a real header; resync
                self.pos = 8;
                continue;
            }
            let is_iend = ctype == b"IEND";
            let end = self.pos + 8 + length as usize + 4;
            if end > self.buf.len() {
                if !self.fill() {
                    return None;
                }
                continue;
            }
            if is_iend {
                let png: Vec<u8> = self.buf.drain(..end).collect();
                self.pos = 8;
                return Some(png);
            }
            self.pos = end;
        }
    }

    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        if self.png { self.next_png() } else { self.next_raw() }
    }

    pub fn close(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
        // Killing the local adb client does not reap the remote process, so
        // signal it, give any write b2g still owes us time to land, then
        // sweep.  Left alone the pump loops forever, re-creating the staging
        // files and fighting the next session over the same paths.
        adb::adb(&["shell", &format!("{}; sleep 0.3; rm -f {}", adb::PUMP_SWEEP, adb::REMOTE_STAGING)]);
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
