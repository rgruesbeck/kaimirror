//! Reading the phone's input mode off its own status bar.
//!
//! Multi-tap needs to know whether the keypad is typing `abc` or `ABC`, and
//! that is the one thing about the keypad the host cannot compute.  Counting
//! `#` presses does not work, for three reasons all measured on hardware:
//! the first press of a burst only wakes the mode banner and changes nothing,
//! the ring differs between builds, and **the IME moves on its own** -- type
//! `. ` and it returns to sentence mode without being asked.  A model that
//! is right when it is written is wrong two characters later.
//!
//! So the mode is read rather than modelled.  KaiOS prints it in the status
//! bar -- `Ab`, `ab`, `AB`, `12`, and a symbols glyph -- and this is a
//! mirroring tool, so the frame is already there for the taking.
//!
//! Nothing here recognises glyphs in general.  Calibration walks the ring
//! once, keeps the pixels of each mode's indicator, and finds `abc` among
//! them by the one property that separates it from every other mode: its
//! first character is x-height, so it starts *lower* than the second.  Every
//! later reading is a nearest-match against those saved crops, which is why
//! it does not care what font or theme the build uses.

use std::thread;
use std::time::Duration;

use crate::stream;

/// Which mode the keypad is in, as far as typing cares.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Case {
    Lower,
    Upper,
    /// Sentence, digits, symbols: everything that is neither, and that a
    /// plan has to press past rather than type in.
    Other,
}

/// The indicator's box on a 240x320 panel, scaled for anything else.  It sits
/// right of the SIM badge, and this is the region that was checked by eye
/// against real captures rather than guessed from the layout.
const REF_W: u32 = 240;
/// Wide enough for the two mode characters and no wider.
///
/// It used to run to x=52, which reached far enough right to catch a third
/// glyph run off the signal indicator -- live content that changes on its own
/// and drowns the difference this is trying to measure.  Measured off real
/// dumps: the two mode characters end by x=43.
const BOX: (u32, u32, u32, u32) = (26, 0, 43, 20);
/// The luminance range a crop must span before it is believed to hold text,
/// out of the ~250 this rough luminance produces.  Status-bar text against
/// its bar spans most of that, either way round; a flat block spans a
/// handful of levels of dither noise.
const MIN_CONTRAST: i32 = 60;

pub struct Indicator {
    /// One saved crop per ring position, in the order `#` walks them.
    ring: Vec<Crop>,
    lower: usize,
    upper: usize,
}

#[derive(Clone)]
pub struct Crop {
    w: usize,
    h: usize,
    ink: Vec<u8>, // 1 where a glyph is, whichever way round the bar is drawn
}

/// Pull the indicator out of an RGB565 frame as an ink mask.
///
/// Thresholded against the crop's own range rather than a fixed level, and
/// with the polarity chosen rather than assumed: KaiOS draws a dark status
/// bar over some screens and a light one over others -- the Note editor's is
/// cream with black text -- so "text is the bright class" is wrong half the
/// time, and taking it on faith made a light bar read as solid ink and get
/// thrown away as unreadable.  What holds either way is that the glyphs are
/// the *minority* of the pixels, which is what decides it here.
pub fn crop(frame: &[u8], w: u32, h: u32) -> Option<Crop> {
    let scale = w as f64 / REF_W as f64;
    let (x0, y0, x1, y1) = BOX;
    let (x0, y0) = ((x0 as f64 * scale) as u32, (y0 as f64 * scale) as u32);
    let (x1, y1) = ((x1 as f64 * scale) as u32, (y1 as f64 * scale) as u32);
    if x1 > w || y1 > h || frame.len() < (w * h * 2) as usize {
        return None;
    }
    let mut lum = Vec::with_capacity(((x1 - x0) * (y1 - y0)) as usize);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * w + x) * 2) as usize;
            let px = u16::from_le_bytes([frame[i], frame[i + 1]]);
            // RGB565 -> a rough luminance; the exact weights do not matter,
            // only that text and background stay far apart.
            let (r, g, b) = ((px >> 11) & 0x1f, (px >> 5) & 0x3f, px & 0x1f);
            lum.push(((r as u32 * 8 * 30 + g as u32 * 4 * 59 + b as u32 * 8 * 11) / 100) as u8);
        }
    }
    let (&low, &high) = (lum.iter().min()?, lum.iter().max()?);
    // A crop with no contrast in it has no text in it: the mode banner that
    // covers the bar after a switch is a flat block, and so is a panel
    // mid-repaint.  Say so, rather than thresholding the noise -- a
    // relative cut turns a flat region into a 50% dither that looks for all
    // the world like a glyph, and the ring then records it as a mode.
    if (high as i32 - low as i32) < MIN_CONTRAST {
        return None;
    }
    // Midway between the darkest and brightest pixel in the crop, so the
    // split follows the bar's own contrast instead of an absolute level.
    let cut = ((low as u32 + high as u32) / 2) as u8;
    let bright: Vec<u8> = lum.iter().map(|&v| u8::from(v > cut)).collect();
    let lit = bright.iter().filter(|&&v| v == 1).count();
    let inverted = lit * 2 > bright.len();
    Some(Crop {
        w: (x1 - x0) as usize,
        h: (y1 - y0) as usize,
        ink: bright.into_iter().map(|v| v ^ u8::from(inverted)).collect(),
    })
}

impl Crop {
    fn at(&self, x: usize, y: usize) -> bool {
        self.ink[y * self.w + x] == 1
    }

    /// Column runs of ink, which for this text are the glyphs.  A one-column
    /// gap does not split a glyph: at this size the letters have thin waists.
    fn glyphs(&self) -> Vec<(usize, usize)> {
        let inked: Vec<bool> = (0..self.w)
            .map(|x| (0..self.h).any(|y| self.at(x, y)))
            .collect();
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut start = None;
        for x in 0..self.w {
            match (inked[x], start) {
                (true, None) => start = Some(x),
                (false, Some(s)) => {
                    if x + 1 < self.w && inked[x + 1] {
                        continue; // a one-column gap inside one glyph
                    }
                    runs.push((s, x));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(s) = start {
            runs.push((s, self.w));
        }
        runs
    }

    fn top_of(&self, (x0, x1): (usize, usize)) -> Option<usize> {
        (0..self.h).find(|&y| (x0..x1).any(|x| self.at(x, y)))
    }

    /// Is this the lowercase indicator?
    ///
    /// `ab` is the only mode whose first character is x-height; in `Ab`, `AB`,
    /// `12` and the symbols glyph the first character reaches as high as the
    /// second.  So the test is whether glyph one starts clearly below glyph
    /// two, which needs no idea of what the glyphs actually are.
    fn looks_lowercase(&self) -> bool {
        let runs = self.glyphs();
        if runs.len() < 2 {
            return false;
        }
        match (self.top_of(runs[0]), self.top_of(runs[1])) {
            (Some(first), Some(second)) => first >= second + 2,
            _ => false,
        }
    }

    /// How different two crops are, as a fraction of their pixels.  Used to
    /// match a reading against the calibrated ring, and to notice when the
    /// ring has come back round to where it started.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for y in 0..self.h {
            for x in 0..self.w {
                out.push(if self.at(x, y) { '#' } else { '.' });
            }
            out.push('\n');
        }
        out
    }

    fn distance(&self, other: &Crop) -> f64 {
        if self.w != other.w || self.h != other.h {
            return 1.0;
        }
        let differing = self
            .ink
            .iter()
            .zip(&other.ink)
            .filter(|(a, b)| a != b)
            .count();
        differing as f64 / self.ink.len() as f64
    }
}

/// How close two crops must be to be called the same mode.
///
/// The gap between two modes is *not* uniformly large, which is what 0.04
/// got wrong.  `ab` and `Ab` differ in their first glyph and are tens of
/// percent apart, but `Ab` and `AB` differ only in the second -- a `b` against
/// a `B`, some fourteen pixels of a 340-pixel crop, about 4%.  At 0.04 the
/// walk read `AB` as a return to `Ab`, decided the ring had closed after two
/// entries, and handed back sentence case as "uppercase": `HELLO` would have
/// typed as `Hello`, silently.  Re-reads of one mode differ by a pixel or two
/// of antialiasing, well under 1%, so there is room to be much stricter.
const SAME: f64 = 0.02;

/// What to say when `#` moves nothing.  Worth being specific: the cause is
/// nearly always that the focus is not in a text field, and the symptom
/// otherwise looks like the tool being broken.
pub const NO_FIELD: &str =
    "the input mode did not change -- is a text field focused?  (`#` goes to \
     the dialer when nothing is listening for text)";

impl Indicator {
    /// Walk the `#` ring once, keeping what each mode looks like.
    ///
    /// The walk stops when the indicator returns to a crop already seen,
    /// which is also what makes the wake press harmless: a press that changes
    /// nothing simply is not a new ring entry.
    pub fn calibrate(display: u32, press: &mut dyn FnMut()) -> Result<Self, String> {
        let mut ring: Vec<Crop> = Vec::new();
        // How many presses in a row have changed nothing.  One is expected --
        // the first `#` of a burst sometimes only raises the mode banner --
        // but two means nothing is listening for text, and that is the one
        // thing this must not press on through: `#` on a field that does not
        // want it goes to the *dialer*, which is how a calibration walk once
        // typed `2###########` into a phone number with CALL one key away.
        let mut stale = 0;
        for _ in 0..12 {
            let seen = read_crop(display).ok_or("could not read the screen")?;
            match ring.iter().position(|c| c.distance(&seen) < SAME) {
                Some(0) if ring.len() >= 2 => break, // the ring is closed
                Some(_) => {
                    stale += 1;
                    if stale > 1 {
                        return Err(NO_FIELD.into());
                    }
                }
                None => {
                    stale = 0;
                    ring.push(seen);
                }
            }
            press();
        }
        if ring.len() < 2 {
            return Err(NO_FIELD.into());
        }
        let Some(lower) = ring.iter().position(Crop::looks_lowercase) else {
            let this = Indicator { ring, lower: 0, upper: 0 };
            this.dump();
            return Err("no lowercase mode found in the input-mode cycle".into());
        };
        // `#` walks abc -> ABC on every build seen, so upper is the next
        // entry.  Anchoring on lowercase rather than on a fixed ring order is
        // what makes this survive a build that inserts a mode elsewhere.
        let upper = (lower + 1) % ring.len();
        Ok(Indicator { ring, lower, upper })
    }

    pub fn read(&self, display: u32) -> Option<Case> {
        let seen = read_crop(display)?;
        let (at, distance) = self
            .ring
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.distance(&seen)))
            .min_by(|a, b| a.1.total_cmp(&b.1))?;
        if distance > 0.25 {
            return None; // not a mode we know; better to say so than to guess
        }
        Some(match at {
            i if i == self.lower => Case::Lower,
            i if i == self.upper => Case::Upper,
            _ => Case::Other,
        })
    }

    pub fn modes(&self) -> usize {
        self.ring.len()
    }

    /// Print what each ring entry looks like, for when a build's status bar
    /// does not sit where this expects and someone has to see why.
    pub fn dump(&self) {
        println!("-- ring of {} modes; lower=ring[{}] upper=ring[{}]",
                 self.ring.len(), self.lower, self.upper);
        for (i, c) in self.ring.iter().enumerate() {
            let runs = c.glyphs();
            let tops: Vec<String> = runs.iter()
                .map(|&r| format!("{:?}", c.top_of(r)))
                .collect();
            println!("-- ring[{i}] glyphs={} tops=[{}] lower={}",
                     runs.len(), tops.join(", "), c.looks_lowercase());
            print!("{}", c.render());
        }
    }
}

/// Read the indicator, waiting out anything covering it.
///
/// Switching modes raises a banner across the top of the screen, and a frame
/// grabbed under it is a solid block rather than a status bar -- it was being
/// recorded as a distinct "mode" of its own.  A blank crop means the panel is
/// mid-repaint or blanked.  Both are worth one more look rather than an
/// answer.
fn read_crop(display: u32) -> Option<Crop> {
    // Light the panel *here*, at the grab, rather than once before a walk.
    //
    // A walk is a press-and-look loop that can run the better part of a
    // minute -- calibration plus up to a full lap of the ring, each round a
    // frame grab and a wait for the mode banner to clear -- and the phone's
    // screen timeout fires inside it.  A blanked panel does not stop
    // compositing, it freezes, so from that moment every read returns the
    // same mode and the walk presses `#` until it gives up.  The signature is
    // unmistakable once seen: `saw Lower -> Lower -> Lower -> Lower`.
    //
    // `ensure_lit` is one adb round trip against a grab that costs a second,
    // and it only presses power when the backlight is actually at zero.
    crate::adb::ensure_lit();
    for _ in 0..6 {
        if let Some(found) = stream::grab(display).and_then(|(f, w, h)| crop(&f, w, h)) {
            let ink = found.ink.iter().filter(|&&v| v == 1).count() as f64
                / found.ink.len() as f64;
            if (0.02..0.5).contains(&ink) {
                return Some(found);
            }
        }
        thread::sleep(Duration::from_millis(400));
    }
    None
}
