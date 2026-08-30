//! Driving the device from the terminal while the mirror runs.

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::imemode::{self, Case, Indicator};
use crate::multitap::{MultiTap, Step};
use crate::{adb, keys};

/// How long the mode banner sits over the status bar after a `#`.
///
/// A frame grabbed under it has no indicator in it at all, so this is paid
/// before every read.  It was 900ms, which was less than the banner and
/// therefore paid *twice*: the read then found a blank bar and retried, and
/// a single mode switch took tens of seconds of frame grabs.  1.8s is the
/// measured banner; the margin here is cheaper than one wasted grab.
const BANNER_MS: u64 = 1900;

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
    /// Which display to read the input-mode indicator from.
    display: u32,
    /// Calibrated on first use, because it costs a walk around the phone's
    /// mode cycle and a session that never types should not pay for it.
    indicator: Option<Indicator>,
    /// Where the keypad's tap cycle currently stands.  One per session: the
    /// plan for a character depends on every character typed before it.
    tap: MultiTap,
    /// Set once `#` has been shown to go nowhere.
    ///
    /// Without this every subsequent character tries the walk again, and each
    /// attempt is a `2` and a couple of `#` into whatever *does* have the
    /// focus -- which, when nothing is listening for text, is the dialer.  A
    /// line of text becomes a phone number that way.  One probe per session
    /// is the budget; navigating clears it, because that is when the focus
    /// can have moved somewhere that listens.
    no_field: bool,
}

impl Controller {
    pub fn new(display: u32) -> Option<Self> {
        Command::new("adb")
            .args(["shell", adb::REMOTE_PUMP, "control"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            // Not null: the pump reports a key it could not inject on stderr,
            // and that message is the only thing distinguishing "this device
            // is not letting us in" from "typing did nothing".
            .stderr(Stdio::inherit())
            .spawn()
            .ok()
            .map(|proc| Controller {
                proc, display, indicator: None, tap: MultiTap::new(), no_field: false,
            })
    }

    pub fn send(&mut self, name: &str) {
        let Some((node, code)) = keys::lookup(name) else { return };
        self.line(&format!("{node} {code}"));
    }

    /// Press the key that cycles the phone's input mode.
    ///
    /// Public because reaching a mode is a loop between the channel and the
    /// screen -- press, look, press again -- and the looking lives elsewhere.
    pub fn press_mode_key(&mut self) {
        self.send("POUND");
        thread::sleep(Duration::from_millis(BANNER_MS));
    }

    fn line(&mut self, text: &str) {
        if let Some(stdin) = self.proc.stdin.as_mut() {
            // A dead channel must not take the mirror down with it.
            let _ = write!(stdin, "{text}\n");
            let _ = stdin.flush();
        }
    }

    pub fn close(&mut self) {
        drop(self.proc.stdin.take());
        let _ = self.proc.kill();
        let _ = self.proc.wait();
    }

    /// Close the channel and let the device drain it.
    ///
    /// `close` kills the pump, which is right for an interactive session
    /// ending on `q` but would discard whatever is still queued -- and a
    /// one-shot `type` has its entire text in that queue.  Here EOF on stdin
    /// ends the pump's read loop on its own.
    pub fn finish(&mut self) {
        drop(self.proc.stdin.take());
        let _ = self.proc.wait();
    }

    /// Type one character.
    ///
    /// `Ok(false)` means the keypad has no way to reach that character, which
    /// is worth reporting but not worth stopping for.  `Err` means the
    /// channel lost control of the phone's input mode, which is fatal: going
    /// on from there types the rest of the line in whatever case the phone
    /// feels like, on top of whatever the failure already left behind.
    pub fn type_char(&mut self, ch: char) -> Result<bool, String> {
        // Plan first, act second: planning borrows the keypad model and
        // running the plan needs the channel, and they cannot overlap.
        let Some(steps) = self.tap.plan(ch) else { return Ok(false) };
        self.run(steps)?;
        Ok(true)
    }

    /// Delete the character before the caret.  The keypad has no
    /// KEY_BACKSPACE at all -- DEL (code 48) is what deletes text inside a
    /// field, and it only means that once the character it would edit has
    /// committed.
    pub fn backspace(&mut self) {
        self.settle_typing();
        self.send("DEL");
    }

    /// Walk a multi-tap plan.  The waits are the plan: they are what keeps
    /// two characters on one key from merging into one.
    fn run(&mut self, steps: Vec<Step>) -> Result<(), String> {
        for step in steps {
            match step {
                Step::Key(name) => self.send(name),
                Step::Wait(d) => thread::sleep(d),
                Step::Mode(case) => self.reach_case(case)?,
            }
        }
        Ok(())
    }

    /// Get the keypad into `want`, by pressing `#` and looking at the phone
    /// after each press.
    ///
    /// Looking is the whole point.  Press counts cannot be predicted -- the
    /// first press of a burst sometimes only wakes the mode banner, the ring
    /// differs between builds, and the IME switches itself back to sentence
    /// mode after a sentence ends.  Reading the indicator makes all three
    /// irrelevant: press, look, stop when it says what we wanted.
    pub fn reach_case(&mut self, want: Case) -> Result<(), String> {
        if self.no_field {
            return Err(imemode::NO_FIELD.into());
        }
        // Keeping the panel lit belongs at the frame grab, not here: the walk
        // below outlasts the screen timeout by itself, so lighting it once up
        // front does not survive the loop.  See `imemode::read_crop`.
        //
        // Look before touching anything.  The field is very often already in
        // the mode being asked for -- a run of lowercase resumed after a
        // sentence reset asks again without anything having moved -- and then
        // there is nothing to press and, more to the point, nothing to clean
        // up afterwards.  One frame grab buys that, against a walk that costs
        // several plus a banner wait each -- so it pays for itself the moment
        // it is right, which for a run of one case is most of the time.
        if let Some(indicator) = self.indicator.as_ref() {
            if indicator.read(self.display) == Some(want) {
                return Ok(());
            }
        }
        // No throwaway character, and no delete afterwards.  There used to be
        // both: `#` was believed to reach the IME only while the field held a
        // character, so a switch typed a letter to happen inside and deleted
        // it again.  Tested directly on a T435SP, pressing `#` by hand into an
        // empty focused Note and reading the indicator after each press:
        // `Ab -> ab -> AB -> 12 -> symbols -> Ab`, five modes, wrapping, field
        // empty throughout and the dialer nowhere in sight.  The belief came
        // from runs where *no field was focused at all* -- which does go to the
        // dialer -- and an empty field was blamed for it.
        //
        // Dropping the pair is not just a saving of two keypresses.  The
        // `DEL` was unconditional, so wherever the throwaway had already gone
        // it deleted the user's own text instead, and on an empty field it is
        // not a delete at all -- the Note editor reads it as "go back" and
        // closes.  That is what emptied an SMS draft and lost the focus in the
        // middle of nearly every typing run.
        let outcome = self.walk_to(want);
        if outcome.as_ref().is_err_and(|e| e == imemode::NO_FIELD) {
            self.no_field = true;
        }
        outcome
    }

    /// The user moved around the phone, so a field that was not listening a
    /// moment ago might be now.  Lets the next character try the walk again.
    pub fn note_navigation(&mut self) {
        self.no_field = false;
    }

    fn walk_to(&mut self, want: Case) -> Result<(), String> {
        if self.indicator.is_none() {
            let display = self.display;
            // Calibration needs to press the key too, and it is the same
            // press, so it borrows this channel through a closure.
            let proc = &mut self.proc;
            let mut press = || {
                if let Some(stdin) = proc.stdin.as_mut() {
                    if let Some((node, code)) = keys::lookup("POUND") {
                        let _ = write!(stdin, "{node} {code}\n");
                        let _ = stdin.flush();
                    }
                }
                thread::sleep(Duration::from_millis(BANNER_MS));
            };
            self.indicator = Some(Indicator::calibrate(display, &mut press)?);
        }
        // Taken out of self for the loop: reading borrows the indicator while
        // pressing borrows the channel, and they alternate.
        let indicator = self.indicator.take().expect("just set");
        let outcome = self.press_until(&indicator, want);
        self.indicator = Some(indicator);
        outcome
    }

    /// Print what calibration recorded: one block per ring position, with the
    /// glyph tops it measured and which entries it called lowercase and
    /// uppercase.  This is what `kaimirror mode` shows, and the only way to
    /// see why a build's cycle was read the way it was.
    pub fn dump_modes(&self) {
        match self.indicator.as_ref() {
            Some(i) => i.dump(),
            None => println!("(no input-mode ring calibrated)"),
        }
    }

    fn press_until(&mut self, indicator: &Indicator, want: Case) -> Result<(), String> {
        // What each look actually saw, because the failure below is otherwise
        // unactionable: "could not reach Upper" says nothing about whether the
        // ring is wrong, the reads are wrong, or the presses are not landing.
        let mut seen = Vec::new();
        for _ in 0..=indicator.modes() {
            match indicator.read(self.display) {
                Some(mode) if mode == want => return Ok(()),
                Some(mode) => seen.push(format!("{mode:?}")),
                // No indicator at all means nothing is listening for text --
                // pressing on from here is what fills a dialer with `#`.
                None => return Err("cannot see the phone's input mode; is a text field focused?".into()),
            }
            self.press_mode_key();
        }
        indicator.dump();
        Err(format!(
            "could not reach {want:?} on the phone's input-mode cycle \
             ({} modes; the walk saw {})",
            indicator.modes(),
            seen.join(" -> ")
        ))
    }

    /// Let the last character finish, where a plan needs it to.  A multi-tap
    /// character is still mid-cycle until the timeout passes, and whatever
    /// the user does next would edit it instead of following it.
    pub fn settle_typing(&mut self) {
        let steps = self.tap.settle();
        let _ = self.run(steps);
    }

    /// Type printable text.  Returns the characters the device has no way to
    /// reach, so the caller can say so rather than silently dropping them.
    pub fn type_text(&mut self, text: &str) -> Result<Vec<char>, String> {
        let mut skipped = Vec::new();
        for ch in text.chars() {
            if !self.type_char(ch)? {
                skipped.push(ch);
            }
        }
        self.settle_typing();
        Ok(skipped)
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

/// The settings to put back if a signal kills us mid-keystroke, as a pointer
/// a signal handler is allowed to read.
///
/// Restoring on Drop is not enough here.  A typed character is a burst of taps
/// and waits seconds long, and Ctrl-C lands *inside* it: the default
/// disposition ends the process there, no destructor runs, and the user gets
/// their shell back with echo off -- which is worse than whatever they were
/// interrupting.  So the handler does the one syscall it needs to and then
/// re-raises the signal to do what it would have done.
static SAVED_TERMIOS: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());

extern "C" fn restore_and_reraise(sig: libc::c_int) {
    // Both of these are async-signal-safe: an atomic load and a tcsetattr.
    let saved = SAVED_TERMIOS.swap(std::ptr::null_mut(), Ordering::SeqCst);
    if !saved.is_null() {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, saved) };
    }
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

impl RawTerminal {
    fn new(fd: libc::c_int) -> Option<Self> {
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return None;
            }
            // Leaked on purpose: the handler may read it at any point up to
            // process exit, so it has to outlive every scope here.
            SAVED_TERMIOS.store(Box::into_raw(Box::new(saved)), Ordering::SeqCst);
            for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                libc::signal(sig, restore_and_reraise as extern "C" fn(libc::c_int) as libc::sighandler_t);
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
        // Whichever gets there first wins; the handler takes the pointer, so
        // the two cannot both restore.
        let saved = SAVED_TERMIOS.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !saved.is_null() {
            drop(unsafe { Box::from_raw(saved) });
        }
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

/// Throw away whatever is sitting in the terminal buffer, and say how much
/// there was.
fn flush_input(fd: libc::c_int) -> usize {
    let mut dropped = 0;
    while readable(fd, 0) {
        match read_byte(fd) {
            Some(_) => dropped += 1,
            None => break,
        }
    }
    dropped
}

/// Which of the two devices a keystroke goes to.
///
/// They cannot be one mode: navigation needs `m` to mean MENU and `.` to
/// mean the right soft key, and typing needs them to mean "m" and ".".  So
/// the terminal has a mode, switched with Tab, the way the phone's own
/// keypad switches between navigating a list and typing into a field.
#[derive(PartialEq, Clone, Copy)]
pub enum Mode {
    Nav,
    Text,
}

pub const NAV_HELP: &str =
    "control: TYPE IN THIS TERMINAL -- the mirror window keeps its own keystrokes.\n\
     \x20        arrows/enter navigate, backspace=back, Ins/PgUp=soft keys,\n\
     \x20        Del=call, PgDn=red, m=menu, Esc=quit.  Digits are the\n\
     \x20        keypad keys -- in a text field they multi-tap, so type\n\
     \x20        digits in text mode.\n\
     \x20        TAB switches to text mode, where letters type on the phone.\n\
     \x20        each forwarded key is echoed below.";

/// What a keystroke means, given which mode the terminal is in.
///
/// Split out from the loop because the loop cannot be tested -- it wants a
/// terminal and a device -- while this is where every decision actually is:
/// the same byte is a device key in one mode and a letter in the other.
#[derive(PartialEq, Debug)]
enum Action {
    Quit,
    Toggle,
    /// A named key on the phone's keypad.
    Nav(&'static str),
    Type(char),
    Enter,
    Backspace,
    Ignore,
}

fn dispatch(seq: &str, mode: Mode) -> Action {
    // Esc quits, from either mode, and so does Ctrl-C.  Esc is bare here --
    // a real escape *sequence* has already been read whole by the caller, so
    // this is the key itself and not the start of an arrow.
    //
    // `q` used to quit in nav mode and type in text mode.  That is one key
    // meaning two things depending on a mode you cannot see, and the failure
    // is silent in the direction that matters: you meant to quit and typed a
    // letter into someone's message instead.  It is a letter everywhere now.
    if seq == "\x03" || seq == "\x1b" {
        return Action::Quit;
    }
    // Tab is the only mode switch, and switches both ways.  It types nothing:
    // a tab character is no use in a phone text field.
    if seq == "\t" {
        return Action::Toggle;
    }
    // Arrows are escape sequences, and the same scancodes on both devices, so
    // navigation keeps working in text mode -- moving the caret inside a
    // field rather than the selection around a list.
    if mode == Mode::Text && !seq.starts_with('\x1b') {
        return match seq {
            "\r" | "\n" => Action::Enter,
            "\x7f" | "\x08" => Action::Backspace,
            _ => match seq.chars().next().filter(char::is_ascii) {
                Some(ch) => Action::Type(ch),
                None => Action::Ignore,
            },
        };
    }
    match keys::from_keystroke(seq) {
        Some(name) => Action::Nav(name),
        None => Action::Ignore,
    }
}

const TEXT_HELP: &str = "  [text mode] letters, digits and symbols type on the phone; \
                         tab returns to nav, Esc quits.";

/// Shown by `kaimirror type` with nothing to type: a session that is text
/// mode from the first keystroke, since that is the whole reason to run it.
pub const TYPE_HELP: &str =
    "typing: TYPE IN THIS TERMINAL and the letters land on the phone.  Focus a\n\
     \x20       text field there first, and leave the panel lit.\n\
     \x20       Each character is a burst of keypad taps, so it lands about a\n\
     \x20       second behind you and a change of case costs a few more --\n\
     \x20       keep typing, nothing is dropped.\n\
     \x20       backspace deletes, enter presses OK, Ins/PgUp are the soft\n\
     \x20       keys and arrows move the caret -- all of which keep working\n\
     \x20       while typing.  TAB switches to nav mode, Esc quits.";

pub fn control_loop(ctl: &mut Controller, stop: Arc<AtomicBool>, start: Mode) {
    let fd = libc::STDIN_FILENO;
    let Some(_restore) = RawTerminal::new(fd) else { return };
    let mut mode = start;

    while !stop.load(Ordering::SeqCst) {
        if !readable(fd, 200) {
            continue;
        }
        let Some(first) = read_byte(fd) else { return };
        let mut seq = String::from(first as char);
        if first == 0x1b {
            // A CSI sequence is ESC [ params final, where final is any byte
            // from 0x40 to 0x7e -- so read until one arrives rather than
            // counting bytes.  Counting worked while arrows were the only
            // sequences (ESC [ A is three), but Ins, Del, PgUp and PgDn are
            // four (ESC [ 2 ~), and stopping at three left the `~` behind to
            // be read as a keystroke of its own.
            //
            // The length test is what keeps `[` from ending the sequence it
            // just started: it is 0x5b, squarely inside the final range.
            for _ in 0..8 {
                if !readable(fd, 50) {
                    break;
                }
                match read_byte(fd) {
                    Some(b) => {
                        seq.push(b as char);
                        if seq.len() > 2 && (0x40..=0x7e).contains(&b) {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }

        match dispatch(&seq, mode) {
            Action::Quit => {
                // Quit the whole mirror, not just this loop -- the same path
                // Ctrl-C takes, so a recording still finalizes.
                stop.store(true, Ordering::SeqCst);
                crate::sink::request_stop();
                return;
            }
            Action::Toggle => {
                if mode == Mode::Text {
                    // Nav keys from here on would edit the character still
                    // mid-cycle under multi-tap, so let it land first.
                    ctl.settle_typing();
                    mode = Mode::Nav;
                    eprintln!("  [nav mode]");
                } else {
                    mode = Mode::Text;
                    eprintln!("{TEXT_HELP}");
                }
            }
            // Echo what was forwarded.  Without it there is no way to tell a
            // key that never arrived -- typed into the mirror window, which
            // keeps its own keystrokes -- from one that arrived and moved
            // nothing on screen.
            Action::Nav(name) => {
                ctl.note_navigation();
                ctl.send(name);
                eprintln!("  -> {name}");
            }
            Action::Type(ch) => match ctl.type_char(ch) {
                Ok(true) => eprintln!("  -> {ch:?}"),
                Ok(false) => {}
                Err(e) => {
                    eprintln!("  !! {e}");
                    // Whatever is still in the terminal buffer was typed as
                    // *text*, and a character this could not case-correct is
                    // followed by a queue of them.  Dropping to nav mode here
                    // -- which is what this used to do -- replays that queue
                    // as navigation: `Hi 42.` lost its case and then pressed
                    // 4, 2 and the right soft key on the phone.  So the queue
                    // goes in the bin and the mode does not change.
                    let dropped = flush_input(fd);
                    if dropped > 0 {
                        eprintln!("  !! dropped {dropped} queued keystroke(s)");
                    }
                }
            },
            Action::Enter => {
                let _ = ctl.type_char('\n');
                eprintln!("  -> ENTER");
            }
            Action::Backspace => {
                ctl.backspace();
                eprintln!("  -> BACKSPACE");
            }
            Action::Ignore => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quitting must not depend on which mode you are in, and `q` must not
    /// be a quit key anywhere -- that pairing is what put stray letters into
    /// text fields.
    #[test]
    fn esc_and_ctrl_c_quit_from_either_mode() {
        for mode in [Mode::Nav, Mode::Text] {
            assert_eq!(dispatch("\x1b", mode), Action::Quit);
            assert_eq!(dispatch("\x03", mode), Action::Quit);
        }
        assert_eq!(dispatch("q", Mode::Text), Action::Type('q'));
        assert_eq!(dispatch("q", Mode::Nav), Action::Ignore);
    }

    #[test]
    fn tab_is_the_only_mode_switch() {
        assert_eq!(dispatch("\t", Mode::Nav), Action::Toggle);
        assert_eq!(dispatch("\t", Mode::Text), Action::Toggle);
    }

    /// An arrow is ESC plus more; only a lone ESC is the Esc key, and the
    /// reader gathers a whole sequence before this ever sees it.
    #[test]
    fn an_arrow_is_not_a_quit() {
        for mode in [Mode::Nav, Mode::Text] {
            assert_eq!(dispatch("\x1b[A", mode), Action::Nav("UP"));
        }
    }

    /// The keys that mean two different things depending on the mode: this is
    /// the whole reason there are modes.
    #[test]
    fn the_same_byte_navigates_or_types() {
        for (seq, nav, ch) in [("m", "MENU", 'm'), ("5", "5", '5'),
                               ("*", "STAR", '*'), ("#", "POUND", '#')] {
            assert_eq!(dispatch(seq, Mode::Nav), Action::Nav(nav));
            assert_eq!(dispatch(seq, Mode::Text), Action::Type(ch));
        }
    }

    #[test]
    fn arrows_navigate_in_both_modes() {
        for mode in [Mode::Nav, Mode::Text] {
            assert_eq!(dispatch("\x1b[A", mode), Action::Nav("UP"));
            assert_eq!(dispatch("\x1b[D", mode), Action::Nav("LEFT"));
        }
    }

    /// The whole point of moving the soft keys onto the navigation cluster:
    /// they mean nothing to a text field, so they can keep working while
    /// typing, and the punctuation they vacated becomes typeable.
    #[test]
    fn the_cluster_keys_navigate_in_both_modes() {
        for mode in [Mode::Nav, Mode::Text] {
            assert_eq!(dispatch("\x1b[2~", mode), Action::Nav("SOFT_LEFT"));
            assert_eq!(dispatch("\x1b[5~", mode), Action::Nav("SOFT_RIGHT"));
            assert_eq!(dispatch("\x1b[3~", mode), Action::Nav("CALL"));
        }
    }

    #[test]
    fn the_punctuation_they_left_types_and_no_longer_navigates() {
        for (seq, ch) in [(",", ','), (".", '.'), ("c", 'c')] {
            assert_eq!(dispatch(seq, Mode::Text), Action::Type(ch));
            assert_eq!(dispatch(seq, Mode::Nav), Action::Ignore);
        }
    }

    #[test]
    fn enter_and_backspace_change_meaning_with_the_mode() {
        assert_eq!(dispatch("\r", Mode::Nav), Action::Nav("OK"));
        assert_eq!(dispatch("\r", Mode::Text), Action::Enter);
        assert_eq!(dispatch("\x7f", Mode::Nav), Action::Nav("BACK"));
        assert_eq!(dispatch("\x7f", Mode::Text), Action::Backspace);
    }

    /// Every printable character has to reach the typing path, or text mode
    /// silently swallows it.
    #[test]
    fn text_mode_passes_every_printable_character_through() {
        for ch in ' '..='~' {
            let seq = ch.to_string();
            assert_eq!(dispatch(&seq, Mode::Text), Action::Type(ch), "{ch:?}");
        }
    }

    #[test]
    fn unknown_control_bytes_are_ignored_rather_than_typed() {
        assert_eq!(dispatch("\x04", Mode::Nav), Action::Ignore);
        assert_eq!(dispatch("\x1b[Z", Mode::Nav), Action::Ignore);
    }
}
